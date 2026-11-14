//! Creating, changing and deleting the user's own events (SPEC §10.4),
//! as desired state (§6.6): an op records what the user asked, shown at
//! once, and the account's calendar sync gives it to the provider, which
//! tells the attendees.
//!
//! What is asked of an event, or of a series, is its `desired`: an event
//! to create, the fields edited (as they are to be), or its deletion. An
//! event's columns are as shown, `desired` over what the provider has
//! (`raw`), so that listing them needs nothing more; a change to a
//! series shows on its occurrences but for their times, which follow
//! once the provider has expanded it anew.
//!
//! A field edited back to what the provider has is no longer pending, so
//! an edit undone before it is given gives nothing.

use crate::{
    name,
    sync::{self, DateTime},
    zones::{self, Zone},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use uplink_protocol::{
    calendar::{
        CalendarChanged, EventChangeKind, EventField, EventFields, EventId, EventResponse,
        PendingEventChange, Recurrence, RecurrenceEnd, RecurrencePattern, ShowAs, WeekIndex,
        Weekday,
    },
    compose::PendingOp,
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{params, OptionalExtension, Transaction};
use uplink_tx_log::{this_entry, Applied, Conflict, Inverse, Observation, Observed, Op};

/// What the user asked of an event or a series that the provider does
/// not have yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Desired {
    /// To be created, as it is; `edited` since it was asked for, which
    /// stay pending should the provider have it as first asked.
    Create {
        fields: EventFields,
        #[serde(default)]
        edited: BTreeSet<EventField>,
        time_zone: String,
    },
    /// The fields `edited` to be as `fields` has them.
    Update {
        fields: EventFields,
        edited: BTreeSet<EventField>,
        time_zone: String,
    },
    Delete,
}

impl Desired {
    fn kind(&self) -> EventChangeKind {
        match self {
            Desired::Create { .. } => EventChangeKind::Create,
            Desired::Update { .. } => EventChangeKind::Update,
            Desired::Delete => EventChangeKind::Delete,
        }
    }
}

/// What a write is of: an event (single, or one occurrence), or a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Event(EventId),
    /// `calendar_series.id`.
    Series(i64),
}

impl Target {
    fn table(self) -> &'static str {
        match self {
            Target::Event(_) => "calendar_event",
            Target::Series(_) => "calendar_series",
        }
    }

    fn id(self) -> i64 {
        match self {
            Target::Event(id) => id.0,
            Target::Series(id) => id,
        }
    }
}

/// Fields of a series shown on its occurrences while pending: all but
/// their times, which the provider gives as it expands it.
const OF_OCCURRENCES: [EventField; 5] = [
    EventField::Subject,
    EventField::Location,
    EventField::Body,
    EventField::Reminder,
    EventField::ShowAs,
];

const ALL_FIELDS: [EventField; 9] = [
    EventField::Subject,
    EventField::Location,
    EventField::Body,
    EventField::Time,
    EventField::Attendees,
    EventField::OnlineMeeting,
    EventField::Reminder,
    EventField::ShowAs,
    EventField::Recurrence,
];

// ── the provider's JSON ───────────────────────────────────────────────────

/// A Graph event (or series master), as far as the user edits it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphEvent {
    subject: Option<String>,
    body: Option<GraphBody>,
    location: Option<GraphLocation>,
    start: DateTime,
    end: DateTime,
    #[serde(default)]
    is_all_day: bool,
    #[serde(default)]
    attendees: Vec<GraphAttendee>,
    #[serde(default)]
    is_online_meeting: bool,
    is_reminder_on: bool,
    reminder_minutes_before_start: i64,
    show_as: ShowAs,
    recurrence: Option<GraphRecurrence>,
    is_cancelled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphBody {
    content_type: String,
    content: String,
}

