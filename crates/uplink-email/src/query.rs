//! Read-only queries over mailbox state, on a reader connection. Each runs
//! in one read transaction and reports the log position it reflects.

use anyhow::{Context, Result};
use uplink_protocol::{
    mail::{
        Conversation, Entry, EntryId, EntryList, Label, LabelId, ListPosition, ListingProgress,
        MailProvider, Member, MessageId, Sender,
    },
    TxLogId,
};
use uplink_storage::rusqlite::{
    params, Connection, OptionalExtension, Row, Statement, ToSql, Transaction,
};
use uplink_tx_log::as_of;

/// SQL: whether the entry with id `entry` is in Trash or Spam, which, as
/// in Gmail, hides it from every list but theirs, from the other labels'
/// counts, and from search.
fn hidden(entry: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM entry_label hl JOIN label h ON h.id = hl.label
                 WHERE hl.entry = {entry} AND hl.desired
                   AND h.external_id IN ('TRASH', 'SPAM'))"
    )
}

/// A configured mail account.
#[derive(Debug, Clone)]
pub struct ConfiguredAccount {
    pub id: String,
    pub address: String,
    pub provider: MailProvider,
}

pub fn accounts(
    conn: &mut Connection,
    configured: &[ConfiguredAccount],
) -> Result<(TxLogId, Vec<uplink_protocol::mail::Account>)> {
    let tx = conn.transaction()?;
    let as_of = as_of(&tx)?;
    let mut accounts = vec![];
    for account in configured {
        let (synced, total): (bool, Option<u32>) = tx
            .query_row(
                "SELECT synced, listing_total FROM account WHERE id = ?1",
                [&account.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((false, None));
        let listing = match total {
            Some(total) => Some(ListingProgress {
                stored: tx.query_row(
                    "SELECT count(*) FROM mail_entry WHERE account_id = ?1",
                    [&account.id],
                    |r| r.get(0),
                )?,
                total,
            }),
            None => None,
        };
        // In one pass over the account's labelled entries; what Trash and
        // Spam hide, and what is unread, is found once. Conversations are
        // counted, as Gmail does: an entry without a thread is one by
        // itself. A label nothing carries has no counts, so zero.
        let conversation = "coalesce(e.thread_id, '#' || e.id)";
        let labels = tx
            .prepare(&format!(
                "WITH hidden AS (
                   SELECT entry FROM entry_label
                   WHERE desired AND label IN (SELECT id FROM label
                                               WHERE account_id = ?1
                                                 AND external_id IN ('TRASH', 'SPAM'))),
                 unread AS (
                   SELECT entry FROM entry_label
                   WHERE desired AND label = (SELECT id FROM label
                                              WHERE account_id = ?1 AND external_id = 'UNREAD')),
                 counts AS (
                   SELECT el.label,
                     count(DISTINCT {conversation}) AS total,
                     count(DISTINCT CASE WHEN el.entry IN (SELECT entry FROM unread)
                                         THEN {conversation} END) AS unread
                   FROM label l
                   JOIN entry_label el ON el.label = l.id AND el.desired
                   JOIN mail_entry e ON e.id = el.entry
                   WHERE l.account_id = ?1
                     AND (l.external_id IN ('TRASH', 'SPAM')
                          OR el.entry NOT IN (SELECT entry FROM hidden))
                   GROUP BY el.label)
                 SELECT l.id, l.external_id, l.name, l.system, l.folder,
                   coalesce(c.total, 0), coalesce(c.unread, 0)
                 FROM label l LEFT JOIN counts c ON c.label = l.id
                 WHERE l.account_id = ?1
                 ORDER BY NOT l.system, l.name",
            ))?
            .query_map([&account.id], |r| {
                Ok(Label {
                    id: LabelId(r.get(0)?),
                    external_id: r.get(1)?,
                    name: r.get(2)?,
                    system: r.get(3)?,
                    folder: r.get(4)?,
                    total: r.get(5)?,
                    unread: r.get(6)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        accounts.push(uplink_protocol::mail::Account {
            id: account.id.clone(),
            address: account.address.clone(),
            provider: account.provider,
            synced,
            listing,
            labels,
        });
    }
    Ok((as_of, accounts))
}

const ENTRY_COLUMNS: &str = "e.id, e.account_id, e.message, e.thread_id, e.received_at,
                             m.subject, m.from_addr, m.from_name, m.snippet,
                             m.has_attachments";

/// A page of conversations, newest first.
pub struct Page {
    pub as_of: TxLogId,
    pub conversations: Vec<Conversation>,
    pub next: Option<ListPosition>,
}

/// Below this many candidates, a page is found by sorting them all; above,
/// by walking entries newest first (by an index) until the page is full,
/// which many candidates make quick. Either way a page costs about this
/// many rows at most, however large the mailbox.
const SORT_AT_MOST: i64 = 2000;

/// Which entries a list's conversations are made of, as SQL over an entry
/// alias: in a list of Trash or Spam, the entries with its label (bound as
/// `:label`); in any other, those in neither.
#[derive(Clone, Copy)]
enum Scope {
    Labelled,
    Unhidden,
}

impl Scope {
    fn sql(self, x: &str) -> String {
        match self {
            Scope::Labelled => format!(
                "EXISTS (SELECT 1 FROM entry_label sl
                         WHERE sl.entry = {x}.id AND sl.label = :label AND sl.desired)"
            ),
            Scope::Unhidden => format!("NOT {}", hidden(&format!("{x}.id"))),
        }
    }
}

/// How a list finds its conversations: those with an entry in scope that
/// `matches`. A conversation is listed by its newest entry in scope, which
/// need not match itself (a reply in Sent, to a thread in the Inbox).
struct Listing {
    scope: Scope,
    /// SQL: whether entry `x` (in scope) puts its conversation on the list.
    matches: Box<dyn Fn(&str) -> String>,
    /// Candidates, when few: `FROM … WHERE …`, naming the entries `c`;
    /// with `None`, or many, the list walks `walk` instead.
    candidates: Option<(String, String)>,
    /// The index to walk entries newest first by, and the SQL its entries
    /// `e` must also meet (e.g. their account).
    walk: (&'static str, String),
}

/// The list's conversations, newest first, after `after`.
pub fn conversations(
    conn: &mut Connection,
    list: &EntryList,
    after: Option<ListPosition>,
    limit: u32,
) -> Result<Page> {
    let tx = conn.transaction()?;
    let as_of = as_of(&tx)?;
    type Params = Vec<(&'static str, Box<dyn ToSql>)>;
    let has_label = |label: &'static str| {
        move |x: &str| {
            format!(
                "EXISTS (SELECT 1 FROM entry_label ml
                         WHERE ml.entry = {x}.id AND ml.label {label} AND ml.desired)"
            )
        }
    };
    let (params, listing): (Params, Listing) = match list {
        EntryList::Label { label } => {
            let (account, external_id): (String, String) = tx.query_row(
                "SELECT account_id, external_id FROM label WHERE id = ?1",
                [label.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let scope = match external_id.as_str() {
                "TRASH" | "SPAM" => Scope::Labelled,
                _ => Scope::Unhidden,
            };
            (
                vec![
                    (":label", Box::new(label.0)),
                    (":account", Box::new(account)),
                ],
                Listing {
                    scope,
                    matches: Box::new(has_label("= :label")),
                    candidates: Some((
                        "entry_label el JOIN mail_entry c ON c.id = el.entry".into(),
                        "el.label = :label AND el.desired".into(),
                    )),
                    walk: (
                        "mail_entry_by_account_received",
                        "e.account_id = :account".into(),
                    ),
                },
            )
        }
        EntryList::AllMail { account } => (
            vec![(":account", Box::new(account.clone()))],
            Listing {
                scope: Scope::Unhidden,
                matches: Box::new(|_| "1".into()),
                candidates: None,
                walk: (
                    "mail_entry_by_account_received",
                    "e.account_id = :account".into(),
                ),
            },
        ),
        EntryList::Inbox => {
            const INBOXES: &str = "IN (SELECT id FROM label WHERE external_id = 'INBOX')";
            (
                vec![],
                Listing {
                    scope: Scope::Unhidden,
                    matches: Box::new(has_label(INBOXES)),
                    candidates: Some((
                        "entry_label el JOIN mail_entry c ON c.id = el.entry".into(),
                        format!("el.label {INBOXES} AND el.desired"),
                    )),
                    walk: ("mail_entry_by_received", "1".into()),
                },
            )
        }
    };
    let params: Vec<(&str, &dyn ToSql)> = params.iter().map(|(n, v)| (*n, &**v)).collect();
    let conversations = page(&tx, &listing, &params, after, limit)?;
    let next = (conversations.len() == limit as usize).then(|| {
        let last = &conversations
            .last()
            .expect("a full page is not empty")
            .latest;
        ListPosition {
            received_at: last.received_at,
            entry: last.id,
        }
    });
    Ok(Page {
        as_of,
        conversations,
        next,
    })
}

/// The listing's conversations, newest first, after `after`.
fn page(
    tx: &Transaction,
    listing: &Listing,
    params: &[(&str, &dyn ToSql)],
    after: Option<ListPosition>,
    limit: u32,
) -> Result<Vec<Conversation>> {
    let scope = |x: &str| listing.scope.sql(x);
    let candidates = match &listing.candidates {
        Some((from, condition)) => {
            let mut count =
                tx.prepare(&format!("SELECT count(*) FROM {from} WHERE {condition}"))?;
            let count: i64 = count.query_row(&*named(&count, params), |r| r.get(0))?;
            (count <= SORT_AT_MOST).then_some((from, condition))
        }
        None => None,
    };
    let (from, condition) = match candidates {
        // The conversations of the candidates, by their newest entries.
        Some((from, condition)) => (
            format!(
                "(SELECT DISTINCT
                    CASE WHEN c.thread_id IS NULL THEN c.id
                    ELSE (SELECT n.id FROM mail_entry n
                          WHERE n.account_id = c.account_id AND n.thread_id = c.thread_id
                            AND {scope_n}
                          ORDER BY n.received_at DESC, n.id DESC LIMIT 1)
                    END AS id
                  FROM {from} WHERE {condition} AND {scope_c}) newest
                 JOIN mail_entry e ON e.id = newest.id",
                scope_n = scope("n"),
                scope_c = scope("c"),
            ),
            "1".to_owned(),
        ),
        // Entries that are their conversation's newest, and whose
        // conversation has an entry that matches.
        None => {
            let (index, walked) = &listing.walk;
            (
                format!("mail_entry e INDEXED BY {index}"),
                format!(
                    "{walked} AND {scope_e}
                     AND (e.thread_id IS NULL
                          OR NOT EXISTS (SELECT 1 FROM mail_entry n
                                         WHERE n.account_id = e.account_id
                                           AND n.thread_id = e.thread_id
                                           AND (n.received_at, n.id) > (e.received_at, e.id)
                                           AND {scope_n}))
                     AND ({matches_e}
                          OR (e.thread_id IS NOT NULL
                              AND EXISTS (SELECT 1 FROM mail_entry x
                                          WHERE x.account_id = e.account_id
                                            AND x.thread_id = e.thread_id AND x.id <> e.id
                                            AND {matches_x} AND {scope_x})))",
                    scope_e = scope("e"),
                    scope_n = scope("n"),
                    scope_x = scope("x"),
                    matches_e = (listing.matches)("e"),
                    matches_x = (listing.matches)("x"),
                ),
            )
        }
    };
    // Positions compare as (received_at, entry); the first page starts
    // after the greatest possible one.
    let after = after.unwrap_or(ListPosition {
        received_at: i64::MAX,
        entry: EntryId(i64::MAX),
    });
    let mut all: Vec<(&str, &dyn ToSql)> = params.to_vec();
    all.extend([
        (":received_at", &after.received_at as &dyn ToSql),
        (":entry", &after.entry.0),
        (":limit", &limit),
    ]);
    let mut latest = tx.prepare(&format!(
        "SELECT {ENTRY_COLUMNS}
         FROM {from}
         JOIN message m ON m.id = e.message
         WHERE {condition}
           AND (e.received_at, e.id) < (:received_at, :entry)
         ORDER BY e.received_at DESC, e.id DESC
         LIMIT :limit"
    ))?;
    let latest = latest
        .query_map(&*named(&latest, &all), entry_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let latest = with_labels(tx, latest)?;
    conversations_of(tx, listing.scope, params, latest)
}

/// The conversations whose newest entries in scope are `latest`.
fn conversations_of(
    tx: &Transaction,
    scope: Scope,
    params: &[(&str, &dyn ToSql)],
    latest: Vec<Entry>,
) -> Result<Vec<Conversation>> {
    let mut members = tx.prepare(&format!(
        "SELECT e.id, e.message, m.from_name, m.from_addr, m.has_attachments
         FROM mail_entry e JOIN message m ON m.id = e.message
         WHERE e.account_id = :account_id AND e.thread_id = :thread_id AND e.id <> :latest
           AND {}
         ORDER BY e.received_at, e.id",
        scope.sql("e")
    ))?;
    let named = named(&members, params);
    let mut labels =
        tx.prepare("SELECT label FROM entry_label WHERE entry = ?1 AND desired ORDER BY label")?;
    latest
        .into_iter()
        .map(|latest| {
            let mut senders: Vec<Sender> = vec![];
            let mut has_attachments = latest.has_attachments;
            let mut earlier = vec![];
            let mut add_sender = |name: Option<String>, addr: Option<String>| {
                let same = |s: &Sender| match (&addr, &s.addr) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    (None, None) => name == s.name,
                    _ => false,
                };
                if !senders.iter().any(same) {
                    senders.push(Sender { name, addr });
                }
            };
            if let Some(thread_id) = &latest.thread_id {
                let mut all = named.clone();
                all.extend([
                    (":account_id", &latest.account as &dyn ToSql),
                    (":thread_id", thread_id),
                    (":latest", &latest.id.0),
                ]);
                let mut rows = members.query(&*all)?;
                while let Some(r) = rows.next()? {
                    let id = EntryId(r.get(0)?);
                    add_sender(r.get(2)?, r.get(3)?);
                    has_attachments |= r.get::<_, bool>(4)?;
                    earlier.push(Member {
                        id,
                        message: MessageId(r.get(1)?),
                        labels: labels
                            .query_map([id.0], |r| Ok(LabelId(r.get(0)?)))?
                            .collect::<Result<_, _>>()?,
                    });
                }
            }
            add_sender(latest.from_name.clone(), latest.from_addr.clone());
            Ok(Conversation {
                latest,
                earlier,
                senders,
                has_attachments,
            })
        })
        .collect()
}

/// Of `params`, those `statement` names: a list's SQL varies with its size.
fn named<'a>(
    statement: &Statement,
    params: &[(&'a str, &'a dyn ToSql)],
) -> Vec<(&'a str, &'a dyn ToSql)> {
    params
        .iter()
        .filter(|(name, _)| matches!(statement.parameter_index(name), Ok(Some(_))))
        .copied()
        .collect()
}

/// Conversations with messages matching `query`, newest first. Each word
/// of the query must match, as a prefix of a word in the message.
pub fn search(
    conn: &mut Connection,
    query: &str,
    limit: u32,
) -> Result<(TxLogId, Vec<Conversation>)> {
    let fts_query = query
        .split_whitespace()
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ");
    let tx = conn.transaction()?;
    let as_of = as_of(&tx)?;
    let listing = Listing {
        scope: Scope::Unhidden,
        matches: Box::new(|x| {
            format!("{x}.message IN (SELECT rowid FROM message_fts WHERE message_fts MATCH :query)")
        }),
        candidates: Some((
            "message_fts f JOIN mail_entry c ON c.message = f.rowid".into(),
            "message_fts MATCH :query".into(),
        )),
        walk: ("mail_entry_by_received", "1".into()),
    };
    let conversations = page(&tx, &listing, &[(":query", &fts_query)], None, limit)?;
    Ok((as_of, conversations))
}

/// The message's entries, in every account, newest first; `None` if there
/// is no such message.
pub fn message_entries(
    conn: &mut Connection,
    message: MessageId,
) -> Result<Option<(TxLogId, Vec<Entry>)>> {
    let tx = conn.transaction()?;
    let as_of = as_of(&tx)?;
    if preferred_variant(&tx, message)?.is_none() {
        return Ok(None);
    }
    let entries = tx
        .prepare(&format!(
            "SELECT {ENTRY_COLUMNS}
             FROM mail_entry e JOIN message m ON m.id = e.message
             WHERE e.message = ?1
             ORDER BY e.received_at DESC, e.id DESC"
        ))?
        .query_map([message.0], entry_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some((as_of, with_labels(&tx, entries)?)))
}

/// The entries of the entry's conversation, whatever their labels, oldest
/// first; `None` if there is no such entry.
pub fn conversation(
    conn: &mut Connection,
    entry: EntryId,
) -> Result<Option<(TxLogId, Vec<Entry>)>> {
    let tx = conn.transaction()?;
    let as_of = as_of(&tx)?;
    let Some((account, thread_id)) = tx
        .query_row(
            "SELECT account_id, thread_id FROM mail_entry WHERE id = ?1",
            [entry.0],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let select = |condition: &str| {
        format!(
            "SELECT {ENTRY_COLUMNS}
             FROM mail_entry e JOIN message m ON m.id = e.message
             WHERE {condition}
             ORDER BY e.received_at, e.id"
        )
    };
    let entries = match thread_id {
        None => tx
            .prepare(&select("e.id = ?1"))?
            .query_map([entry.0], entry_row)?
            .collect::<Result<Vec<_>, _>>()?,
        Some(thread_id) => tx
            .prepare(&select("e.account_id = ?1 AND e.thread_id = ?2"))?
            .query_map(params![account, thread_id], entry_row)?
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(Some((as_of, with_labels(&tx, entries)?)))
}

fn entry_row(r: &Row) -> uplink_storage::rusqlite::Result<Entry> {
    Ok(Entry {
        id: EntryId(r.get(0)?),
        account: r.get(1)?,
        message: MessageId(r.get(2)?),
        thread_id: r.get(3)?,
        received_at: r.get(4)?,
        subject: r.get(5)?,
        from_addr: r.get(6)?,
        from_name: r.get(7)?,
        snippet: r.get(8)?,
        has_attachments: r.get(9)?,
        labels: vec![],
    })
}

fn with_labels(tx: &Transaction, mut entries: Vec<Entry>) -> Result<Vec<Entry>> {
    let mut stmt =
        tx.prepare("SELECT label FROM entry_label WHERE entry = ?1 AND desired ORDER BY label")?;
    for entry in &mut entries {
        entry.labels = stmt
            .query_map([entry.id.0], |r| Ok(LabelId(r.get(0)?)))?
            .collect::<Result<_, _>>()?;
    }
    Ok(entries)
}

/// The account of each label, `None` for labels that don't exist.
pub fn label_accounts(conn: &Connection, labels: &[LabelId]) -> Result<Vec<Option<String>>> {
    let mut stmt = conn.prepare("SELECT account_id FROM label WHERE id = ?1")?;
    labels
        .iter()
        .map(|l| Ok(stmt.query_row([l.0], |r| r.get(0)).optional()?))
        .collect()
}

/// The account of each entry, `None` for entries that don't exist.
pub fn entry_accounts(conn: &Connection, entries: &[EntryId]) -> Result<Vec<Option<String>>> {
    let mut stmt = conn.prepare("SELECT account_id FROM mail_entry WHERE id = ?1")?;
    entries
        .iter()
        .map(|e| Ok(stmt.query_row([e.0], |r| r.get(0)).optional()?))
        .collect()
}

/// The message's preferred variant.
pub fn preferred_variant(
    conn: &Connection,
    message: MessageId,
) -> Result<Option<(String, String)>> {
    conn.query_row(
        "SELECT message_id, preferred_variant FROM message WHERE id = ?1",
        [message.0],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .context("reading message")
}
