//! What actions on conversations change (SPEC §12.8), as Gmail's do: each
//! of their entries, but for marking unread and starring, which take the
//! latest.

use crate::mailbox::LabelChange;
use std::collections::HashMap;
use uplink_protocol::{
    mail::{ConversationAction, EntryId, LabelId},
    ErrorCode, ServerError,
};
use uplink_storage::rusqlite::{Connection, OptionalExtension};
use uplink_tx_log::answer::{error, internal};

/// A conversation's entries, oldest first, and their account.
struct Conversation {
    account: String,
    entries: Vec<EntryId>,
}

/// The label changes that do `action` to `conversations`, each given as
/// its entries in the list's scope.
pub fn changes(
    conn: &Connection,
    conversations: &[Vec<EntryId>],
    action: &ConversationAction,
) -> Result<Vec<LabelChange>, ServerError> {
    let conversations = conversations
        .iter()
        .map(|entries| conversation(conn, entries))
        .collect::<Result<Vec<_>, _>>()?;
    let mut labels = SystemLabels {
        conn,
        ids: HashMap::new(),
    };
    let mut changes = vec![];
    for c in &conversations {
        let account = &c.account;
        match *action {
            ConversationAction::Archive => {
                changes.extend(c.each(labels.of(account, "INBOX")?, false))
            }
            ConversationAction::Trash => changes.extend(c.each(labels.of(account, "TRASH")?, true)),
            ConversationAction::Spam { spam: true } => {
                changes.extend(c.each(labels.of(account, "SPAM")?, true))
            }
            ConversationAction::Spam { spam: false } => {
                changes.extend(c.each(labels.of(account, "SPAM")?, false));
                changes.extend(c.each(labels.of(account, "INBOX")?, true));
            }
            ConversationAction::Read { read: true } => {
                changes.extend(c.each(labels.of(account, "UNREAD")?, false))
            }
            ConversationAction::Read { read: false } => {
                changes.push(c.latest(labels.of(account, "UNREAD")?, true))
            }
            ConversationAction::Star { star: true } => {
                let star = labels.of(account, "STARRED")?;
                if !has_label(conn, &c.entries, star)? {
                    changes.push(c.latest(star, true));
                }
            }
            ConversationAction::Star { star: false } => {
                changes.extend(c.each(labels.of(account, "STARRED")?, false))
            }
            ConversationAction::Important { important } => {
                changes.extend(c.each(labels.of(account, "IMPORTANT")?, important))
            }
            ConversationAction::Label { label, add } => {
                of_account(conn, label, account)?;
                changes.extend(c.each(label, add));
            }
            ConversationAction::Move { from, to } => {
                if from == to {
                    return Err(error(
                        ErrorCode::Invalid,
                        "a move is into another label".into(),
                    ));
                }
                of_account(conn, from, account)?;
                of_account(conn, to, account)?;
                changes.extend(c.each(to, true));
                changes.extend(c.each(from, false));
            }
        }
    }
    Ok(changes)
}

impl Conversation {
    /// The change of the label on each entry.
    fn each(&self, label: LabelId, desired: bool) -> impl Iterator<Item = LabelChange> + '_ {
        self.entries.iter().map(move |&entry| LabelChange {
            entry,
            label,
            desired,
        })
    }

    /// The change of the label on the latest entry.
    fn latest(&self, label: LabelId, desired: bool) -> LabelChange {
        LabelChange {
            entry: *self.entries.last().expect("a conversation has entries"),
            label,
            desired,
        }
    }
}

/// The entries' conversation: they must exist, and be of one account.
fn conversation(conn: &Connection, entries: &[EntryId]) -> Result<Conversation, ServerError> {
    let mut stmt = conn
        .prepare_cached("SELECT account_id, received_at FROM mail_entry WHERE id = ?1")
        .map_err(|e| internal(e.into()))?;
    let mut found = vec![];
    for &entry in entries {
        let (account, received_at): (String, i64) = stmt
            .query_row([entry.0], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(|e| internal(e.into()))?
            .ok_or_else(|| error(ErrorCode::NotFound, format!("no entry {}", entry.0)))?;
        found.push((received_at, entry, account));
    }
    found.sort_by_key(|&(received_at, entry, _)| (received_at, entry));
    let account = match found.first() {
        Some((.., account)) => account.clone(),
        None => {
            return Err(error(
                ErrorCode::Invalid,
                "a conversation has entries".into(),
            ))
        }
    };
    if let Some((_, entry, _)) = found.iter().find(|(.., a)| *a != account) {
        return Err(error(
            ErrorCode::Invalid,
            format!("entry {} is not of its conversation's account", entry.0),
        ));
    }
    Ok(Conversation {
        account,
        entries: found.into_iter().map(|(_, entry, _)| entry).collect(),
    })
}

/// The accounts' system labels, by external id, as looked up.
struct SystemLabels<'a> {
    conn: &'a Connection,
    ids: HashMap<(String, &'static str), LabelId>,
}

impl SystemLabels<'_> {
    fn of(&mut self, account: &str, external_id: &'static str) -> Result<LabelId, ServerError> {
        if let Some(&id) = self.ids.get(&(account.to_owned(), external_id)) {
            return Ok(id);
        }
        let id = self
            .conn
            .query_row(
                "SELECT id FROM label WHERE account_id = ?1 AND external_id = ?2",
                (account, external_id),
                |r| Ok(LabelId(r.get(0)?)),
            )
            .optional()
            .map_err(|e| internal(e.into()))?
            .ok_or_else(|| {
                error(
                    ErrorCode::Invalid,
                    format!("account {account} has no {external_id}"),
                )
            })?;
        self.ids.insert((account.to_owned(), external_id), id);
        Ok(id)
    }
}

/// Whether one of the entries has the label.
fn has_label(conn: &Connection, entries: &[EntryId], label: LabelId) -> Result<bool, ServerError> {
    let mut stmt = conn
        .prepare_cached("SELECT 1 FROM entry_label WHERE entry = ?1 AND label = ?2 AND desired")
        .map_err(|e| internal(e.into()))?;
    for entry in entries {
        if stmt
            .exists((entry.0, label.0))
            .map_err(|e| internal(e.into()))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The label must be the account's: labels belong to one.
fn of_account(conn: &Connection, label: LabelId, account: &str) -> Result<(), ServerError> {
    let label_account: String = conn
        .query_row(
            "SELECT account_id FROM label WHERE id = ?1",
            [label.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| internal(e.into()))?
        .ok_or_else(|| error(ErrorCode::NotFound, format!("no label {}", label.0)))?;
    if label_account != account {
        return Err(error(
            ErrorCode::Invalid,
            format!("label {} is not of account {account}", label.0),
        ));
    }
    Ok(())
}
