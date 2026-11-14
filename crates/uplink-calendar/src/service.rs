//! Answers the calendar requests of the WebSocket protocol (SPEC §5.4).

use crate::{
    respond::{answerable, Respond},
    write::{self, CreateEvent, DeleteEvent, UpdateEvent},
    zones::Zone,
};
use arc_swap::ArcSwap;
use std::{collections::BTreeSet, sync::Arc};
use uplink_protocol::{
    calendar::{
        EventField, EventFields, EventId, EventResponse, Recurrence, RecurrenceEnd,
        RecurrencePattern, TimeZone,
    },
    ErrorCode, ServerResponse,
};
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::{
    answer::{error, internal, Reader, Response},
    Dispatcher,
};

/// The longest stretch one request may list.
const MAX_RANGE_MS: i64 = 400 * 86_400_000;

#[derive(Clone)]
pub struct Calendar {
    dispatcher: Dispatcher,
    reader: Reader,
    time_zones: Arc<ArcSwap<Vec<TimeZone>>>,
}

impl Calendar {
    /// `reader` is a read-only connection to the database.
    pub fn new(dispatcher: Dispatcher, reader: Connection, time_zones: Vec<TimeZone>) -> Self {
        Self {
            dispatcher,
            reader: Reader::new(reader),
            time_zones: Arc::new(ArcSwap::from_pointee(time_zones)),
        }
    }

    pub fn configure(&self, time_zones: Vec<TimeZone>) {
        self.time_zones.store(Arc::new(time_zones));
    }

    pub async fn list(&self, from: i64, to: i64) -> Response {
        if from >= to || to - from > MAX_RANGE_MS {
            return Err(error(
                ErrorCode::Invalid,
                format!("{from}–{to} is not a range of at most 400 days"),
            ));
        }
        let time_zones = Vec::clone(&self.time_zones.load());
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let events = crate::events_between(&tx, from, to).map_err(internal)?;
                Ok(ServerResponse::Events {
                    as_of,
                    events,
                    time_zones,
                })
            })
            .await
    }

    pub async fn get(&self, id: EventId) -> Response {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let (event, raw) = crate::event(&tx, id)
                    .map_err(internal)?
                    .ok_or_else(|| error(ErrorCode::NotFound, format!("no event {}", id.0)))?;
                let shown = write::overlay(&tx, id).map_err(internal)?;
                let editable = (write::own(event.response) && !event.cancelled)
                    .then(|| write::editable(&tx, id))
                    .transpose()
                    .map_err(internal)?;
                let details = crate::details::details(event, raw.as_deref(), shown, editable)
                    .map_err(internal)?;
                Ok(ServerResponse::Event {
                    as_of,
                    details: Box::new(details),
                })
            })
            .await
    }

    /// Answers the meeting request `event` as `response`.
    pub async fn respond(&self, event: EventId, response: EventResponse) -> Response {
        if crate::respond::action(response).is_none() {
            return Err(error(
                ErrorCode::Invalid,
                format!("{response:?} is not an answer to give"),
            ));
        }
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let (found, _) = crate::event(&tx, event)
                    .map_err(internal)?
                    .ok_or_else(|| error(ErrorCode::NotFound, format!("no event {}", event.0)))?;
                // As shown: an answer asked for is not of the organizer.
                if !answerable(found.response) {
                    return Err(error(
                        ErrorCode::Invalid,
                        format!("'{}' is not a meeting request to answer", found.subject),
                    ));
                }
                Ok(())
            })
            .await?;
        self.dispatcher.answer(Respond { event, response }).await
    }

    /// Creates an event, or a series, on `account`'s calendar.
    pub async fn create(
        &self,
        account: String,
        fields: EventFields,
        time_zone: String,
    ) -> Response {
        valid(&fields, &time_zone, fields.recurrence.is_some())?;
        self.dispatcher
            .answer(CreateEvent {
                account,
                fields,
                time_zone,
            })
            .await
    }

    /// Changes the `edited` fields of the user's own `event`, or of its
    /// series, to `fields`'.
    pub async fn update(
        &self,
        event: EventId,
        series: bool,
        fields: EventFields,
        edited: Vec<EventField>,
        time_zone: String,
    ) -> Response {
        valid(&fields, &time_zone, series)?;
        if edited.is_empty() {
            return Err(error(ErrorCode::Invalid, "nothing edited".into()));
        }
        if edited.contains(&EventField::Recurrence) && !series {
            return Err(error(
                ErrorCode::Invalid,
                "only a series recurs: an occurrence or an event does not".into(),
            ));
        }
        self.own(event, series).await?;
        self.dispatcher
            .answer(UpdateEvent {
                event,
                series,
                fields,
                edited: edited.into_iter().collect::<BTreeSet<_>>(),
                time_zone,
            })
            .await
    }

    /// Deletes the user's own `event`, or its series.
    pub async fn delete(&self, event: EventId, series: bool) -> Response {
        self.own(event, series).await?;
        self.dispatcher.answer(DeleteEvent { event, series }).await
    }

    /// Fails unless `event` is the user's own, to edit: as shown, and of a
    /// series the calendar has if `series`.
    async fn own(&self, event: EventId, series: bool) -> Result<(), uplink_protocol::ServerError> {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let (found, _) = crate::event(&tx, event)
                    .map_err(internal)?
                    .ok_or_else(|| error(ErrorCode::NotFound, format!("no event {}", event.0)))?;
                if !write::own(found.response) || found.cancelled {
                    return Err(error(
                        ErrorCode::Invalid,
                        format!(
                            "'{}' is not an event of the user's own to edit",
                            found.subject
                        ),
                    ));
                }
                let (_, of_series) = write::editable(&tx, event).map_err(internal)?;
                if series && of_series.is_none() {
                    return Err(error(
                        ErrorCode::Invalid,
                        format!("'{}' is of no series the calendar has", found.subject),
                    ));
                }
                Ok(())
            })
            .await
    }
}

