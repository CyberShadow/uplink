//! Read-only queries over mailbox state, on a reader connection. Each runs
//! in one read transaction and reports the log position it reflects.

use anyhow::{Context, Result};
use uplink_protocol::{
    mail::{
        Conversation, Entry, EntryId, Label, LabelId, ListPosition, ListScope, ListingProgress,
        MailProvider, Member, MessageId, Sender,
    },
    TxLogId,
};
use uplink_query::{Fields, Invalid, Kind, Query, Term};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use uplink_tx_log::as_of;

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
        // Kept as entries change (SPEC §4).
        let labels = tx
            .prepare(
                "SELECT id, external_id, name, system, kind,
                        conversations, unread_conversations
                 FROM label
                 WHERE account_id = ?1
                 ORDER BY NOT system, name",
            )?
            .query_map([&account.id], |r| {
                Ok(Label {
                    id: LabelId(r.get(0)?),
                    external_id: r.get(1)?,
                    name: r.get(2)?,
                    system: r.get(3)?,
                    kind: crate::mailbox::kind_at(r, 4)?,
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
    pub conversations: Vec<Conversation>,
    pub next: Option<ListPosition>,
    pub scope: ListScope,
}

/// Why a query lists nothing: it names what does not exist.
#[derive(Debug)]
pub enum ListError {
    Invalid(Invalid),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ListError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl From<uplink_storage::rusqlite::Error> for ListError {
    fn from(e: uplink_storage::rusqlite::Error) -> Self {
        Self::Internal(e.into())
    }
}

/// Below this many candidates, a page is found by sorting them all; above,
/// by walking entries newest first (by an index) until the page is full,
/// which many candidates make quick. Candidates are the entries that meet
/// the whole condition, found by the conjunct of an index met by fewest:
/// two labels of many entries each may have few in common, which walking
/// would look through the whole mailbox for. Either way a page costs
/// about this many rows at most, or the fewest's, however large the
/// mailbox.
const SORT_AT_MOST: i64 = 2000;

/// A query as SQL is made of: a condition on an entry, with the parts
/// a page can be found by kept apart.
enum Condition {
    And(Vec<Condition>),
    Or(Vec<Condition>),
    Not(Box<Condition>),
    /// Its message matches the FTS5 expression.
    Fts(String),
    /// It has one of the labels.
    Label(Vec<LabelId>),
    /// It is the account's.
    Account(String),
    /// Any other: SQL over the entry alias it is given.
    Sql(Box<dyn Fn(&str) -> String>),
    /// Of every entry, or none: as a term mail lacks (`is:room`) is false.
    Const(bool),
}

impl Condition {
    /// All of `all`, constants folded away.
    fn and(all: Vec<Condition>) -> Condition {
        let mut kept = vec![];
        for c in all {
            match c {
                Condition::Const(true) => {}
                Condition::Const(false) => return Condition::Const(false),
                c => kept.push(c),
            }
        }
        match kept.len() {
            0 => Condition::Const(true),
            1 => kept.pop().unwrap(),
            _ => Condition::And(kept),
        }
    }

    /// Any of `any`, constants folded away.
    fn or(any: Vec<Condition>) -> Condition {
        let mut kept = vec![];
        for c in any {
            match c {
                Condition::Const(false) => {}
                Condition::Const(true) => return Condition::Const(true),
                c => kept.push(c),
            }
        }
        match kept.len() {
            0 => Condition::Const(false),
            1 => kept.pop().unwrap(),
            _ => Condition::Or(kept),
        }
    }

    fn not(c: Condition) -> Condition {
        match c {
            Condition::Const(b) => Condition::Const(!b),
            c => Condition::Not(Box::new(c)),
        }
    }

    /// SQL: whether entry `x` meets it.
    fn sql(&self, x: &str) -> String {
        let join = |all: &[Condition], with: &str, empty: &str| match all {
            [] => empty.to_owned(),
            _ => all
                .iter()
                .map(|c| format!("({})", c.sql(x)))
                .collect::<Vec<_>>()
                .join(with),
        };
        match self {
            Condition::And(all) => join(all, " AND ", "1"),
            Condition::Or(any) => join(any, " OR ", "0"),
            Condition::Not(c) => format!("NOT ({})", c.sql(x)),
            Condition::Fts(fts) => format!(
                "{x}.message IN (SELECT rowid FROM message_fts WHERE message_fts MATCH {})",
                literal(fts)
            ),
            Condition::Label(labels) => format!(
                "EXISTS (SELECT 1 FROM entry_label ml
                         WHERE ml.entry = {x}.id AND ml.label IN ({}) AND ml.desired)",
                ids(labels)
            ),
            Condition::Account(account) => format!("{x}.account_id = {}", literal(account)),
            Condition::Sql(sql) => sql(x),
            Condition::Const(true) => "1".into(),
            Condition::Const(false) => "0".into(),
        }
    }
}

/// A string as an SQL literal.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn ids(labels: &[LabelId]) -> String {
    labels
        .iter()
        .map(|l| l.0.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// SQL: whether entry `x` is in `scope`.
fn in_scope(scope: &ListScope, x: &str) -> String {
    let with = |labels: &[LabelId]| {
        format!(
            "EXISTS (SELECT 1 FROM entry_label sl
                         WHERE sl.entry = {x}.id AND sl.label IN ({}) AND sl.desired)",
            ids(labels)
        )
    };
    match scope {
        ListScope::Within { labels } => with(labels),
        ListScope::Except { labels } if labels.is_empty() => "1".into(),
        ListScope::Except { labels } => format!("NOT {}", with(labels)),
    }
}

/// What a label or account no mail account has makes of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unknown {
    /// An error, as in a list of mail: a misspelt name lists nothing.
    Refuse,
    /// A term false of mail: of a query of other kinds too, whose names
    /// were checked against all of them (a room's tag, a Matrix account).
    False,
}

/// A query made SQL, with the names it holds resolved.
struct Compiler<'a> {
    tx: &'a Transaction<'a>,
    accounts: &'a [ConfiguredAccount],
    unknown: Unknown,
    /// Trash's and Spam's labels the query names, but not to exclude them.
    hiding: Vec<LabelId>,
    /// Whether it names `in:anywhere`, but not to exclude it.
    anywhere: bool,
}

impl Compiler<'_> {
    fn condition(&mut self, query: &Query, positive: bool) -> Result<Condition, ListError> {
        if let Some(fts) = fts(query) {
            return Ok(Condition::Fts(fts));
        }
        Ok(match query {
            // Its text, in one FTS5 expression, is one index lookup.
            Query::And(all) => {
                let (text, rest) = fts_and(all);
                let mut conditions: Vec<Condition> = text.into_iter().map(Condition::Fts).collect();
                for q in rest {
                    conditions.push(self.condition(q, positive)?);
                }
                Condition::and(conditions)
            }
            Query::Or(any) => Condition::or(
                any.iter()
                    .map(|q| self.condition(q, positive))
                    .collect::<Result<_, _>>()?,
            ),
            Query::Not(q) => Condition::not(self.condition(q, !positive)?),
            Query::Term(term) => self.term(term, positive)?,
        })
    }

    fn term(&mut self, term: &Term, positive: bool) -> Result<Condition, ListError> {
        let sql = |f: Box<dyn Fn(&str) -> String>| Ok(Condition::Sql(f));
        match term {
            Term::Text { .. } => unreachable!("text is FTS"),
            Term::Me(fields) => {
                let addresses: Vec<(String, String)> = self
                    .accounts
                    .iter()
                    .map(|a| (literal(&a.id), literal(&a.address)))
                    .collect();
                let field = *fields;
                sql(Box::new(move |x| {
                    let of = |address: &str| match field {
                        Fields::From => format!("mm.from_addr = {address} COLLATE NOCASE"),
                        Fields::To => {
                            format!("{} OR {}", among("to", address), among("cc", address))
                        }
                        Fields::Cc => among("cc", address),
                        Fields::Subject | Fields::Body => unreachable!("no me: in {field:?}"),
                    };
                    let accounts: Vec<String> = addresses
                        .iter()
                        .map(|(id, address)| {
                            format!("({x}.account_id = {id} AND ({}))", of(address))
                        })
                        .collect();
                    match accounts.is_empty() {
                        true => "0".into(),
                        false => format!(
                            "EXISTS (SELECT 1 FROM message mm WHERE mm.id = {x}.message AND ({}))",
                            accounts.join(" OR ")
                        ),
                    }
                }))
            }
            Term::Label(name) => self.label(name, positive),
            Term::Unread => self.label("UNREAD", positive),
            Term::Kind(kind) => Ok(Condition::Const(*kind == Kind::Mail)),
            // Of other kinds.
            Term::New
            | Term::Mention
            | Term::Notifying
            | Term::Direct
            | Term::Invited
            | Term::Space(_)
            | Term::Due(_)
            | Term::Overdue
            | Term::HasDue
            | Term::Done => Ok(Condition::Const(false)),
            Term::Anywhere => {
                self.anywhere |= positive;
                sql(Box::new(|_| "1".into()))
            }
            Term::Account(account) => {
                let configured = self
                    .accounts
                    .iter()
                    .find(|a| a.id == *account || a.address.eq_ignore_ascii_case(account));
                match (configured, self.unknown) {
                    (Some(configured), _) => Ok(Condition::Account(configured.id.clone())),
                    (None, Unknown::Refuse) => {
                        Err(ListError::Invalid(Invalid(format!("no account {account}"))))
                    }
                    (None, Unknown::False) => Ok(Condition::Const(false)),
                }
            }
            Term::HasAttachment => sql(Box::new(|x| {
                format!(
                    "EXISTS (SELECT 1 FROM message mm
                             WHERE mm.id = {x}.message AND mm.has_attachments)"
                )
            })),
            &Term::After(at) => sql(Box::new(move |x| format!("{x}.received_at >= {at}"))),
            &Term::Before(at) => sql(Box::new(move |x| format!("{x}.received_at < {at}"))),
            &Term::Larger(bytes) => sql(Box::new(move |x| format!("{} > {bytes}", size(x)))),
            &Term::Smaller(bytes) => sql(Box::new(move |x| format!("{} < {bytes}", size(x)))),
            Term::MessageId(id) => {
                let id = literal(id);
                sql(Box::new(move |x| {
                    format!("{x}.message IN (SELECT id FROM message WHERE message_id = {id})")
                }))
            }
        }
    }
}

