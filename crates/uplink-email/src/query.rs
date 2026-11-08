//! Read-only queries over mailbox state, on a reader connection. Each runs
//! in one read transaction and reports the log position it reflects.

use anyhow::{Context, Result};
use uplink_protocol::{
    mail::{Entry, EntryId, EntryList, Label, LabelId, ListPosition, MailProvider, MessageId},
    TxLogId,
};
use uplink_storage::rusqlite::{Connection, OptionalExtension, Row, ToSql, Transaction};
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
        let synced: bool = tx
            .query_row(
                "SELECT sync_state IS NOT NULL FROM account WHERE id = ?1",
                [&account.id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        // Per label, so each count is of its own index entries; what Trash
        // and Spam hide is found once.
        let labels = tx
            .prepare(
                "WITH hidden AS (
                   SELECT entry FROM entry_label
                   WHERE desired AND label IN (SELECT id FROM label
                                               WHERE account_id = ?1
                                                 AND external_id IN ('TRASH', 'SPAM'))),
                 unread AS (
                   SELECT entry FROM entry_label
                   WHERE desired AND label = (SELECT id FROM label
                                              WHERE account_id = ?1 AND external_id = 'UNREAD'))
                 SELECT l.id, l.external_id, l.name, l.system, l.folder,
                   (SELECT count(*) FROM entry_label el
                    WHERE el.label = l.id AND el.desired
                      AND (l.external_id IN ('TRASH', 'SPAM')
                           OR el.entry NOT IN (SELECT entry FROM hidden))),
                   (SELECT count(*) FROM unread u
                    JOIN entry_label el ON el.entry = u.entry AND el.label = l.id AND el.desired
                    WHERE l.external_id IN ('TRASH', 'SPAM')
                       OR u.entry NOT IN (SELECT entry FROM hidden))
                 FROM label l
                 WHERE l.account_id = ?1
                 ORDER BY NOT l.system, l.name",
            )?
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
            labels,
        });
    }
    Ok((as_of, accounts))
}

const ENTRY_COLUMNS: &str = "e.id, e.account_id, e.message, e.thread_id, e.received_at,
                             m.subject, m.from_addr, m.from_name, m.snippet";

/// A page of entries, newest first.
pub struct Page {
    pub as_of: TxLogId,
    pub entries: Vec<Entry>,
    pub next: Option<ListPosition>,
}

/// Below this many candidates, a page is found by sorting them all; above,
/// by walking entries newest first (by an index) until the page is full,
/// which many candidates make quick. Either way a page costs about this
/// many rows at most, however large the mailbox.
const SORT_AT_MOST: i64 = 2000;

