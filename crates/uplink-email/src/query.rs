//! Read-only queries over mailbox state, on a reader connection. Each runs
//! in one read transaction and reports the log position it reflects.

use anyhow::{Context, Result};
use uplink_protocol::{
    mail::{Entry, EntryId, EntryList, Label, LabelId, ListPosition, MessageId},
    TxLogId,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Row, ToSql, Transaction};
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
        let labels = tx
            .prepare(&format!(
                "SELECT l.id, l.external_id, l.name, l.system,
                        count(el.entry),
                        count(unread.entry)
                 FROM label l
                 LEFT JOIN entry_label el ON el.label = l.id AND el.desired
                   AND (l.external_id IN ('TRASH', 'SPAM') OR NOT {})
                 LEFT JOIN entry_label unread
                   ON unread.entry = el.entry AND unread.desired
                  AND unread.label = (SELECT id FROM label
                                      WHERE account_id = l.account_id
                                        AND external_id = 'UNREAD')
                 WHERE l.account_id = ?1
                 GROUP BY l.id
                 ORDER BY NOT l.system, l.name",
                hidden("el.entry")
            ))?
            .query_map([&account.id], |r| {
                Ok(Label {
                    id: LabelId(r.get(0)?),
                    external_id: r.get(1)?,
                    name: r.get(2)?,
                    system: r.get(3)?,
                    total: r.get(4)?,
                    unread: r.get(5)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        accounts.push(uplink_protocol::mail::Account {
            id: account.id.clone(),
            address: account.address.clone(),
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

/// The list's entries, newest first, after `after`.
pub fn entries(
    conn: &mut Connection,
    list: &EntryList,
    after: Option<ListPosition>,
    limit: u32,
) -> Result<Page> {
    let tx = conn.transaction()?;
    let as_of = as_of(&tx)?;
    // Positions compare as (received_at, entry); the first page starts
    // after the greatest possible one.
    let after = after.unwrap_or(ListPosition {
        received_at: i64::MAX,
        entry: EntryId(i64::MAX),
    });
    let unhidden = format!("NOT {}", hidden("e.id"));
    // `?1` is what the list is of.
    let (key, from, condition): (Box<dyn ToSql>, &str, String) = match list {
        EntryList::Label { label } => {
            let external_id: String = tx.query_row(
                "SELECT external_id FROM label WHERE id = ?1",
                [label.0],
                |r| r.get(0),
            )?;
            let condition = match external_id.as_str() {
                "TRASH" | "SPAM" => "el.label = ?1 AND el.desired".to_owned(),
                _ => format!("el.label = ?1 AND el.desired AND {unhidden}"),
            };
            (
                Box::new(label.0),
                "entry_label el JOIN mail_entry e ON e.id = el.entry",
                condition,
            )
        }
        EntryList::AllMail { account } => (
            Box::new(account.clone()),
            "mail_entry e",
            format!("e.account_id = ?1 AND {unhidden}"),
        ),
    };
    let entries = tx
        .prepare(&format!(
            "SELECT {ENTRY_COLUMNS}
             FROM {from}
             JOIN message m ON m.id = e.message
             WHERE {condition}
               AND (e.received_at, e.id) < (?2, ?3)
             ORDER BY e.received_at DESC, e.id DESC
             LIMIT ?4"
        ))?
        .query_map(
            params![key, after.received_at, after.entry.0, limit],
            entry_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let entries = with_labels(&tx, entries)?;
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

/// Entries of messages matching `query`, newest first. Each word of the
/// query must match, as a prefix of a word in the message.
pub fn search(conn: &mut Connection, query: &str, limit: u32) -> Result<(TxLogId, Vec<Entry>)> {
    let hidden = hidden("e.id");
    let fts_query = query
        .split_whitespace()
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ");
    let tx = conn.transaction()?;
    let as_of = as_of(&tx)?;
    let entries = tx
        .prepare(&format!(
            "SELECT {ENTRY_COLUMNS}
             FROM message_fts f
             JOIN mail_entry e ON e.message = f.rowid
             JOIN message m ON m.id = e.message
             WHERE message_fts MATCH ?1 AND NOT {hidden}
             ORDER BY e.received_at DESC, e.id DESC
             LIMIT ?2"
        ))?
        .query_map(params![fts_query, limit], entry_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok((as_of, with_labels(&tx, entries)?))
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