impl Compiler<'_> {
    /// Entries with a label by this name, in any account.
    fn label(&mut self, name: &str, positive: bool) -> Result<Condition, ListError> {
        let labels: Vec<(LabelId, bool)> = self
            .tx
            .prepare(
                "SELECT id, external_id IN ('TRASH', 'SPAM') FROM label
                 WHERE name = ?1 COLLATE NOCASE ORDER BY id",
            )?
            .query_map([name], |r| Ok((LabelId(r.get(0)?), r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        if labels.is_empty() {
            return match self.unknown {
                Unknown::Refuse => Err(ListError::Invalid(Invalid(format!("no label {name}")))),
                Unknown::False => Ok(Condition::Const(false)),
            };
        }
        if positive {
            self.hiding
                .extend(labels.iter().filter(|(_, hiding)| *hiding).map(|(l, _)| *l));
        }
        Ok(Condition::Label(
            labels.into_iter().map(|(l, _)| l).collect(),
        ))
    }
}

/// SQL: whether `address` is among message `mm`'s `field` (`to`, `cc`).
fn among(field: &str, address: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM json_each(mm.{field}_addrs) WHERE value = {address} COLLATE NOCASE)"
    )
}

/// SQL: the size of entry `x`'s copy of its message.
fn size(x: &str) -> String {
    format!("(SELECT size_bytes FROM message_variant WHERE sha256 = {x}.variant)")
}

