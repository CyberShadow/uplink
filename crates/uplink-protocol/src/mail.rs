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
    pub provider: MailProvider,
    /// Whether the first full sync has completed.
    pub synced: bool,
    /// Until then, how far it has got, if the provider says how far it
    /// goes.
    pub listing: Option<ListingProgress>,
    pub labels: Vec<Label>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ListingProgress {
    /// Messages stored.
    pub stored: u32,
    /// Messages the mailbox held as the listing began, Spam and Trash
    /// included, as the listing does.
    pub total: u32,
}

/// Who keeps a mail account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum MailProvider {
    Gmail,
    M365,
    /// Any mail server, by IMAP and SMTP.
    Imap,
    /// Any mail server, by POP3 and SMTP.
    Pop3,
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
    /// One of the account's folders, of which an entry is in exactly one.
    pub folder: bool,
    /// Conversations with an entry carrying the label (as desired).
    pub total: u32,
    /// Of those, the ones with an entry carrying it and `UNREAD` too.
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
    /// Whether it has attachments other than images its HTML body shows.
    pub has_attachments: bool,
    /// Labels as desired, i.e. including changes not yet pushed.
    pub labels: Vec<LabelId>,
}

/// A conversation as a list shows it: the entries of one account's
/// thread (Gmail's `threadId`, Microsoft 365's `conversationId`), or an
/// entry without one by itself, as far as the list's scope goes: the
/// entries in Trash, in a list of Trash; else those not in Trash or Spam.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Conversation {
    /// Its newest entry: the row shows it, and the list is ordered by it.
    pub latest: Entry,
    /// Its other entries, oldest first.
    pub earlier: Vec<Member>,
    /// Who sent its entries, each once, in the order they first did.
    pub senders: Vec<Sender>,
    /// Whether any of its entries has attachments.
    pub has_attachments: bool,
}

/// An entry of a conversation other than its latest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub id: EntryId,
    pub message: MessageId,
    /// Labels as desired.
    pub labels: Vec<LabelId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Sender {
    pub name: Option<String>,
    pub addr: Option<String>,
}

/// Which entries a list's conversations are made of (SPEC §4): those
/// with one of the labels, in a list of Trash or Spam; else those with
/// none of them, Trash and Spam (none at all with `in:anywhere`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ListScope {
    Within { labels: Vec<LabelId> },
    Except { labels: Vec<LabelId> },
}

/// Conversations to act on (SPEC §12.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationSet {
    /// As a list shows them: each its entries in the list's scope.
    Listed { conversations: Vec<Vec<EntryId>> },
    /// Every conversation `query` lists, as a search does (SPEC §8.7),
    /// as far as its scope goes, when the op is done.
    Matching { query: String },
}

/// What is done to conversations, as Gmail's actions do (SPEC §12.8): to
/// each of their entries, but for marking unread and starring, which
/// take the latest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationAction {
    /// Out of the Inbox.
    Archive,
    /// Into Trash.
    Trash,
    /// Into Spam; or, not spam, out of it and into the Inbox.
    Spam {
        spam: bool,
    },
    /// Read, every entry; unread, the latest.
    Read {
        read: bool,
    },
    /// The latest entry of each conversation without a star gets one; or
    /// every star goes.
    Star {
        star: bool,
    },
    Important {
        important: bool,
    },
    /// Adds a label of the conversations' account, or removes it.
    Label {
        label: LabelId,
        add: bool,
    },
    /// Out of `from` into `to`, labels of the conversations' account.
    Move {
        from: LabelId,
        to: LabelId,
    },
}

/// Where a page of a message list ends: its last conversation's latest
/// entry. Lists run newest first.
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
    /// The hosts of its remote images, by name, and how many each has: but
    /// likely tracking pixels (tiny or hidden `<img>`s), which never load,
    /// and are only counted.
    pub remote_images: Vec<RemoteHost>,
    pub tracking_pixels: u32,
    /// The user's rules that have some of its remote images shown
    /// unasked (`AlwaysShowImages`).
    pub image_rules: Vec<ImageRule>,
    pub attachments: Vec<Attachment>,
}

/// A host a message's remote images are at, as its HTML body has them
/// fetched: `/api/remote-image/<host>?url=…`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RemoteHost {
    pub host: String,
    pub images: u32,
}

/// What has the user's mail's remote images shown unasked (SPEC §8.9):
/// all of a sender's, or those at a host, whoever's. Lowercased.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageRule {
    /// Mail whose `From:` is the address.
    Sender { address: String },
    /// Mail whose `From:` is at the domain, exactly.
    Domain { domain: String },
    /// Images at the host, in anyone's mail.
    Host { host: String },
}

/// A part of a message other than its body. Its content is had over HTTP:
/// `/api/messages/{message}/attachments/{index}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub index: u32,
    /// The file name the message gives it, if any.
    pub name: Option<String>,
    /// `type/subtype`.
    pub mime: String,
    /// Decoded, in bytes.
    pub size: u64,
    /// An image the HTML body shows, rather than a file attached.
    pub inline: bool,
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

/// Whether the user has the rule, now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ImageRuleChanged {
    pub rule: ImageRule,
    pub always: bool,
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
