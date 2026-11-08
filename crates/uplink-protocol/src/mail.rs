//! Mail: accounts, labels, entries and messages (SPEC §4.3, §8).
//!
//! A *message* is the logical message (one per Message-ID, across
//! accounts); an *entry* is a message as it exists in one account, with
//! that account's labels. Read and starred state are labels (`UNREAD`,
//! `STARRED`), so label changes are the only kind of mailbox edit.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct MessageId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct EntryId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct LabelId(pub i64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub id: String,
    pub address: String,
    /// Whether the first full sync has completed.
    pub synced: bool,
    pub labels: Vec<Label>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Label {
    pub id: LabelId,
    /// The provider's id, e.g. `INBOX` or `Label_12`.
    pub external_id: String,
    pub name: String,
    /// Defined by the provider rather than the user.
    pub system: bool,
    /// Entries carrying the label (as desired).
    pub total: u32,
    /// Of those, the ones also labelled `UNREAD`.
    pub unread: u32,
}

/// An entry as shown in a message list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: EntryId,
    pub account: String,
    pub message: MessageId,
    pub thread_id: Option<String>,
    /// When the account received it, ms since the Unix epoch.
    pub received_at: i64,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub from_name: Option<String>,
    pub snippet: String,
    /// Labels as desired, i.e. including changes not yet pushed.
    pub labels: Vec<LabelId>,
}

/// What a message list is of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntryList {
    /// The entries with the label.
    Label { label: LabelId },
    /// The account's entries outside Trash and Spam: Gmail's All Mail.
    AllMail { account: String },
}

/// Where a page of a message list ends; lists run newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ListPosition {
    pub received_at: i64,
    pub entry: EntryId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MessageContent {
    pub id: MessageId,
    pub message_id: String,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub from_name: Option<String>,
    pub to_addrs: Vec<String>,
    pub cc_addrs: Vec<String>,
    /// The `Date:` header, unix seconds.
    pub date_sent: Option<i64>,
    pub body_text: String,
    /// The HTML body, sanitized, if the message has one. It may refer to
    /// remote resources, which clients should not load unasked.
    pub body_html: Option<String>,
}

// ── events ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct EntriesAdded {
    pub entries: Vec<EntryId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct EntriesRemoved {
    pub entries: Vec<EntryId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct LabelsChanged {
    pub changes: Vec<LabelState>,
}

/// An entry's membership of a label after a change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct LabelState {
    pub entry: EntryId,
    pub label: LabelId,
    pub desired: bool,
    pub observed: bool,
}

/// An account's labels were created, renamed or deleted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct LabelListChanged {
    pub account: String,
}
