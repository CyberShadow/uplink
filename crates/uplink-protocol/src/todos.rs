//! To-dos (SPEC §11).

use crate::mail::MessageId;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct TodoId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Todo {
    pub id: TodoId,
    pub title: String,
    pub notes: String,
    /// A date, `YYYY-MM-DD`.
    pub due: Option<String>,
    /// When it was done, ms since the Unix epoch; `None` while open.
    pub done_at: Option<i64>,
    /// The message it was flagged from, which is flagged while it is open.
    pub message: Option<MessageId>,
    /// Ms since the Unix epoch.
    pub created_at: i64,
}

/// A change to one of a to-do's fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "field", rename_all = "snake_case", deny_unknown_fields)]
pub enum TodoEdit {
    Title { title: String },
    Notes { notes: String },
    Due { due: Option<String> },
    Done { done: bool },
}

/// To-dos were added, removed or changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TodosChanged {
    pub todos: Vec<TodoId>,
}