/// Text as HTML, a paragraph a line, as a text body is edited.
fn text_to_html(text: &str) -> String {
    text.lines()
        .map(|line| {
            let line = line
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            format!("<p>{line}</p>")
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphLocation {
    display_name: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GraphAttendee {
    email_address: GraphAddress,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize, Serialize, Clone)]
struct GraphAddress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    address: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphRecurrence {
    pattern: GraphPattern,
    range: GraphRange,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct GraphPattern {
    #[serde(rename = "type")]
    kind: String,
    interval: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    days_of_week: Vec<Weekday>,
    #[serde(default, skip_serializing_if = "is_zero")]
    day_of_month: u8,
    #[serde(default, skip_serializing_if = "is_zero")]
    month: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    index: Option<WeekIndex>,
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphRange {
    #[serde(rename = "type")]
    kind: String,
    start_date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    end_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    number_of_occurrences: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recurrence_time_zone: Option<String>,
}

impl GraphRecurrence {
    fn recurrence(self) -> Result<Recurrence> {
        let p = self.pattern;
        let pattern = match p.kind.as_str() {
            "daily" => RecurrencePattern::Daily,
            "weekly" => RecurrencePattern::Weekly {
                days: p.days_of_week,
            },
            "absoluteMonthly" => RecurrencePattern::AbsoluteMonthly {
                day: p.day_of_month,
            },
            "relativeMonthly" => RecurrencePattern::RelativeMonthly {
                index: p.index.unwrap_or(WeekIndex::First),
                days: p.days_of_week,
            },
            "absoluteYearly" => RecurrencePattern::AbsoluteYearly {
                month: p.month,
                day: p.day_of_month,
            },
            "relativeYearly" => RecurrencePattern::RelativeYearly {
                month: p.month,
                index: p.index.unwrap_or(WeekIndex::First),
                days: p.days_of_week,
            },
            other => bail!("a recurrence pattern {other}"),
        };
        let until = match self.range.kind.as_str() {
            "noEnd" => RecurrenceEnd::Never,
            "endDate" => RecurrenceEnd::On {
                date: self
                    .range
                    .end_date
                    .context("an endDate range without one")?,
            },
            "numbered" => RecurrenceEnd::After {
                count: self
                    .range
                    .number_of_occurrences
                    .context("a numbered range without a number")?,
            },
            other => bail!("a recurrence range {other}"),
        };
        Ok(Recurrence {
            pattern,
            interval: p.interval,
            until,
        })
    }

    /// Graph's of `recurrence`, the series starting at `start` (ms; of an
    /// all-day one, a UTC midnight) in `zone`.
    fn of(recurrence: &Recurrence, start: i64, all_day: bool, zone: &Zone) -> Self {
        let mut pattern = GraphPattern {
            interval: recurrence.interval,
            ..Default::default()
        };
        match &recurrence.pattern {
            RecurrencePattern::Daily => pattern.kind = "daily".into(),
            RecurrencePattern::Weekly { days } => {
                pattern.kind = "weekly".into();
                pattern.days_of_week.clone_from(days);
            }
            RecurrencePattern::AbsoluteMonthly { day } => {
                pattern.kind = "absoluteMonthly".into();
                pattern.day_of_month = *day;
            }
            RecurrencePattern::RelativeMonthly { index, days } => {
                pattern.kind = "relativeMonthly".into();
                pattern.index = Some(*index);
                pattern.days_of_week.clone_from(days);
            }
            RecurrencePattern::AbsoluteYearly { month, day } => {
                pattern.kind = "absoluteYearly".into();
                pattern.month = *month;
                pattern.day_of_month = *day;
            }
            RecurrencePattern::RelativeYearly { month, index, days } => {
                pattern.kind = "relativeYearly".into();
                pattern.month = *month;
                pattern.index = Some(*index);
                pattern.days_of_week.clone_from(days);
            }
        }
        let (kind, end_date, number_of_occurrences) = match &recurrence.until {
            RecurrenceEnd::Never => ("noEnd", None, None),
            RecurrenceEnd::On { date } => ("endDate", Some(date.clone()), None),
            RecurrenceEnd::After { count } => ("numbered", None, Some(*count)),
        };
        GraphRecurrence {
            pattern,
            range: GraphRange {
                kind: kind.into(),
                start_date: if all_day {
                    zones::utc_date(start)
                } else {
                    zone.date(start)
                },
                end_date,
                number_of_occurrences,
                recurrence_time_zone: Some(zone.windows.to_owned()),
            },
        }
    }
}

/// An address list as typed, as compose's: `addr`, or `"Name" <addr>`.
fn typed(attendees: &[&GraphAttendee]) -> String {
    attendees
        .iter()
        .filter_map(|a| {
            let address = a.email_address.address.as_deref()?;
            Some(match a.email_address.name.as_deref() {
                Some(name) if !name.is_empty() && name != address => format!(
                    "\"{}\" <{address}>",
                    name.replace('\\', "\\\\").replace('"', "\\\"")
                ),
                _ => address.to_owned(),
            })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What the provider has of the event (or series master) of `raw`, as
/// the user edits it; its body as Graph has it.
pub(crate) fn observed(raw: &str) -> Result<EventFields> {
    let event: GraphEvent = serde_json::from_str(raw).context("an event's JSON")?;
    let of = |kind: &str| -> Vec<&GraphAttendee> {
        event.attendees.iter().filter(|a| a.kind == kind).collect()
    };
    Ok(EventFields {
        subject: event.subject.clone().unwrap_or_default(),
        location: event
            .location
            .as_ref()
            .and_then(|l| l.display_name.clone())
            .unwrap_or_default(),
        body: match &event.body {
            Some(b) if b.content_type == "text" => text_to_html(&b.content),
            Some(b) => b.content.clone(),
            None => String::new(),
        },
        start: event.start.ms()?,
        end: event.end.ms()?,
        all_day: event.is_all_day,
        required: typed(&of("required")),
        optional: typed(&of("optional")),
        online_meeting: event.is_online_meeting,
        reminder: event
            .is_reminder_on
            .then_some(event.reminder_minutes_before_start),
        show_as: event.show_as,
        recurrence: event
            .recurrence
            .map(GraphRecurrence::recurrence)
            .transpose()?,
    })
}

/// The rooms and equipment the event of `raw` has, which the user does not
/// edit: kept as its attendees are changed.
fn resources(raw: Option<&str>) -> Result<Vec<GraphAttendee>> {
    let Some(raw) = raw else { return Ok(vec![]) };
    let event: GraphEvent = serde_json::from_str(raw).context("an event's JSON")?;
    Ok(event
        .attendees
        .into_iter()
        .filter(|a| a.kind == "resource")
        .collect())
}

/// The addresses of a list as typed, as compared: lowercased, as a set.
fn addresses(list: &str) -> Result<BTreeSet<String>> {
    Ok(uplink_email::compose::parse_addresses(list)?
        .into_iter()
        .map(|(_, address)| address.to_lowercase())
        .collect())
}

/// Whether `a` and `b` have `field` the same.
fn same(field: EventField, a: &EventFields, b: &EventFields) -> Result<bool> {
    Ok(match field {
        EventField::Subject => a.subject == b.subject,
        EventField::Location => a.location == b.location,
        EventField::Body => a.body == b.body,
        EventField::Time => (a.start, a.end, a.all_day) == (b.start, b.end, b.all_day),
        EventField::Attendees => {
            (addresses(&a.required)?, addresses(&a.optional)?)
                == (addresses(&b.required)?, addresses(&b.optional)?)
        }
        EventField::OnlineMeeting => a.online_meeting == b.online_meeting,
        EventField::Reminder => a.reminder == b.reminder,
        EventField::ShowAs => a.show_as == b.show_as,
        EventField::Recurrence => a.recurrence == b.recurrence,
    })
}

/// `fields` with `field` as `from` has it.
fn set(fields: &mut EventFields, field: EventField, from: &EventFields) {
    match field {
        EventField::Subject => fields.subject.clone_from(&from.subject),
        EventField::Location => fields.location.clone_from(&from.location),
        EventField::Body => fields.body.clone_from(&from.body),
        EventField::Time => {
            (fields.start, fields.end, fields.all_day) = (from.start, from.end, from.all_day);
        }
        EventField::Attendees => {
            fields.required.clone_from(&from.required);
            fields.optional.clone_from(&from.optional);
        }
        EventField::OnlineMeeting => fields.online_meeting = from.online_meeting,
        EventField::Reminder => fields.reminder = from.reminder,
        EventField::ShowAs => fields.show_as = from.show_as,
        EventField::Recurrence => fields.recurrence.clone_from(&from.recurrence),
    }
}

/// Graph's JSON setting `fields`' `edited` fields, the event in `zone`;
/// keeping `resources` among its attendees.
fn graph_body(
    fields: &EventFields,
    edited: &BTreeSet<EventField>,
    zone: &Zone,
    resources: Vec<GraphAttendee>,
) -> Result<Value> {
    let mut body = serde_json::Map::new();
    for field in edited {
        match field {
            EventField::Subject => {
                body.insert("subject".into(), json!(fields.subject));
            }
            EventField::Location => {
                body.insert("location".into(), json!({ "displayName": fields.location }));
            }
            EventField::Body => {
                body.insert(
                    "body".into(),
                    json!({ "contentType": "html", "content": fields.body }),
                );
            }
            EventField::Time => {
                // An all-day event's days are the same wherever one is, as
                // the calendar reads them: given so, as UTC's.
                let time = |ms: i64| match fields.all_day {
                    true => {
                        json!({ "dateTime": format!("{}T00:00:00", zones::utc_date(ms)), "timeZone": "UTC" })
                    }
                    false => json!({ "dateTime": zone.wall(ms), "timeZone": zone.windows }),
                };
                body.insert("start".into(), time(fields.start));
                body.insert("end".into(), time(fields.end));
                body.insert("isAllDay".into(), json!(fields.all_day));
            }
            EventField::Attendees => {
                let mut attendees = vec![];
                for (list, kind) in [
                    (&fields.required, "required"),
                    (&fields.optional, "optional"),
                ] {
                    for (name, address) in uplink_email::compose::parse_addresses(list)? {
                        attendees.push(GraphAttendee {
                            email_address: GraphAddress {
                                name,
                                address: Some(address),
                            },
                            kind: kind.into(),
                        });
                    }
                }
                attendees.extend(resources.iter().cloned());
                body.insert("attendees".into(), serde_json::to_value(attendees)?);
            }
            EventField::OnlineMeeting => {
                body.insert("isOnlineMeeting".into(), json!(fields.online_meeting));
                if fields.online_meeting {
                    body.insert("onlineMeetingProvider".into(), json!("teamsForBusiness"));
                }
            }
            EventField::Reminder => {
                body.insert("isReminderOn".into(), json!(fields.reminder.is_some()));
                if let Some(minutes) = fields.reminder {
                    body.insert("reminderMinutesBeforeStart".into(), json!(minutes));
                }
            }
            EventField::ShowAs => {
                body.insert("showAs".into(), json!(name(fields.show_as)));
            }
            EventField::Recurrence => {
                let recurrence = fields
                    .recurrence
                    .as_ref()
                    .context("a series without a recurrence")?;
                body.insert(
                    "recurrence".into(),
                    serde_json::to_value(GraphRecurrence::of(
                        recurrence,
                        fields.start,
                        fields.all_day,
                        zone,
                    ))?,
                );
            }
        }
    }
    Ok(Value::Object(body))
}

// ── what is stored ────────────────────────────────────────────────────────

/// An event's or a series' state, as writes read it.
struct State {
    account: String,
    external_id: Option<String>,
    raw: Option<String>,
    desired: Option<Desired>,
}

fn state(tx: &Transaction, target: Target) -> Result<Option<State>> {
    type Row = (String, Option<String>, Option<String>, Option<String>);
    let row: Option<Row> = tx
        .query_row(
            &format!(
                "SELECT account_id, external_id, raw, desired FROM {} WHERE id = ?1",
                target.table()
            ),
            [target.id()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    row.map(|(account, external_id, raw, desired)| {
        Ok(State {
            account,
            external_id,
            raw,
            desired: desired.map(|d| serde_json::from_str(&d)).transpose()?,
        })
    })
    .transpose()
}

impl State {
    /// What the provider has of it; `None` while being created.
    fn observed(&self) -> Result<Option<EventFields>> {
        self.raw.as_deref().map(observed).transpose()
    }

    /// Its fields as shown: what is asked over what the provider has.
    fn shown(&self) -> Result<EventFields> {
        match &self.desired {
            Some(Desired::Create { fields, .. }) => Ok(fields.clone()),
            Some(Desired::Update { fields, edited, .. }) => {
                let mut shown = self.observed()?.context("a change of nothing")?;
                for field in edited {
                    set(&mut shown, *field, fields);
                }
                Ok(shown)
            }
            Some(Desired::Delete) | None => self.observed()?.context("an event of nothing"),
        }
    }
}

/// Records what is asked of `target`, by the op running (none, `None`).
fn desire(tx: &Transaction, target: Target, desired: Option<&Desired>) -> Result<()> {
    let by = this_entry(tx)?.0;
    tx.execute(
        &format!(
            "UPDATE {} SET desired = ?2, desired_by = ?3, desired_refused = NULL WHERE id = ?1",
            target.table()
        ),
        params![
            target.id(),
            desired.map(serde_json::to_string).transpose()?,
            desired.is_some().then_some(by)
        ],
    )?;
    Ok(())
}

/// The events showing `target`: itself, or its series' occurrences.
fn showing(tx: &Transaction, target: Target) -> Result<Vec<EventId>> {
    Ok(match target {
        Target::Event(id) => vec![id],
        Target::Series(series) => tx
            .prepare("SELECT id FROM calendar_event WHERE series_id = ?1 ORDER BY id")?
            .query_map([series], |r| Ok(EventId(r.get(0)?)))?
            .collect::<Result<_, _>>()?,
    })
}

/// Sets the event's columns as shown: what the provider has, under what
/// is asked of it and of its series.
pub(crate) fn refresh(tx: &Transaction, id: EventId) -> Result<()> {
    let (raw, own, series): (Option<String>, Option<String>, Option<String>) = tx.query_row(
        "SELECT e.raw, e.desired, s.desired FROM calendar_event e
         LEFT JOIN calendar_series s ON s.id = e.series_id WHERE e.id = ?1",
        [id.0],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let parse = |d: Option<String>| -> Result<Option<Desired>> {
        d.map(|d| serde_json::from_str(&d))
            .transpose()
            .map_err(Into::into)
    };
    let (own, series) = (parse(own)?, parse(series)?);
    let mut shown = match (&raw, &own, &series) {
        (Some(raw), ..) => Shown::of(sync::observed(serde_json::from_str(raw)?)?),
        (None, Some(Desired::Create { fields, .. }), _)
        | (None, _, Some(Desired::Create { fields, .. })) => Shown::of_fields(fields),
        (None, ..) => bail!("event {} is of nothing", id.0),
    };
    if let Some(Desired::Update { fields, edited, .. }) = &series {
        for field in edited.iter().filter(|f| OF_OCCURRENCES.contains(f)) {
            shown.set(*field, fields);
        }
    }
    if let Some(Desired::Update { fields, edited, .. }) = &own {
        for field in edited {
            shown.set(*field, fields);
        }
    }
    tx.execute(
        "UPDATE calendar_event SET exception = ?2, subject = ?3, location = ?4, preview = ?5,
           start_ts = ?6, end_ts = ?7, all_day = ?8, cancelled = ?9, show_as = ?10,
           response = ?11, join_url = ?12, reminder = ?13, modified_ts = coalesce(?14, modified_ts)
         WHERE id = ?1",
        params![
            id.0,
            shown.exception,
            shown.subject,
            shown.location,
            shown.preview,
            shown.start,
            shown.end,
            shown.all_day,
            shown.cancelled,
            name(shown.show_as),
            name(shown.response),
            shown.join_url,
            shown.reminder,
            shown.modified,
        ],
    )?;
    Ok(())
}

/// The columns of an event as shown.
struct Shown {
    exception: bool,
    subject: String,
    location: String,
    preview: String,
    start: i64,
    end: i64,
    all_day: bool,
    cancelled: bool,
    show_as: ShowAs,
    response: EventResponse,
    join_url: Option<String>,
    reminder: Option<i64>,
    /// When the provider says it last changed; of one being created, when
    /// it was asked for, as recorded.
    modified: Option<i64>,
}

/// The start of a body, as text, as Graph's `bodyPreview` is.
fn preview(body: &str) -> String {
    let text = mail_parser::decoders::html::html_to_text(body);
    let words: Vec<&str> = text.split_whitespace().collect();
    words.join(" ").chars().take(255).collect()
}

impl Shown {
    fn of(event: crate::ObservedEvent) -> Self {
        Self {
            exception: event.exception,
            subject: event.subject,
            location: event.location,
            preview: event.preview,
            start: event.start,
            end: event.end,
            all_day: event.all_day,
            cancelled: event.cancelled,
            show_as: event.show_as,
            response: event.response,
            join_url: event.join_url,
            reminder: event.reminder,
            modified: Some(event.modified),
        }
    }

    /// Of one being created: the user's own.
    fn of_fields(fields: &EventFields) -> Self {
        Self {
            exception: false,
            subject: fields.subject.clone(),
            location: fields.location.clone(),
            preview: preview(&fields.body),
            start: fields.start,
            end: fields.end,
            all_day: fields.all_day,
            cancelled: false,
            show_as: fields.show_as,
            response: EventResponse::Organizer,
            join_url: None,
            reminder: fields.reminder,
            modified: None,
        }
    }

    fn set(&mut self, field: EventField, fields: &EventFields) {
        match field {
            EventField::Subject => self.subject.clone_from(&fields.subject),
            EventField::Location => self.location.clone_from(&fields.location),
            EventField::Body => self.preview = preview(&fields.body),
            EventField::Time => {
                (self.start, self.end, self.all_day) = (fields.start, fields.end, fields.all_day);
            }
            EventField::Reminder => self.reminder = fields.reminder,
            EventField::ShowAs => self.show_as = fields.show_as,
            EventField::Attendees | EventField::OnlineMeeting | EventField::Recurrence => {}
        }
    }
}

/// What a client shows of an event's fields: as shown, and those still
/// to be given, if any; `None` for an event the provider has as is.
pub(crate) fn overlay(
    tx: &Transaction,
    id: EventId,
) -> Result<Option<(EventFields, BTreeSet<EventField>)>> {
    let own = state(tx, Target::Event(id))?.context("no event")?;
    let series_id: Option<i64> = tx.query_row(
        "SELECT series_id FROM calendar_event WHERE id = ?1",
        [id.0],
        |r| r.get(0),
    )?;
    let series = series_id
        .map(|s| state(tx, Target::Series(s)))
        .transpose()?
        .flatten();
    let pending = |state: &State, of: &[EventField]| -> Option<BTreeSet<EventField>> {
        match &state.desired {
            Some(Desired::Create { .. }) => Some(of.iter().copied().collect()),
            Some(Desired::Update { edited, .. }) => {
                Some(edited.iter().filter(|f| of.contains(f)).copied().collect())
            }
            Some(Desired::Delete) | None => None,
        }
    };
    if own.raw.is_none() {
        // Being created, of itself or as its series is.
        let state = if own.desired.is_some() {
            &own
        } else {
            series.as_ref().context("of nothing")?
        };
        let mut fields = state.shown()?;
        fields.recurrence = None;
        return Ok(Some((fields, ALL_FIELDS.into_iter().collect())));
    }
    let of_series = series
        .as_ref()
        .is_some_and(|s| matches!(s.desired, Some(Desired::Update { .. })));
    let Some(mut edited) = pending(&own, &ALL_FIELDS).or(of_series.then(BTreeSet::new)) else {
        return Ok(None);
    };
    let mut fields = own.shown()?;
    if let Some(series) = &series {
        if let Some(Desired::Update {
            fields: of_series,
            edited: series_edited,
            ..
        }) = &series.desired
        {
            for field in series_edited.iter().filter(|f| OF_OCCURRENCES.contains(f)) {
                if !edited.contains(field) {
                    set(&mut fields, *field, of_series);
                    edited.insert(*field);
                }
            }
        }
    }
    Ok((!edited.is_empty()).then_some((fields, edited)))
}

/// The fields of the user's own event as the edit form starts from, and
/// of its series, if it is an occurrence of one the calendar has.
pub(crate) fn editable(
    tx: &Transaction,
    id: EventId,
) -> Result<(EventFields, Option<EventFields>)> {
    let own = match overlay(tx, id)? {
        Some((fields, _)) => fields,
        None => state(tx, Target::Event(id))?.context("no event")?.shown()?,
    };
    let series_id: Option<i64> = tx.query_row(
        "SELECT series_id FROM calendar_event WHERE id = ?1",
        [id.0],
        |r| r.get(0),
    )?;
    let series = match series_id {
        None => None,
        Some(series) => {
            let state = state(tx, Target::Series(series))?.context("no series")?;
            match (&state.raw, &state.desired) {
                (None, None | Some(Desired::Update { .. } | Desired::Delete)) => None,
                _ => Some(state.shown()?),
            }
        }
    };
    Ok((own, series))
}

/// Whether the user can edit an event the provider has answered
/// `response` (as shown): the user's own, not a meeting request.
pub fn own(response: EventResponse) -> bool {
    matches!(response, EventResponse::Organizer | EventResponse::None)
}

// ── ops ───────────────────────────────────────────────────────────────────

fn changed(account: &str) -> Vec<ServerEvent> {
    vec![ServerEvent::CalendarChanged(CalendarChanged {
        account: account.to_owned(),
    })]
}

/// A transaction id for Graph, so that creating an event again, its
/// outcome unknown, creates nothing more: random, as it is the mailbox's.
fn transaction_id() -> String {
    let random: [u8; 16] = rand::random();
    random.iter().map(|b| format!("{b:02x}")).collect()
}

/// Creates the event (or series) at once, for the sync to give the
/// provider; returns what shows it.
fn create(
    tx: &Transaction,
    account: &str,
    fields: &EventFields,
    time_zone: &str,
) -> Result<(EventId, Target)> {
    let kind: Option<String> = tx
        .query_row("SELECT kind FROM account WHERE id = ?1", [account], |r| {
            r.get(0)
        })
        .optional()?;
    ensure!(
        kind.as_deref() == Some(uplink_email::m365::sync::KIND),
        "{account} has no calendar"
    );
    let desired = serde_json::to_string(&Desired::Create {
        fields: fields.clone(),
        edited: BTreeSet::new(),
        time_zone: time_zone.to_owned(),
    })?;
    let by = this_entry(tx)?.0;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis() as i64;
    let series = match fields.recurrence {
        None => {
            tx.execute(
                "INSERT INTO calendar_event (account_id, transaction_id, exception, subject,
                   location, preview, start_ts, end_ts, all_day, cancelled, show_as, response,
                   modified_ts, desired, desired_by)
                 VALUES (?1, ?2, 0, '', '', '', 0, 0, 0, 0, '', 'organizer', ?3, ?4, ?5)",
                params![account, transaction_id(), now, desired, by],
            )?;
            None
        }
        Some(_) => {
            tx.execute(
                "INSERT INTO calendar_series (account_id, transaction_id, desired, desired_by)
                 VALUES (?1, ?2, ?3, ?4)",
                params![account, transaction_id(), desired, by],
            )?;
            let series = tx.last_insert_rowid();
            // Its first occurrence, shown until the provider has expanded
            // it.
            tx.execute(
                "INSERT INTO calendar_event (account_id, series_id, exception, subject,
                   location, preview, start_ts, end_ts, all_day, cancelled, show_as, response,
                   modified_ts)
                 VALUES (?1, ?2, 0, '', '', '', 0, 0, 0, 0, '', 'organizer', ?3)",
                params![account, series, now],
            )?;
            Some(series)
        }
    };
    let event = EventId(tx.last_insert_rowid());
    refresh(tx, event)?;
    Ok((event, series.map_or(Target::Event(event), Target::Series)))
}

/// Creates an event, or a series, on the account's calendar: shown at
/// once, its attendees invited as the provider has it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateEvent {
    pub account: String,
    pub fields: EventFields,
    pub time_zone: String,
}

impl Op for CreateEvent {
    const KIND: &'static str = "calendar.create_event";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let (event, target) = create(tx, &self.account, &self.fields, &self.time_zone)?;
        let mut events = changed(&self.account);
        crate::telling_outbox(tx, &self.account, &mut events, || Ok(()))?;
        Ok(Applied {
            description: format!("Created '{}'", self.fields.subject),
            inverse: Inverse::of(&DeleteEvent {
                event,
                series: matches!(target, Target::Series(_)),
            }),
            events,
        })
    }
}

/// What `event` (with `series`, its series) is the target of.
fn target_of(tx: &Transaction, event: EventId, series: bool) -> Result<Target> {
    let series_id: Option<Option<i64>> = tx
        .query_row(
            "SELECT series_id FROM calendar_event WHERE id = ?1",
            [event.0],
            |r| r.get(0),
        )
        .optional()?;
    let series_id =
        series_id.ok_or_else(|| Conflict("the event is no longer on the calendar".into()))?;
    Ok(match (series, series_id) {
        (false, _) => Target::Event(event),
        (true, Some(series)) => Target::Series(series),
        (true, None) => bail!("event {} is of no series", event.0),
    })
}

/// Changes the `edited` fields of the user's own event, or of its
/// series, to `fields`'; or, as an inverse, back. Its attendees are told
/// as the provider has it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateEvent {
    pub event: EventId,
    pub series: bool,
    pub fields: EventFields,
    pub edited: BTreeSet<EventField>,
    pub time_zone: String,
}

impl Op for UpdateEvent {
    const KIND: &'static str = "calendar.update_event";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let target = target_of(tx, self.event, self.series)?;
        let state = state(tx, target)?
            .ok_or_else(|| Conflict("the event is no longer on the calendar".into()))?;
        if state.desired == Some(Desired::Delete) {
            return Err(Conflict("the event is being deleted".into()).into());
        }
        if !self.series && state.raw.is_none() && state.desired.is_none() {
            bail!("event {} is shown of a series being created", self.event.0);
        }
        let before = state.shown()?;
        let mut after = before.clone();
        for field in &self.edited {
            set(&mut after, *field, &self.fields);
        }
        let desired = match &state.desired {
            Some(Desired::Create {
                edited, time_zone, ..
            }) => Some(Desired::Create {
                fields: after.clone(),
                edited: edited.union(&self.edited).copied().collect(),
                time_zone: time_zone.clone(),
            }),
            Some(Desired::Update { .. }) | None => {
                let observed = state.observed()?.context("a change of nothing")?;
                let mut edited = BTreeSet::new();
                let pending: BTreeSet<EventField> = match &state.desired {
                    Some(Desired::Update { edited, .. }) => edited.clone(),
                    _ => BTreeSet::new(),
                };
                for field in pending.union(&self.edited) {
                    if !same(*field, &after, &observed)? {
                        edited.insert(*field);
                    }
                }
                (!edited.is_empty()).then(|| Desired::Update {
                    fields: after.clone(),
                    edited,
                    time_zone: self.time_zone.clone(),
                })
            }
            Some(Desired::Delete) => unreachable!("refused above"),
        };
        let inverse = Inverse::of(&UpdateEvent {
            fields: before.clone(),
            ..self.clone()
        });
        let mut events = changed(&state.account);
        crate::telling_outbox(tx, &state.account, &mut events, || {
            desire(tx, target, desired.as_ref())?;
            for id in showing(tx, target)? {
                refresh(tx, id)?;
            }
            Ok(())
        })?;
        Ok(Applied {
            description: match self.series {
                false => format!("Changed '{}'", after.subject),
                true => format!("Changed the series '{}'", after.subject),
            },
            inverse,
            events,
        })
    }
}

/// Whether deleting what the provider has of `raw` cancels a meeting,
/// its attendees told.
fn cancels(raw: Option<&str>) -> Result<bool> {
    let Some(raw) = raw else { return Ok(false) };
    let event: GraphEvent = serde_json::from_str(raw).context("an event's JSON")?;
    Ok(!event.attendees.is_empty() && !event.is_cancelled)
}

/// Deletes the user's own event, or its series: at once, for the sync to
/// give the provider, which cancels a meeting (its attendees told). One
/// the provider does not have yet is just gone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteEvent {
    pub event: EventId,
    pub series: bool,
}

impl Op for DeleteEvent {
    const KIND: &'static str = "calendar.delete_event";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let target = target_of(tx, self.event, self.series)?;
        let state = state(tx, target)?
            .ok_or_else(|| Conflict("the event is no longer on the calendar".into()))?;
        if state.desired == Some(Desired::Delete) {
            return Err(Conflict("the event is being deleted".into()).into());
        }
        let shown = state.shown()?;
        let verb = if cancels(state.raw.as_deref())? {
            "Cancelled"
        } else {
            "Deleted"
        };
        let description = match self.series {
            false => format!("{verb} '{}'", shown.subject),
            true => format!("{verb} the series '{}'", shown.subject),
        };
        let time_zone = match &state.desired {
            Some(Desired::Create { time_zone, .. } | Desired::Update { time_zone, .. }) => {
                time_zone.clone()
            }
            Some(Desired::Delete) | None => "UTC".to_owned(),
        };
        let inverse = Inverse::of(&RestoreEvent {
            event: self.event,
            series: self.series,
            desired: state.desired.clone(),
            account: state.account.clone(),
            fields: shown,
            time_zone,
        });
        let mut events = changed(&state.account);
        crate::telling_outbox(tx, &state.account, &mut events, || {
            if state.external_id.is_none() {
                // The provider has nothing of it.
                gone(tx, target)
            } else {
                desire(tx, target, Some(&Desired::Delete))
            }
        })?;
        Ok(Applied {
            description,
            inverse,
            events,
        })
    }
}

/// Takes `target` off the calendar, with whatever shows it.
fn gone(tx: &Transaction, target: Target) -> Result<()> {
    match target {
        Target::Event(id) => {
            tx.execute("DELETE FROM calendar_event WHERE id = ?1", [id.0])?;
        }
        Target::Series(id) => {
            tx.execute("DELETE FROM calendar_event WHERE series_id = ?1", [id])?;
            tx.execute("DELETE FROM calendar_series WHERE id = ?1", [id])?;
        }
    }
    Ok(())
}

/// Takes a deletion back: the event as it was, if still there; else
/// (the provider has deleted it, or never had it) created anew, as it
/// was shown, its attendees invited again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreEvent {
    pub event: EventId,
    pub series: bool,
    /// What was asked of it before its deletion.
    desired: Option<Desired>,
    pub account: String,
    pub fields: EventFields,
    pub time_zone: String,
}

impl Op for RestoreEvent {
    const KIND: &'static str = "calendar.restore_event";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let found = target_of(tx, self.event, self.series)
            .ok()
            .map(|target| Ok::<_, anyhow::Error>((target, state(tx, target)?)))
            .transpose()?;
        let mut events = changed(&self.account);
        if let Some((target, Some(state))) = found {
            if state.desired == Some(Desired::Delete) {
                crate::telling_outbox(tx, &self.account, &mut events, || {
                    desire(tx, target, self.desired.as_ref())?;
                    for id in showing(tx, target)? {
                        refresh(tx, id)?;
                    }
                    Ok(())
                })?;
                return Ok(Applied {
                    description: format!("Restored '{}'", self.fields.subject),
                    inverse: Inverse::of(&DeleteEvent {
                        event: self.event,
                        series: self.series,
                    }),
                    events,
                });
            }
        }
        // An occurrence is created anew as an event of its own.
        let mut fields = self.fields.clone();
        if !self.series {
            fields.recurrence = None;
        }
        let (event, target) = crate::telling_outbox(tx, &self.account, &mut events, || {
            create(tx, &self.account, &fields, &self.time_zone)
        })?;
        Ok(Applied {
            description: format!("Created '{}' anew", self.fields.subject),
            inverse: Inverse::of(&DeleteEvent {
                event,
                series: matches!(target, Target::Series(_)),
            }),
            events,
        })
    }
}

// ── pushes ────────────────────────────────────────────────────────────────

/// A write for the account's calendar sync to give the provider.
#[derive(Debug, Clone)]
pub struct Write {
    pub target: Target,
    /// The op that asked for it.
    pub by: TxLogId,
    pub request: Request,
}

/// A request to Graph, below `/me`.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Create(Value),
    Patch(String, Value),
    Delete(String),
    Cancel(String),
}

