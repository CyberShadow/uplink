//! The calendars of Microsoft 365 accounts, read-only (SPEC §10): each
//! account's synced from a Graph calendar view by its sync task, and
//! served to clients.

pub mod details;
pub mod service;
pub mod sync;
#[cfg(test)]
mod tests;

use anyhow::{ensure, Context, Result};
use serde::Serialize;
use uplink_protocol::{
    calendar::{CalendarChanged, CalendarEvent, EventId, EventResponse, ShowAs},
    ServerEvent,
};
use uplink_query::{text_matches, Kind, Query, Term};
use uplink_storage::rusqlite::{params, Transaction};
use uplink_tx_log::{Observation, Observed};

/// The state of an account's calendar, or of what changed in it, as just
/// fetched. Recorded by the account's calendar sync, the only writer of
/// its events.
#[derive(Debug, Serialize)]
pub struct CalendarObserved {
    /// `account.kind`, for an account observed for the first time.
    pub account_kind: &'static str,
    /// `events` are all there is (a calendar view listed anew), rather
    /// than what changed.
    pub replace: bool,
    /// Events new or changed.
    pub events: Vec<ObservedEvent>,
    /// Events gone, by external id.
    pub removed: Vec<String>,
    /// Replaces the calendar's sync state.
    pub sync_state: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObservedEvent {
    pub external_id: String,
    /// The series it is an occurrence of.
    pub series: Option<String>,
    pub exception: bool,
    pub subject: String,
    pub location: String,
    pub preview: String,
    /// Ms since the epoch.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub cancelled: bool,
    pub show_as: ShowAs,
    pub response: EventResponse,
    pub join_url: Option<String>,
    /// The provider's JSON of it; not in the log, which records what
    /// changed rather than the provider's every word.
    #[serde(skip)]
    pub raw: String,
}

impl Observation for CalendarObserved {
    const KIND: &'static str = "calendar.observed";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        tx.execute(
            "INSERT INTO account (id, kind) VALUES (?1, ?2) ON CONFLICT (id) DO NOTHING",
            [account, self.account_kind],
        )?;
        let kind: String =
            tx.query_row("SELECT kind FROM account WHERE id = ?1", [account], |r| {
                r.get(0)
            })?;
        ensure!(
            kind == self.account_kind,
            "account {account} is recorded as {kind}, not {}",
            self.account_kind
        );
        if self.replace {
            tx.execute(
                "DELETE FROM calendar_event WHERE account_id = ?1",
                [account],
            )?;
        }
        for event in &self.events {
            tx.execute(
                "INSERT INTO calendar_event
                   (account_id, external_id, series, exception, subject, location, preview,
                    start_ts, end_ts, all_day, cancelled, show_as, response, join_url, raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT (account_id, external_id) DO UPDATE SET
                   series = excluded.series, exception = excluded.exception,
                   subject = excluded.subject, location = excluded.location,
                   preview = excluded.preview, start_ts = excluded.start_ts,
                   end_ts = excluded.end_ts, all_day = excluded.all_day,
                   cancelled = excluded.cancelled, show_as = excluded.show_as,
                   response = excluded.response, join_url = excluded.join_url,
                   raw = excluded.raw",
                params![
                    account,
                    event.external_id,
                    event.series,
                    event.exception,
                    event.subject,
                    event.location,
                    event.preview,
                    event.start,
                    event.end,
                    event.all_day,
                    event.cancelled,
                    name(event.show_as),
                    name(event.response),
                    event.join_url,
                    event.raw,
                ],
            )?;
        }
        let mut removed = 0;
        for external_id in &self.removed {
            // Graph also reports as removed what it never listed: events
            // outside the window that changed.
            removed += tx.execute(
                "DELETE FROM calendar_event WHERE account_id = ?1 AND external_id = ?2",
                params![account, external_id],
            )?;
        }
        tx.execute(
            "INSERT INTO calendar_sync (account_id, state) VALUES (?1, ?2)
             ON CONFLICT (account_id) DO UPDATE SET state = excluded.state",
            params![account, self.sync_state.to_string()],
        )?;
        let description = if self.replace {
            format!("Calendar listed: {}", events(self.events.len()))
        } else {
            let mut parts = vec![];
            if !self.events.is_empty() {
                parts.push(format!("{} changed", events(self.events.len())));
            }
            if removed > 0 {
                parts.push(format!("{} removed", events(removed)));
            }
            if parts.is_empty() {
                "Calendar synced".to_owned()
            } else {
                format!("Calendar: {}", parts.join(", "))
            }
        };
        Ok(Observed {
            description,
            events: vec![ServerEvent::CalendarChanged(CalendarChanged {
                account: account.to_owned(),
            })],
        })
    }
}

