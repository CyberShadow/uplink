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
    /// The provider's JSON of it, indented.
    pub raw: String,
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