/// Fails unless `fields` make an event (a series, if `series`) of a time
/// zone Graph knows.
fn valid(
    fields: &EventFields,
    time_zone: &str,
    series: bool,
) -> Result<(), uplink_protocol::ServerError> {
    let invalid = |message: String| error(ErrorCode::Invalid, message);
    Zone::new(time_zone).map_err(|e| invalid(format!("{e:#}")))?;
    if fields.end < fields.start {
        return Err(invalid("an event ends before it starts".into()));
    }
    const DAY: i64 = 86_400_000;
    if fields.all_day
        && (fields.start % DAY != 0 || fields.end % DAY != 0 || fields.end == fields.start)
    {
        return Err(invalid("an all-day event is of whole days".into()));
    }
    for (list, which) in [
        (&fields.required, "required"),
        (&fields.optional, "optional"),
    ] {
        uplink_email::compose::parse_addresses(list)
            .map_err(|e| invalid(format!("{which} attendees: {e:#}")))?;
    }
    if fields.reminder.is_some_and(|m| m < 0) {
        return Err(invalid("a reminder after the start".into()));
    }
    match (&fields.recurrence, series) {
        (Some(recurrence), true) => recurs(recurrence).map_err(invalid),
        (None, false) => Ok(()),
        (Some(_), false) => Err(invalid("only a series recurs".into())),
        (None, true) => Err(invalid("a series recurs".into())),
    }
}

fn recurs(recurrence: &Recurrence) -> Result<(), String> {
    if recurrence.interval == 0 {
        return Err("a recurrence every 0".into());
    }
    let (day, month, days) = match &recurrence.pattern {
        RecurrencePattern::Daily => (None, None, None),
        RecurrencePattern::Weekly { days } => (None, None, Some(days)),
        RecurrencePattern::AbsoluteMonthly { day } => (Some(*day), None, None),
        RecurrencePattern::RelativeMonthly { days, .. } => (None, None, Some(days)),
        RecurrencePattern::AbsoluteYearly { month, day } => (Some(*day), Some(*month), None),
        RecurrencePattern::RelativeYearly { month, days, .. } => (None, Some(*month), Some(days)),
    };
    if day.is_some_and(|d| !(1..=31).contains(&d)) || month.is_some_and(|m| !(1..=12).contains(&m))
    {
        return Err("a recurrence on no day".into());
    }
    if days.is_some_and(|d| d.is_empty()) {
        return Err("a recurrence on no day of the week".into());
    }
    match &recurrence.until {
        RecurrenceEnd::After { count: 0 } => Err("a recurrence of no occurrences".into()),
        RecurrenceEnd::On { date } if jiff::civil::Date::strptime("%Y-%m-%d", date).is_err() => {
            Err(format!("{date} is not a date"))
        }
        _ => Ok(()),
    }
}
