//! The calendars of Microsoft 365 accounts (SPEC §10): each account's
//! synced from a Graph calendar view by its sync task, and served to
//! clients; meeting requests answered (§10.3); the user's own events
//! created, changed and deleted (§10.4).

pub mod details;
pub mod respond;
pub mod service;
pub mod sync;
#[cfg(test)]
mod tests;
pub mod write;
pub mod zones;

use anyhow::{ensure, Context, Result};
use serde::Serialize;
use uplink_protocol::{
    calendar::{CalendarChanged, CalendarEvent, EventId, EventResponse, ShowAs},
    compose::{AccountOutbox, OutboxChanged, Pending},
    ServerEvent,
};
use uplink_query::{text_matches, Kind, Query, Term};
use uplink_storage::rusqlite::{params, OptionalExtension, Transaction};
use uplink_tx_log::{Observation, Observed, Registry};

pub fn register(registry: &mut Registry) {
    registry.register::<respond::Respond>();
    registry.register::<write::CreateEvent>();
    registry.register::<write::UpdateEvent>();
    registry.register::<write::DeleteEvent>();
    registry.register::<write::RestoreEvent>();
}

/// What a change to the account's calendar did to what it has pending:
/// run `change`, then tell the outbox if that changed.
pub(crate) fn telling_outbox<T>(
    tx: &Transaction,
    account: &str,
    events: &mut Vec<ServerEvent>,
    change: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let pending = || -> Result<_> {
        Ok((
            respond::pending_of(tx, account)?,
            write::pending_of(tx, account)?,
        ))
    };
    let before = pending()?;
    let result = change()?;
    if pending()? != before {
        events.push(ServerEvent::OutboxChanged(OutboxChanged {
            accounts: vec![account.to_owned()],
        }));
    }
    Ok(result)
}

/// Each account with answers being given or events being written, by
/// account.
pub fn outbox(tx: &Transaction) -> Result<Vec<AccountOutbox>> {
    fn of(accounts: &mut Vec<AccountOutbox>, account: String) -> &mut Pending {
        let at = match accounts.iter().position(|a| a.account == account) {
            Some(at) => at,
            None => {
                accounts.push(AccountOutbox {
                    account,
                    failing: None,
                    pending: Pending::Calendar {
                        responses: vec![],
                        changes: vec![],
                    },
                });
                accounts.len() - 1
            }
        };
        &mut accounts[at].pending
    }
    let mut accounts: Vec<AccountOutbox> = vec![];
    for (account, response) in respond::outbox(tx)? {
        let Pending::Calendar { responses, .. } = of(&mut accounts, account) else {
            unreachable!("of the calendar")
        };
        responses.push(response);
    }
    for (account, change) in write::outbox(tx)? {
        let Pending::Calendar { changes, .. } = of(&mut accounts, account) else {
            unreachable!("of the calendar")
        };
        changes.push(change);
    }
    accounts.sort_by(|a, b| a.account.cmp(&b.account));
    Ok(accounts)
}

/// The state of an account's calendar, or of what changed in it, as just
/// fetched. Recorded by the account's calendar sync, the only writer of
/// its events but for the answers asked for (§6.6): an answer pending
/// prevails until the provider has it, and goes with its event.
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
    /// The masters of the series whose occurrences changed (calendar views
    /// list occurrences only), as fetched.
    pub series: Vec<ObservedSeries>,
    /// Replaces the calendar's sync state.
    pub sync_state: serde_json::Value,
}