/// The account's writes still to give, but those refused: those of
/// series first (an event of one asked of after), creations first.
pub fn wanted(tx: &Transaction, account: &str) -> Result<Vec<Write>> {
    let mut writes = vec![];
    for table in ["calendar_series", "calendar_event"] {
        let rows: Vec<(i64, i64)> = tx
            .prepare(&format!(
                "SELECT id, desired_by FROM {table}
                 WHERE account_id = ?1 AND desired IS NOT NULL AND desired_refused IS NULL
                 ORDER BY desired_by, id"
            ))?
            .query_map([account], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (id, by) in rows {
            let target = match table {
                "calendar_series" => Target::Series(id),
                _ => Target::Event(EventId(id)),
            };
            let state = state(tx, target)?.expect("listed");
            writes.push(Write {
                target,
                by: TxLogId(by),
                request: request(tx, target, &state)?,
            });
        }
    }
    writes.sort_by_key(|w| !matches!(w.request, Request::Create(_)));
    Ok(writes)
}

/// The request giving what is asked of `target`.
fn request(tx: &Transaction, target: Target, state: &State) -> Result<Request> {
    let path = || {
        format!(
            "/events/{}",
            state.external_id.as_deref().expect("at the provider")
        )
    };
    Ok(match state.desired.as_ref().expect("asked") {
        Desired::Create {
            fields, time_zone, ..
        } => {
            let transaction_id: String = tx.query_row(
                &format!(
                    "SELECT transaction_id FROM {} WHERE id = ?1",
                    target.table()
                ),
                [target.id()],
                |r| r.get(0),
            )?;
            let mut all: BTreeSet<EventField> = ALL_FIELDS.into_iter().collect();
            if fields.recurrence.is_none() {
                all.remove(&EventField::Recurrence);
            }
            let mut body = graph_body(fields, &all, &Zone::new(time_zone)?, vec![])?;
            body["transactionId"] = json!(transaction_id);
            Request::Create(body)
        }
        Desired::Update {
            fields,
            edited,
            time_zone,
        } => Request::Patch(
            path(),
            graph_body(
                fields,
                edited,
                &Zone::new(time_zone)?,
                resources(state.raw.as_deref())?,
            )?,
        ),
        Desired::Delete if cancels(state.raw.as_deref())? => Request::Cancel(path()),
        Desired::Delete => Request::Delete(path()),
    })
}

/// What the provider made of a write.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Done: its JSON, as the provider has it now.
    Written(#[serde(skip)] String),
    /// Deleted; or it was gone.
    Gone,
    Refused(String),
}

/// The provider has a write asked by the op `by` done, or refused it.
#[derive(Debug, Serialize)]
pub struct EventWritten {
    pub target: Target,
    pub by: TxLogId,
    pub outcome: Outcome,
}

impl Observation for EventWritten {
    const KIND: &'static str = "calendar.event_written";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let state = state(tx, self.target)?;
        let mut events = changed(account);
        let Some(state) = state else {
            // Gone since, by a sync: what was asked went with it.
            return Ok(Observed {
                description: "Calendar: written, though gone since".to_owned(),
                events,
            });
        };
        let subject = state.shown().map(|f| f.subject).unwrap_or_default();
        let still = state.desired.is_some()
            && tx.query_row(
                &format!(
                    "SELECT desired_by = ?2 FROM {} WHERE id = ?1",
                    self.target.table()
                ),
                params![self.target.id(), self.by.0],
                |r| r.get::<_, bool>(0),
            )?;
        let description = crate::telling_outbox(tx, account, &mut events, || {
            Ok(match &self.outcome {
                Outcome::Refused(reason) => {
                    if still {
                        tx.execute(
                            &format!(
                                "UPDATE {} SET desired_refused = ?2 WHERE id = ?1",
                                self.target.table()
                            ),
                            params![self.target.id(), reason],
                        )?;
                    }
                    format!("The provider refused to write '{subject}': {reason}")
                }
                Outcome::Gone => {
                    gone(tx, self.target)?;
                    format!("'{subject}' is off the calendar at the provider")
                }
                Outcome::Written(raw) => {
                    let created = state.external_id.is_none();
                    written(tx, self.target, &state, raw, still)?;
                    match created {
                        true => format!("Created '{subject}' at the provider"),
                        false => format!("Changed '{subject}' at the provider"),
                    }
                }
            })
        })?;
        Ok(Observed {
            description,
            events,
        })
    }
}