/// Of terms that must all match, the text as one FTS5 expression, and
/// the rest. Text a term must lack is left out of it (FTS5's NOT is
/// binary) unless there is text it must have.
fn fts_and(all: &[Query]) -> (Option<String>, Vec<&Query>) {
    let mut have = vec![];
    let mut lack = vec![];
    let mut rest = vec![];
    for q in all {
        match (fts(q), q) {
            (Some(f), _) => have.push(format!("({f})")),
            (None, Query::Not(n)) if fts(n).is_some() => lack.push((q, fts(n).unwrap())),
            (None, _) => rest.push(q),
        }
    }
    if have.is_empty() {
        rest.extend(lack.into_iter().map(|(q, _)| q));
        return (None, rest);
    }
    let mut text = format!("({})", have.join(" AND "));
    for (_, f) in lack {
        text = format!("{text} NOT ({f})");
    }
    (Some(text), rest)
}

/// The query as one FTS5 expression, if it is of text alone.
fn fts(query: &Query) -> Option<String> {
    match query {
        Query::And(queries) => match fts_and(queries) {
            (text, rest) if rest.is_empty() => text,
            _ => None,
        },
        Query::Or(queries) => {
            let parts = queries
                .iter()
                .map(|q| fts(q).map(|f| format!("({f})")))
                .collect::<Option<Vec<_>>>()?;
            (!parts.is_empty()).then(|| parts.join(" OR "))
        }
        Query::Not(_) => None,
        Query::Term(Term::Text {
            fields,
            text,
            exact,
        }) => {
            let columns = match fields {
                None => "",
                Some(Fields::From) => "{from_addr from_name} : ",
                Some(Fields::To) => "{to_addrs cc_addrs} : ",
                Some(Fields::Cc) => "{cc_addrs} : ",
                Some(Fields::Subject) => "{subject} : ",
                Some(Fields::Body) => "{body_text} : ",
            };
            let prefix = if *exact { "" } else { "*" };
            Some(format!(
                "{columns}\"{}\"{prefix}",
                text.replace('"', "\"\"")
            ))
        }
        Query::Term(_) => None,
    }
}