/// A series' master, as Graph has it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObservedSeries {
    pub external_id: String,
    /// Of one uplink created, the transaction id it gave.
    pub transaction_id: Option<String>,
    /// Not in the log, as an event's.
    #[serde(skip)]
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObservedEvent {
    pub external_id: String,
    /// Of one uplink created, the transaction id it gave.
    pub transaction_id: Option<String>,
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
    pub reminder: Option<i64>,
    /// When it last changed, as the provider says: ms since the epoch.
    pub modified: i64,
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
        let mut events = vec![ServerEvent::CalendarChanged(CalendarChanged {
            account: account.to_owned(),
        })];
        let removed = telling_outbox(tx, account, &mut events, || self.record(account, tx))?;
        let description = if self.replace {
            format!("Calendar listed: {}", count(self.events.len()))
        } else {
            let mut parts = vec![];
            if !self.events.is_empty() {
                parts.push(format!("{} changed", count(self.events.len())));
            }
            if removed > 0 {
                parts.push(format!("{} removed", count(removed)));
            }
            if parts.is_empty() {
                "Calendar synced".to_owned()
            } else {
                format!("Calendar: {}", parts.join(", "))
            }
        };
        Ok(Observed {
            description,
            events,
        })
    }
}

impl CalendarObserved {
    /// Records the events; returns how many of those removed were stored.
    fn record(&self, account: &str, tx: &Transaction) -> Result<usize> {
        for series in &self.series {
            // One uplink created, its outcome unknown: the provider has it.
            if let Some(found) = being_created(
                tx,
                "calendar_series",
                account,
                series.transaction_id.as_deref(),
            )? {
                write::adopt(tx, write::Target::Series(found), &series.raw)?;
            }
            tx.execute(
                "INSERT INTO calendar_series (account_id, external_id, raw) VALUES (?1, ?2, ?3)
                 ON CONFLICT (account_id, external_id) DO UPDATE SET raw = excluded.raw",
                params![account, series.external_id, series.raw],
            )?;
        }
        if self.replace {
            // Those listed again stay, so that they keep their ids; so do
            // those being created, which the provider does not have yet.
            let listed = serde_json::to_string(
                &self
                    .events
                    .iter()
                    .map(|e| &e.external_id)
                    .collect::<Vec<_>>(),
            )?;
            tx.execute(
                "DELETE FROM calendar_event WHERE account_id = ?1 AND external_id IS NOT NULL
                   AND external_id NOT IN (SELECT value FROM json_each(?2))",
                params![account, listed],
            )?;
        }
        for event in &self.events {
            if let Some(found) = being_created(
                tx,
                "calendar_event",
                account,
                event.transaction_id.as_deref(),
            )? {
                write::adopt(tx, write::Target::Event(EventId(found)), &event.raw)?;
            }
            let series: Option<i64> = event
                .series
                .as_ref()
                .map(|series| {
                    tx.execute(
                        "INSERT INTO calendar_series (account_id, external_id) VALUES (?1, ?2)
                         ON CONFLICT (account_id, external_id) DO NOTHING",
                        params![account, series],
                    )?;
                    tx.query_row(
                        "SELECT id FROM calendar_series WHERE account_id = ?1 AND external_id = ?2",
                        params![account, series],
                        |r| r.get(0),
                    )
                })
                .transpose()?;
            tx.execute(
                "INSERT INTO calendar_event
                   (account_id, external_id, series_id, exception, subject, location, preview,
                    start_ts, end_ts, all_day, cancelled, show_as, response, join_url, reminder,
                    modified_ts, raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                 ON CONFLICT (account_id, external_id) DO UPDATE SET
                   series_id = excluded.series_id, exception = excluded.exception,
                   subject = excluded.subject, location = excluded.location,
                   preview = excluded.preview, start_ts = excluded.start_ts,
                   end_ts = excluded.end_ts, all_day = excluded.all_day,
                   cancelled = excluded.cancelled, show_as = excluded.show_as,
                   response = excluded.response, join_url = excluded.join_url,
                   reminder = excluded.reminder, modified_ts = excluded.modified_ts,
                   raw = excluded.raw,
                   -- An answer asked for, the provider has it now.
                   response_by = CASE WHEN desired_response = excluded.response
                                 THEN NULL ELSE response_by END,
                   response_refused = CASE WHEN desired_response = excluded.response
                                      THEN NULL ELSE response_refused END,
                   desired_response = CASE WHEN desired_response = excluded.response
                                      THEN NULL ELSE desired_response END",
                params![
                    account,
                    event.external_id,
                    series,
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
                    event.reminder,
                    event.modified,
                    event.raw,
                ],
            )?;
        }
        let mut removed = 0;
        for external_id in &self.removed {
            // Graph also reports as removed what it never listed: events
            // outside the window that changed. Those deleted take what is
            // asked of them along (§6.6).
            removed += tx.execute(
                "DELETE FROM calendar_event WHERE account_id = ?1 AND external_id = ?2",
                params![account, external_id],
            )?;
        }
        // What is asked of an event prevails, as shown, until the provider
        // has it.
        let changing: Vec<i64> = tx
            .prepare(
                "SELECT e.id FROM calendar_event e LEFT JOIN calendar_series s ON s.id = e.series_id
                 WHERE e.account_id = ?1 AND (e.desired IS NOT NULL OR s.desired IS NOT NULL)",
            )?
            .query_map([account], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for id in changing {
            write::refresh(tx, EventId(id))?;
        }
        // Series with nothing on the calendar, nor asked of.
        tx.execute(
            "DELETE FROM calendar_series WHERE account_id = ?1 AND desired IS NULL
               AND NOT EXISTS (SELECT 1 FROM calendar_event e WHERE e.series_id = calendar_series.id)",
            [account],
        )?;
        tx.execute(
            "INSERT INTO calendar_sync (account_id, state) VALUES (?1, ?2)
             ON CONFLICT (account_id) DO UPDATE SET state = excluded.state",
            params![account, self.sync_state.to_string()],
        )?;
        Ok(removed)
    }
}

/// The id of the account's event or series (of `table`) being created
/// with `transaction_id`, if there is one.
fn being_created(
    tx: &Transaction,
    table: &str,
    account: &str,
    transaction_id: Option<&str>,
) -> Result<Option<i64>> {
    let Some(transaction_id) = transaction_id else {
        return Ok(None);
    };
    Ok(tx
        .query_row(
            &format!(
                "SELECT id FROM {table}
                 WHERE account_id = ?1 AND transaction_id = ?2 AND external_id IS NULL"
            ),
            params![account, transaction_id],
            |r| r.get(0),
        )
        .optional()?)
}

fn count(count: usize) -> String {
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
        | Term::New
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
     series_id IS NOT NULL, exception, cancelled, show_as,
     coalesce(desired_response, response), join_url, reminder, modified_ts";

/// Of an event: it is being deleted, or its series is (SPEC §10.4).
const DELETING: &str = "'delete' IN (coalesce(json_extract(desired, '$.kind'), ''),
     coalesce((SELECT json_extract(s.desired, '$.kind') FROM calendar_series s
               WHERE s.id = series_id), ''))";

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
        reminder: r.get(14)?,
        modified: r.get(15)?,
    })
}

