//! Drafts and the outbox (SPEC §6.7, §7).
//!
//! A draft is edited by span replacements on its text fields, each based
//! on the draft's revision: an edit based on an older revision than the
//! current one is refused, and the client rebases it (§7.2). Offsets and
//! lengths count UTF-16 code units, as JavaScript strings do.

use crate::mail::MessageId;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Chosen by the client creating the draft (a UUID), so that it can be
/// edited before the server has answered.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct DraftId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct OutboxId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DraftContent {
    /// Address lists as typed, e.g. `Bob <bob@example.com>, carol@example.com`.
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    /// HTML, as the editor produces it.
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub id: DraftId,
    /// The account it is sent from.
    pub account: String,
    /// Number of edits applied since creation.
    pub revision: i64,
    pub content: DraftContent,
    /// The message this replies to.
    pub in_reply_to: Option<MessageId>,
    /// Ms since the Unix epoch.
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DraftField {
    To,
    Cc,
    Bcc,
    Subject,
    Body,
}

/// Replaces `delete` at `offset` of `field` with `insert`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Splice {
    pub field: DraftField,
    /// In UTF-16 code units.
    pub offset: u32,
    pub delete: String,
    pub insert: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    /// Waiting for the send delay to pass; can still be cancelled.
    Queued,
    /// Being handed to the provider.
    Sending,
    /// Accepted by the provider; listed until its copy is synced back.
    Sent,
    /// Not sent; `error` says why. Can be cancelled, returning the draft.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct OutboxItem {
    pub id: OutboxId,
    pub account: String,
    pub draft: Draft,
    pub state: OutboxState,
    pub error: Option<String>,
    /// When it is sent, ms since the Unix epoch.
    pub not_before: i64,
}

// ── events ────────────────────────────────────────────────────────────────

/// A draft's text changed by `splice`, making its revision `revision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DraftEdited {
    pub draft: DraftId,
    pub revision: i64,
    pub splice: Splice,
}

/// Drafts were created or deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DraftsChanged {
    pub drafts: Vec<DraftId>,
}

/// Outbox items were added, removed or changed state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct OutboxChanged {
    pub items: Vec<OutboxId>,
}