/// Whether a mail label is by this name, ignoring case.
pub fn label_known(tx: &Transaction, name: &str) -> Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM label WHERE name = ?1 COLLATE NOCASE",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The conversations `query` lists, newest first, after `after`.
pub fn conversations(
    tx: &Transaction,
    accounts: &[ConfiguredAccount],
    query: &Query,
    unknown: Unknown,
    after: Option<ListPosition>,
    limit: u32,
) -> Result<Page, ListError> {
    let (condition, scope) = compile(tx, accounts, query, unknown)?;
    let conversations = match condition {
        // Of none, as a query of other kinds is: nothing to look for.
        Condition::Const(false) => vec![],
        condition => page(tx, &scope, condition, after, limit)?,
    };
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
        conversations,
        next,
        scope,
    })
}

/// How many conversations `query` lists.
pub fn count(
    tx: &Transaction,
    accounts: &[ConfiguredAccount],
    query: &Query,
    unknown: Unknown,
) -> Result<u64, ListError> {
    let (condition, scope) = compile(tx, accounts, query, unknown)?;
    if matches!(condition, Condition::Const(false)) {
        return Ok(0);
    }
    let (from, condition) = newest(tx, &scope, condition)?;
    let count: i64 = tx.query_row(
        &format!("SELECT count(*) FROM {from} WHERE {condition}"),
        [],
        |r| r.get(0),
    )?;
    Ok(count as u64)
}

/// Where the conversations `query` lists are, as the nav links to them
/// (SPEC §12.14): how many have an entry with each label (in scope), and
/// how many are of each account. If `unseen`, only those whose latest
/// entry came after each of their places was last seen (§12.15: the
/// `seen` table's, of Home, their account's All Mail and their labels).
#[derive(Debug, Default, PartialEq)]
pub struct Places {
    pub labels: Vec<(LabelId, u64)>,
    pub accounts: Vec<(String, u64)>,
}