/// Records the provider's JSON of `target`, as written: what was asked
/// is given, unless asked otherwise since (`still` not), which stays
/// pending as far as it differs.
fn written(tx: &Transaction, target: Target, state: &State, raw: &str, still: bool) -> Result<()> {
    let json: Value = serde_json::from_str(raw).context("an event's JSON")?;
    let external_id = json["id"].as_str().context("an event without an id")?;
    let desired = match (&state.desired, still) {
        (_, true) | (None, _) => None,
        (Some(Desired::Delete), false) => Some(Desired::Delete),
        (
            Some(
                Desired::Create {
                    fields,
                    edited,
                    time_zone,
                }
                | Desired::Update {
                    fields,
                    edited,
                    time_zone,
                },
            ),
            false,
        ) => {
            let observed = observed(raw)?;
            let mut pending = BTreeSet::new();
            for field in edited {
                if !same(*field, fields, &observed)? {
                    pending.insert(*field);
                }
            }
            (!pending.is_empty()).then(|| Desired::Update {
                fields: fields.clone(),
                edited: pending,
                time_zone: time_zone.clone(),
            })
        }
    };
    let by: Option<i64> = tx.query_row(
        &format!("SELECT desired_by FROM {} WHERE id = ?1", target.table()),
        [target.id()],
        |r| r.get(0),
    )?;
    tx.execute(
        &format!(
            "UPDATE {} SET external_id = ?2, raw = ?3, desired = ?4, desired_by = ?5,
               desired_refused = NULL
             WHERE id = ?1",
            target.table()
        ),
        params![
            target.id(),
            external_id,
            raw,
            desired.as_ref().map(serde_json::to_string).transpose()?,
            desired.is_some().then_some(by).flatten(),
        ],
    )?;
    match target {
        Target::Event(id) => refresh(tx, id)?,
        Target::Series(series) => {
            // Its first occurrence was shown until the provider expanded
            // it: the sync lists its occurrences next.
            tx.execute(
                "DELETE FROM calendar_event WHERE series_id = ?1 AND external_id IS NULL",
                [series],
            )?;
            for id in showing(tx, target)? {
                refresh(tx, id)?;
            }
        }
    }
    Ok(())
}