/// The list's entries, newest first, after `after`.
pub fn entries(
    conn: &mut Connection,
    list: &EntryList,
    after: Option<ListPosition>,
    limit: u32,
) -> Result<Page> {
    let tx = conn.transaction()?;
    let as_of = as_of(&tx)?;
    let unhidden = format!("NOT {}", hidden("e.id"));
    type Params = Vec<(&'static str, Box<dyn ToSql>)>;
    let (params, from, condition): (Params, &str, String) = match list {
        EntryList::Label { label } => {
            let (account, external_id): (String, String) = tx.query_row(
                "SELECT account_id, external_id FROM label WHERE id = ?1",
                [label.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let unhidden = match external_id.as_str() {
                "TRASH" | "SPAM" => "1".to_owned(),
                _ => unhidden,
            };
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM entry_label WHERE label = ?1 AND desired",
                [label.0],
                |r| r.get(0),
            )?;
            if count <= SORT_AT_MOST {
                (
                    vec![(":label", Box::new(label.0))],
                    "entry_label el JOIN mail_entry e ON e.id = el.entry",
                    format!("el.label = :label AND el.desired AND {unhidden}"),
                )
            } else {
                (
                    vec![
                        (":label", Box::new(label.0)),
                        (":account", Box::new(account)),
                    ],
                    "mail_entry e INDEXED BY mail_entry_by_account_received",
                    format!(
                        "e.account_id = :account AND {unhidden}
                         AND EXISTS (SELECT 1 FROM entry_label el
                                     WHERE el.entry = e.id AND el.label = :label AND el.desired)"
                    ),
                )
            }
        }
        EntryList::AllMail { account } => (
            vec![(":account", Box::new(account.clone()))],
            "mail_entry e INDEXED BY mail_entry_by_account_received",
            format!("e.account_id = :account AND {unhidden}"),
        ),
        EntryList::Inbox => {
            let inboxes = "SELECT id FROM label WHERE external_id = 'INBOX'";
            let count: i64 = tx.query_row(
                &format!("SELECT count(*) FROM entry_label WHERE label IN ({inboxes}) AND desired"),
                [],
                |r| r.get(0),
            )?;
            if count <= SORT_AT_MOST {
                (
                    vec![],
                    "entry_label el JOIN mail_entry e ON e.id = el.entry",
                    format!("el.label IN ({inboxes}) AND el.desired AND {unhidden}"),
                )
            } else {
                (
                    vec![],
                    "mail_entry e INDEXED BY mail_entry_by_received",
                    format!(
                        "{unhidden}
                         AND EXISTS (SELECT 1 FROM entry_label el
                                     WHERE el.entry = e.id AND el.label IN ({inboxes})
                                       AND el.desired)"
                    ),
                )
            }
        }
    };
    let params: Vec<(&str, &dyn ToSql)> = params.iter().map(|(n, v)| (*n, &**v)).collect();
    let entries = page(&tx, from, &condition, &params, after, limit)?;
    let next = (entries.len() == limit as usize).then(|| {
        let last = entries.last().expect("a full page is not empty");
        ListPosition {
            received_at: last.received_at,
            entry: last.id,
        }
    });
    Ok(Page {
        as_of,
        entries,
        next,
    })
}

/// The entries `FROM from WHERE condition`, newest first, after `after`.
/// `from` names the entries `e`.
fn page(
    tx: &Transaction,
    from: &str,
    condition: &str,
    params: &[(&str, &dyn ToSql)],
    after: Option<ListPosition>,
    limit: u32,
) -> Result<Vec<Entry>> {
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
    let entries = tx
        .prepare(&format!(
            "SELECT {ENTRY_COLUMNS}
             FROM {from}
             JOIN message m ON m.id = e.message
             WHERE {condition}
               AND (e.received_at, e.id) < (:received_at, :entry)
             ORDER BY e.received_at DESC, e.id DESC
             LIMIT :limit"
        ))?
        .query_map(&*all, entry_row)?
        .collect::<Result<Vec<_>, _>>()?;
    with_labels(tx, entries)
}

/// Entries of messages matching `query`, newest first. Each word of the
/// query must match, as a prefix of a word in the message.
pub fn search(conn: &mut Connection, query: &str, limit: u32) -> Result<(TxLogId, Vec<Entry>)> {
    let fts_query = query
        .split_whitespace()
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ");
    let tx = conn.transaction()?;
    let as_of = as_of(&tx)?;
    let matches: i64 = tx.query_row(
        "SELECT count(*) FROM message_fts WHERE message_fts MATCH ?1",
        [&fts_query],
        |r| r.get(0),
    )?;
    let unhidden = format!("NOT {}", hidden("e.id"));
    let (from, condition) = if matches <= SORT_AT_MOST {
        (
            "message_fts f JOIN mail_entry e ON e.message = f.rowid",
            format!("message_fts MATCH :query AND {unhidden}"),
        )
    } else {
        (
            "mail_entry e INDEXED BY mail_entry_by_received",
            format!(
                "e.message IN (SELECT rowid FROM message_fts WHERE message_fts MATCH :query)
                 AND {unhidden}"
            ),
        )
    };
    let entries = page(
        &tx,
        from,
        &condition,
        &[(":query", &fts_query)],
        None,
        limit,
    )?;
    Ok((as_of, entries))
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
