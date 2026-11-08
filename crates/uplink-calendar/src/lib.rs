//! The calendars of Microsoft 365 accounts, read-only (SPEC §10): each
//! account's synced from a Graph calendar view by its sync task, and
//! served to clients.

pub mod service;
pub mod sync;
#[cfg(test)]
mod tests;

use anyhow::{ensure, Result};
use serde::Serialize;
use uplink_protocol::{
    calendar::{CalendarChanged, CalendarEvent, EventId},
    ServerEvent,
};
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
            tx.execute("DELETE FROM calendar_event WHERE account_id = ?1", [account])?;
        }
        for event in &self.events {
            tx.execute(
                "INSERT INTO calendar_event
                   (account_id, external_id, series, exception, subject, location, preview,
                    start_ts, end_ts, all_day, raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (account_id, external_id) DO UPDATE SET
                   series = excluded.series, exception = excluded.exception,
                   subject = excluded.subject, location = excluded.location,
                   preview = excluded.preview, start_ts = excluded.start_ts,
                   end_ts = excluded.end_ts, all_day = excluded.all_day, raw = excluded.raw",
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

/// The events overlapping `from`–`to` (ms), by start.
pub fn events_between(tx: &Transaction, from: i64, to: i64) -> Result<Vec<CalendarEvent>> {
    Ok(tx
        .prepare(
            "SELECT id, account_id, subject, location, preview, start_ts, end_ts, all_day,
                    series IS NOT NULL, exception
             FROM calendar_event
             WHERE start_ts < ?2 AND (end_ts > ?1 OR (end_ts = start_ts AND start_ts >= ?1))
             ORDER BY start_ts, id",
        )?
        .query_map([from, to], |r| {
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
            })
        })?
        .collect::<Result<_, _>>()?)
}