/// A sync found `target`, being created, at the provider, as `raw`: the
/// outcome of creating it was unknown. As written, but for what was asked
/// since.
pub(crate) fn adopt(tx: &Transaction, target: Target, raw: &str) -> Result<()> {
    let state = state(tx, target)?.expect("being created");
    written(tx, target, &state, raw, false)
}

// ── the outbox ────────────────────────────────────────────────────────────

/// A write pending, as the outbox compares them: its table, row, what is
/// asked, and why it was refused.
pub(crate) type PendingWrite = (String, i64, String, Option<String>);

/// The account's writes pending, as the outbox compares them.
pub(crate) fn pending_of(tx: &Transaction, account: &str) -> Result<Vec<PendingWrite>> {
    let mut pending = vec![];
    for table in ["calendar_series", "calendar_event"] {
        pending.extend(
            tx.prepare(&format!(
                "SELECT '{table}', id, desired, desired_refused FROM {table}
                 WHERE account_id = ?1 AND desired IS NOT NULL ORDER BY id"
            ))?
            .query_map([account], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(pending)
}

/// Each account's writes pending, newest first.
pub(crate) fn outbox(tx: &Transaction) -> Result<Vec<(String, PendingEventChange)>> {
    let mut changes = vec![];
    for table in ["calendar_series", "calendar_event"] {
        let rows: Vec<(String, i64, Option<String>, PendingOp)> = tx
            .prepare(&format!(
                "SELECT x.account_id, x.id, x.desired_refused, t.id, t.description, t.ts
                 FROM {table} x JOIN tx_log t ON t.id = x.desired_by"
            ))?
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    PendingOp {
                        id: TxLogId(r.get(3)?),
                        description: r.get(4)?,
                        ts: r.get(5)?,
                    },
                ))
            })?
            .collect::<Result<_, _>>()?;
        for (account, id, refused, op) in rows {
            let target = match table {
                "calendar_series" => Target::Series(id),
                _ => Target::Event(EventId(id)),
            };
            let state = state(tx, target)?.expect("listed");
            let desired = state.desired.as_ref().expect("asked");
            let shows: Option<i64> = match target {
                Target::Event(id) => Some(id.0),
                Target::Series(series) => tx.query_row(
                    "SELECT min(id) FROM calendar_event WHERE series_id = ?1",
                    [series],
                    |r| r.get(0),
                )?,
            };
            changes.push((
                account,
                PendingEventChange {
                    event: (*desired != Desired::Delete)
                        .then_some(shows)
                        .flatten()
                        .map(EventId),
                    subject: state.shown()?.subject,
                    change: desired.kind(),
                    series: matches!(target, Target::Series(_)),
                    op,
                    refused,
                },
            ));
        }
    }
    changes.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.op.id.cmp(&a.1.op.id)));
    Ok(changes)
}