fn events(count: usize) -> String {
    match count {
        1 => "1 event".to_owned(),
        n => format!("{n} events"),
    }
}

/// The events `query` matches (SPEC §8.7), by subject, location and
/// preview, account and start: those yet to end first, soonest first,
/// then those past, latest first.
pub fn matching(tx: &Transaction, query: &Query, now: i64) -> Result<Vec<CalendarEvent>> {
    let all = events_between(tx, i64::MIN, i64::MAX)?;
    let (mut coming, mut past): (Vec<_>, Vec<_>) = all
        .into_iter()
        .filter(|event| uplink_query::matches(query, &|term| term_matches(event, term)))
        .partition(|event| event.end > now);
    past.reverse();
    coming.append(&mut past);
    Ok(coming)
}

fn term_matches(event: &CalendarEvent, term: &Term) -> bool {
    match term {
        Term::Text {
            fields: None,
            text,
            exact,
        } => [&event.subject, &event.location, &event.preview]
            .iter()
            .any(|field| text_matches(field, text, *exact)),
        Term::Kind(kind) => *kind == Kind::Event,
        Term::Account(account) => event.account == *account,
        // Of everything, as Trash and Spam are not of events.
        Term::Anywhere => true,
        Term::After(at) => event.start >= *at,
        Term::Before(at) => event.start < *at,
        // Of other kinds.
        Term::Text {
            fields: Some(_), ..
        }
        | Term::Me(_)
        | Term::Label(_)
        | Term::HasAttachment
        | Term::Larger(_)
        | Term::Smaller(_)
        | Term::MessageId(_)
        | Term::Unread
        | Term::Mention
        | Term::Notifying
        | Term::Direct
        | Term::Invited
        | Term::Space(_)
        | Term::Due(_)
        | Term::Overdue
        | Term::HasDue
        | Term::Done => false,
    }
}

/// A provider's name of a value, as the protocol has it.
fn name<T: Serialize>(value: T) -> String {
    match serde_json::to_value(value).expect("serializes") {
        serde_json::Value::String(name) => name,
        other => unreachable!("{other} is not a name"),
    }
}

/// The value of a name [`name`] gave.
fn named<T: serde::de::DeserializeOwned>(name: String) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(name.clone()))
        .with_context(|| format!("{name} names no value"))
}

const EVENT_COLUMNS: &str = "id, account_id, subject, location, preview, start_ts, end_ts, all_day,
     series IS NOT NULL, exception, cancelled, show_as, response, join_url";

fn event_of(r: &uplink_storage::rusqlite::Row) -> Result<CalendarEvent> {
    Ok(CalendarEvent {
        id: EventId(r.get(0)?),
        account: r.get(1)?,
        subject: r.get(2)?,
        location: r.get(3)?,
        preview: r.get(4)?,
        start: r.get(5)?,
        end: r.get(6)?,
        all_day: r.get(7)?,
        recurring: r.get(8)?,
        exception: r.get(9)?,
        cancelled: r.get(10)?,
        show_as: named(r.get(11)?)?,
        response: named(r.get(12)?)?,
        join_url: r.get(13)?,
    })
}

/// The events overlapping `from`–`to` (ms), by start.
pub fn events_between(tx: &Transaction, from: i64, to: i64) -> Result<Vec<CalendarEvent>> {
    let mut statement = tx.prepare(&format!(
        "SELECT {EVENT_COLUMNS}
         FROM calendar_event
         WHERE start_ts < ?2 AND (end_ts > ?1 OR (end_ts = start_ts AND start_ts >= ?1))
         ORDER BY start_ts, id"
    ))?;
    let mut rows = statement.query([from, to])?;
    let mut events = vec![];
    while let Some(row) = rows.next()? {
        events.push(event_of(row)?);
    }
    Ok(events)
}

/// The event, and its provider's JSON; `None` if there is none of `id`.
pub fn event(tx: &Transaction, id: EventId) -> Result<Option<(CalendarEvent, String)>> {
    let mut statement = tx.prepare(&format!(
        "SELECT {EVENT_COLUMNS}, raw FROM calendar_event WHERE id = ?1"
    ))?;
    let mut rows = statement.query([id.0])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some((event_of(row)?, row.get(14)?)))
}