pub fn places(
    tx: &Transaction,
    accounts: &[ConfiguredAccount],
    query: &Query,
    unknown: Unknown,
    unseen: bool,
) -> Result<Places, ListError> {
    let (condition, scope) = compile(tx, accounts, query, unknown)?;
    if matches!(condition, Condition::Const(false)) {
        return Ok(Places::default());
    }
    let (from, condition) = newest(tx, &scope, condition)?;
    // Home holds every conversation of its buckets, so those it showed
    // are seen: the unseen are only those after (and the rest is not
    // walked, which may be most of the mailbox).
    let (after, unseen) = match unseen {
        false => (0, "1".to_owned()),
        true => (
            tx.query_row(
                "SELECT coalesce(max(mail), 0) FROM seen WHERE place = 'home'",
                [],
                |r| r.get::<_, i64>(0),
            )?,
            // After their account's All Mail and each of their labels.
            "n.id > max(
               coalesce((SELECT s.mail FROM seen s WHERE s.place = 'account:' || n.account_id), 0),
               coalesce((SELECT ls.mark FROM label_seen ls WHERE ls.conversation = n.id), 0))"
                .to_owned(),
        ),
    };
    // The conversations once, by their newest entries; their entries by
    // their thread (by its index); and their labels.
    let mut places = Places::default();
    let mut stmt = tx.prepare(&format!(
        "WITH newest AS MATERIALIZED (
           SELECT e.id, e.account_id, e.thread_id FROM {from}
           WHERE e.id > {after} AND ({condition})),
         members (conversation, entry) AS (
           SELECT id, id FROM newest WHERE thread_id IS NULL
           UNION ALL
           SELECT n.id, m.id FROM newest n
           JOIN mail_entry m ON m.account_id = n.account_id AND m.thread_id = n.thread_id
           WHERE {scope}),
         labelled AS MATERIALIZED (
           SELECT DISTINCT mb.conversation, el.label FROM members mb
           JOIN entry_label el ON el.entry = mb.entry AND el.desired),
         label_seen AS MATERIALIZED (
           SELECT l.conversation, max(s.mail) AS mark FROM labelled l
           JOIN seen s ON s.place = 'label:' || l.label
           GROUP BY l.conversation),
         counted AS MATERIALIZED (
           SELECT n.id, n.account_id FROM newest n WHERE {unseen})
         SELECT l.label, NULL, count(*) FROM labelled l JOIN counted c ON c.id = l.conversation
         GROUP BY l.label
         UNION ALL
         SELECT NULL, c.account_id, count(*) FROM counted c GROUP BY c.account_id",
        scope = in_scope(&scope, "m"),
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let count = row.get::<_, i64>(2)? as u64;
        match (
            row.get::<_, Option<i64>>(0)?,
            row.get::<_, Option<String>>(1)?,
        ) {
            (Some(label), None) => places.labels.push((LabelId(label), count)),
            (None, Some(account)) => places.accounts.push((account, count)),
            other => unreachable!("a place is a label or an account, not {other:?}"),
        }
    }
    places.labels.sort();
    places.accounts.sort();
    Ok(places)
}

