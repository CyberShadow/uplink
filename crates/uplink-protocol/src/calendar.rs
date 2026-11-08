//! The calendar, read-only (SPEC §10).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(transparent)]
pub struct EventId(pub i64);

/// An event as the calendar shows it: a single event, or one occurrence
/// of a series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CalendarEvent {
    pub id: EventId,
    pub account: String,
    pub subject: String,
    pub location: String,
    /// The start of the body, as text.
    pub preview: String,
    /// Ms since the Unix epoch. An all-day event's start and end are UTC
    /// midnights, of its first day and of the day after its last.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    /// One occurrence of a series.
    pub recurring: bool,
    /// An occurrence of a series changed from what the series says.
    pub exception: bool,
}

/// An account's calendar changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CalendarChanged {
    pub account: String,
}