/// The events overlapping `from`–`to` (ms), by start; but those declined,
/// which Outlook does not show (SPEC §10.2), and those being deleted.
pub fn events_between(tx: &Transaction, from: i64, to: i64) -> Result<Vec<CalendarEvent>> {
    let mut statement = tx.prepare(&format!(
        "SELECT {EVENT_COLUMNS}
         FROM calendar_event
         WHERE start_ts < ?2 AND (end_ts > ?1 OR (end_ts = start_ts AND start_ts >= ?1))
           AND coalesce(desired_response, response) != 'declined' AND NOT {DELETING}
         ORDER BY start_ts, id"
    ))?;
    let mut rows = statement.query([from, to])?;
    let mut events = vec![];
    while let Some(row) = rows.next()? {
        events.push(event_of(row)?);
    }
    Ok(events)
}

/// The event, and its provider's JSON (`None` while being created);
/// `None` if there is none of `id`, or it is being deleted.
pub fn event(tx: &Transaction, id: EventId) -> Result<Option<(CalendarEvent, Option<String>)>> {
    let mut statement = tx.prepare(&format!(
        "SELECT {EVENT_COLUMNS}, raw FROM calendar_event WHERE id = ?1 AND NOT {DELETING}"
    ))?;
    let mut rows = statement.query([id.0])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some((event_of(row)?, row.get(16)?)))
}