/// The entries of every conversation `query` lists, in its scope, each
/// conversation's oldest first.
pub fn matching(
    tx: &Transaction,
    accounts: &[ConfiguredAccount],
    query: &Query,
    unknown: Unknown,
) -> Result<Vec<Vec<EntryId>>, ListError> {
    let (condition, scope) = compile(tx, accounts, query, unknown)?;
    if matches!(condition, Condition::Const(false)) {
        return Ok(vec![]);
    }
    let (from, condition) = newest(tx, &scope, condition)?;
    let newest = tx
        .prepare(&format!(
            "SELECT e.id, e.account_id, e.thread_id FROM {from} WHERE {condition}"
        ))?
        .query_map([], |r| {
            Ok((
                EntryId(r.get(0)?),
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut members = tx.prepare(&format!(
        "SELECT e.id FROM mail_entry e
         WHERE e.account_id = ?1 AND e.thread_id = ?2 AND {}
         ORDER BY e.received_at, e.id",
        in_scope(&scope, "e")
    ))?;
    Ok(newest
        .into_iter()
        .map(|(id, account, thread)| match thread {
            Some(thread) => members
                .query_map(params![account, thread], |r| Ok(EntryId(r.get(0)?)))?
                .collect(),
            None => Ok(vec![id]),
        })
        .collect::<Result<_, uplink_storage::rusqlite::Error>>()?)
}

/// The query made SQL: the condition an entry meets, and the scope of the
/// conversations it lists.
fn compile(
    tx: &Transaction,
    accounts: &[ConfiguredAccount],
    query: &Query,
    unknown: Unknown,
) -> Result<(Condition, ListScope), ListError> {
    let mut compiler = Compiler {
        tx,
        accounts,
        unknown,
        hiding: vec![],
        anywhere: false,
    };
    let condition = compiler.condition(query, true)?;
    let scope = match (compiler.anywhere, compiler.hiding) {
        (true, _) => ListScope::Except { labels: vec![] },
        (false, mut hiding) if !hiding.is_empty() => {
            hiding.sort();
            hiding.dedup();
            ListScope::Within { labels: hiding }
        }
        (false, _) => ListScope::Except {
            labels: tx
                .prepare("SELECT id FROM label WHERE external_id IN ('TRASH', 'SPAM') ORDER BY id")?
                .query_map([], |r| Ok(LabelId(r.get(0)?)))?
                .collect::<Result<_, _>>()?,
        },
    };
    Ok((condition, scope))
}

/// The conversations with an entry in scope that meets `condition`,
/// newest first, after `after`. A conversation is listed by its newest
/// entry in scope, which need not meet it itself (a reply in Sent, to a
/// thread in the Inbox).
fn page(
    tx: &Transaction,
    scope: &ListScope,
    condition: Condition,
    after: Option<ListPosition>,
    limit: u32,
) -> Result<Vec<Conversation>> {
    let (from, condition) = newest(tx, scope, condition)?;
    // Positions compare as (received_at, entry); the first page starts
    // after the greatest possible one.
    let after = after.unwrap_or(ListPosition {
        received_at: i64::MAX,
        entry: EntryId(i64::MAX),
    });
    let mut latest = tx.prepare(&format!(
        "SELECT {ENTRY_COLUMNS}
         FROM {from}
         JOIN message m ON m.id = e.message
         WHERE {condition}
           AND (e.received_at, e.id) < (?1, ?2)
         ORDER BY e.received_at DESC, e.id DESC
         LIMIT ?3"
    ))?;
    let latest = latest
        .query_map(params![after.received_at, after.entry.0, limit], entry_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let latest = with_labels(tx, latest)?;
    conversations_of(tx, scope, latest)
}

/// `FROM …` and `WHERE …` of the newest entries in scope of the
/// conversations with an entry in scope that meets `condition`, naming
/// them `e`.
fn newest(tx: &Transaction, scope: &ListScope, condition: Condition) -> Result<(String, String)> {
    let scope_sql = |x: &str| in_scope(scope, x);
    let mut conjuncts = match condition {
        Condition::And(all) => all,
        c => vec![c],
    };
    // The fewest candidates a conjunct is met by, if it is of an index:
    // `FROM … WHERE …`, naming the entries `c`. Each is counted by its
    // index alone (a label's rows are of entries), and only as far as it
    // may be the fewest.
    let mut fewest: Option<(i64, usize, String, String)> = None;
    for (at, conjunct) in conjuncts.iter().enumerate() {
        let (counted, from, condition) = match conjunct {
            Condition::Fts(fts) => (
                "message_fts f JOIN mail_entry c ON c.message = f.rowid",
                "message_fts f JOIN mail_entry c ON c.message = f.rowid",
                format!("message_fts MATCH {}", literal(fts)),
            ),
            Condition::Label(labels) => (
                "entry_label el",
                "entry_label el JOIN mail_entry c ON c.id = el.entry",
                format!("el.label IN ({}) AND el.desired", ids(labels)),
            ),
            _ => continue,
        };
        let limit = fewest.as_ref().map_or(i64::MAX, |(least, ..)| *least);
        let count: i64 = tx.query_row(
            &format!(
                "SELECT count(*) FROM (SELECT 1 FROM {counted} WHERE {condition} LIMIT {limit})"
            ),
            [],
            |r| r.get(0),
        )?;
        if count < limit {
            fewest = Some((count, at, from.to_owned(), condition));
        }
    }
    // Of the fewest, those that meet the other conjuncts too, up to one
    // too many.
    let candidates = |at: usize, from: &str, condition: &str| -> Result<i64> {
        let rest: Vec<String> = conjuncts
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != at)
            .map(|(_, c)| format!("({})", c.sql("c")))
            .collect();
        Ok(tx.query_row(
            &format!(
                "SELECT count(*) FROM (SELECT 1 FROM {from} WHERE {condition} AND {rest} AND {scope}
                                       LIMIT {limit})",
                rest = rest.join(" AND "),
                scope = scope_sql("c"),
                limit = SORT_AT_MOST + 1,
            ),
            [],
            |r| r.get(0),
        )?)
    };
    let sorted = match &fewest {
        Some((count, ..)) if *count <= SORT_AT_MOST => fewest,
        Some((_, at, from, condition))
            if conjuncts.len() > 1 && candidates(*at, from, condition)? <= SORT_AT_MOST =>
        {
            fewest
        }
        _ => None,
    };
    let (from, condition) = match sorted {
        // The conversations of the candidates, by their newest entries.
        Some((_, at, from, condition)) => {
            conjuncts.remove(at);
            let rest = Condition::And(conjuncts);
            (
                format!(
                    "(SELECT DISTINCT
                        CASE WHEN c.thread_id IS NULL THEN c.id
                        ELSE (SELECT n.id FROM mail_entry n
                              WHERE n.account_id = c.account_id AND n.thread_id = c.thread_id
                                AND {scope_n}
                              ORDER BY n.received_at DESC, n.id DESC LIMIT 1)
                        END AS id
                      FROM {from} WHERE {condition} AND {rest_c} AND {scope_c}) newest
                     JOIN mail_entry e ON e.id = newest.id",
                    scope_n = scope_sql("n"),
                    rest_c = rest.sql("c"),
                    scope_c = scope_sql("c"),
                ),
                "1".to_owned(),
            )
        }
        // Entries that are their conversation's newest, and whose
        // conversation has an entry that meets the condition, walked
        // newest first by an index: an account's, if the condition is of
        // one, since its conversations are.
        _ => {
            let (index, walked) = match conjuncts.iter().find_map(|c| match c {
                Condition::Account(account) => Some(account),
                _ => None,
            }) {
                Some(account) => (
                    "mail_entry_by_account_received",
                    format!("e.account_id = {}", literal(account)),
                ),
                None => ("mail_entry_by_received", "1".to_owned()),
            };
            let condition = Condition::And(conjuncts);
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
                    scope_e = scope_sql("e"),
                    scope_n = scope_sql("n"),
                    scope_x = scope_sql("x"),
                    matches_e = condition.sql("e"),
                    matches_x = condition.sql("x"),
                ),
            )
        }
    };
    Ok((from, condition))
}

/// The conversations whose newest entries in scope are `latest`.
fn conversations_of(
    tx: &Transaction,
    scope: &ListScope,
    latest: Vec<Entry>,
) -> Result<Vec<Conversation>> {
    let mut members = tx.prepare(&format!(
        "SELECT e.id, e.message, m.from_name, m.from_addr, m.has_attachments
         FROM mail_entry e JOIN message m ON m.id = e.message
         WHERE e.account_id = ?1 AND e.thread_id = ?2 AND e.id <> ?3
           AND {}
         ORDER BY e.received_at, e.id",
        in_scope(scope, "e")
    ))?;
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
                let mut rows = members.query(params![latest.account, thread_id, latest.id.0])?;
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
