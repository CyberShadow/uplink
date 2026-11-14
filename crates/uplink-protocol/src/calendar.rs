//! The calendar (SPEC §10).

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
    /// Cancelled by its organizer, but still on the calendar.
    pub cancelled: bool,
    pub show_as: ShowAs,
    /// The user's answer to it.
    pub response: EventResponse,
    /// Where to join its online meeting.
    pub join_url: Option<String>,
    /// How many minutes before its start the user is reminded of it, if
    /// at all.
    pub reminder: Option<i64>,
    /// When it last changed, as its calendar says: ms since the epoch.
    pub modified: i64,
}

/// How an event has the user's time shown to others (Graph `showAs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ShowAs {
    Free,
    Tentative,
    Busy,
    /// Out of office.
    Oof,
    WorkingElsewhere,
    Unknown,
}

/// An answer to a meeting request (Graph `responseType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum EventResponse {
    /// None asked for, as of an appointment.
    None,
    /// The user organizes it.
    Organizer,
    TentativelyAccepted,
    Accepted,
    Declined,
    NotResponded,
}

/// A meeting request being answered (SPEC §10.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PendingResponse {
    pub event: EventId,
    pub subject: String,
    pub response: EventResponse,
    /// The op that answered: undoing it takes the answer back.
    pub op: crate::compose::PendingOp,
    /// Why the provider refused, if it did: it needs a decision, which
    /// undoing the op makes.
    pub refused: Option<String>,
}

/// An event as the user makes it (SPEC §10.4): a single event, an
/// occurrence of a series, or a series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct EventFields {
    pub subject: String,
    pub location: String,
    /// HTML, as a message's body is written.
    pub body: String,
    /// Ms since the Unix epoch; of an all-day event, UTC midnights, of its
    /// first day and of the day after its last. Of a series, its first
    /// occurrence's.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    /// Its attendees, as typed: comma-separated, each `addr` or
    /// `Name <addr>`.
    pub required: String,
    pub optional: String,
    /// It is a Teams meeting.
    pub online_meeting: bool,
    /// Minutes before its start the user is reminded of it, if at all.
    pub reminder: Option<i64>,
    pub show_as: ShowAs,
    /// How a series recurs; `None` of a single event or an occurrence.
    pub recurrence: Option<Recurrence>,
}

/// What an edit of an event changes: the fields the user edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum EventField {
    Subject,
    Location,
    Body,
    /// Its start, end and whether it is all-day.
    Time,
    /// Its required and optional attendees.
    Attendees,
    OnlineMeeting,
    Reminder,
    ShowAs,
    Recurrence,
}

/// How a series recurs, as Graph has it (`patternedRecurrence`): every
/// `interval` days, weeks, months or years, as `pattern` says which,
/// from its first occurrence until `until`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Recurrence {
    pub pattern: RecurrencePattern,
    pub interval: u32,
    pub until: RecurrenceEnd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecurrencePattern {
    Daily,
    Weekly {
        days: Vec<Weekday>,
    },
    /// On a day of the month.
    AbsoluteMonthly {
        day: u8,
    },
    /// On the `index`th of `days` of the month (e.g. its second Tuesday).
    RelativeMonthly {
        index: WeekIndex,
        days: Vec<Weekday>,
    },
    AbsoluteYearly {
        month: u8,
        day: u8,
    },
    RelativeYearly {
        month: u8,
        index: WeekIndex,
        days: Vec<Weekday>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum Weekday {
    Sunday,
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
}

/// Which of the month's such days (Graph `weekIndex`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum WeekIndex {
    First,
    Second,
    Third,
    Fourth,
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecurrenceEnd {
    Never,
    /// Its last occurrence is on or before the date (`YYYY-MM-DD`, in the
    /// series' time zone).
    On {
        date: String,
    },
    After {
        count: u32,
    },
}

/// An event being created, changed or deleted (SPEC §10.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PendingEventChange {
    /// The event, of the series if the change is; `None` once it is gone
    /// (deleted, its deletion still to be given).
    pub event: Option<EventId>,
    pub subject: String,
    pub change: EventChangeKind,
    /// Of the series, rather than of one event.
    pub series: bool,
    /// The op that asked: undoing it takes the change back.
    pub op: crate::compose::PendingOp,
    /// Why the provider refused, if it did: it needs a decision, which
    /// undoing the op or changing the event again makes.
    pub refused: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum EventChangeKind {
    Create,
    Update,
    Delete,
}

/// What a calendar shows of an event opened: all it has, as the provider
/// has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct EventDetails {
    pub event: CalendarEvent,
    pub organizer: Option<Person>,
    pub attendees: Vec<Attendee>,
    /// The body, sanitized, if it is HTML. It may refer to remote images,
    /// which clients should not load unasked, as of a message's.
    pub body_html: Option<String>,
    /// The body, if it is text.
    pub body_text: Option<String>,
    pub remote_images: Vec<crate::mail::RemoteHost>,
    /// Where the provider's web app shows it.
    pub web_link: Option<String>,
    /// What the user can edit of it, as shown, if it is the user's own (not
    /// a meeting request, nor cancelled): its body as `body_html` has it.
    pub editable: Option<EventFields>,
    /// Of an occurrence of the user's own series, the series', once the
    /// calendar has it.
    pub series: Option<EventFields>,
    /// The provider's JSON of it, indented; `None` while being created.
    pub raw: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Person {
    /// The address, if it has no name.
    pub name: String,
    pub address: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Attendee {
    pub person: Person,
    pub kind: AttendeeKind,
    pub response: EventResponse,
}

/// Graph `attendeeType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum AttendeeKind {
    Required,
    Optional,
    /// A room or equipment.
    Resource,
}

/// A time zone the week's hours are also shown in (SPEC §10.2), beside
/// the browser's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TimeZone {
    /// What its column is headed.
    pub label: String,
    /// Its IANA name, e.g. `Europe/Berlin`.
    pub zone: String,
}

/// An account's calendar changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CalendarChanged {
    pub account: String,
}
