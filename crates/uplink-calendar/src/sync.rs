//! An account's calendar sync (SPEC §10.1): a Graph calendar view of a
//! window around today, kept up to date through its delta link.
//!
//! Graph expands series in a calendar view: it lists each occurrence, as
//! changed if it was, and leaves out those cancelled. A delta link holds
//! its window, so as days pass the window is moved by listing it anew.
//!
//! It is also the calendar's reconciler (§6.6): after each sync, and as
//! the user answers meeting requests and writes events, it gives Graph
//! the answers asked for (§10.3), and the events created, changed and
//! deleted (§10.4).

use crate::{
    respond::{self, ResponseGiven, CANNOT_TAKE_BACK},
    write::{self, EventWritten, Outcome, Request},
    CalendarObserved, ObservedEvent, ObservedSeries,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::sync::broadcast::{self, error::RecvError};
use uplink_config::M365AccountConfig;
use uplink_email::m365::api::{Changed, Graph, Written};
use uplink_protocol::{
    calendar::{EventResponse, ShowAs},
    LogEntry, ServerEvent, Source,
};
use uplink_storage::rusqlite::{Connection, OptionalExtension};
use uplink_tx_log::Dispatcher;

/// How often to poll for changes (SPEC §10.1).
pub const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DAY_MS: i64 = 86_400_000;
/// The window: from this long ago...
const PAST_MS: i64 = 31 * DAY_MS;
/// ...to this far ahead.
const FUTURE_MS: i64 = 366 * DAY_MS;
/// The window is moved once its start is this far behind where it would
/// be now.
const MOVE_AFTER_MS: i64 = 7 * DAY_MS;
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// `calendar_sync.state`: the window listed, and the delta link it is
/// recorded up to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncState {
    /// Ms since the epoch.
    start: i64,
    end: i64,
    delta_link: String,
}

pub struct CalendarSync {
    graph: Graph,
    account: String,
    dispatcher: Dispatcher,
    reader: Arc<Mutex<Connection>>,
    poll_interval: Duration,
}

/// The calendar sync of `config`'s account, through `graph`.
pub fn task(
    config: &M365AccountConfig,
    graph: Graph,
    dispatcher: Dispatcher,
    data_root: &Path,
) -> Result<CalendarSync> {
    Ok(CalendarSync::new(
        graph,
        config.id.clone(),
        dispatcher,
        uplink_storage::open_reader(data_root)?,
        POLL_INTERVAL,
    ))
}

/// A Graph event, as far as uplink reads it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    series_master_id: Option<String>,
    /// Only of one created with one.
    transaction_id: Option<String>,
    subject: Option<String>,
    body_preview: Option<String>,
    location: Option<Location>,
    start: DateTime,
    end: DateTime,
    #[serde(default)]
    is_all_day: bool,
    is_cancelled: bool,
    show_as: ShowAs,
    response_status: ResponseStatus,
    online_meeting: Option<OnlineMeeting>,
    /// Of online meetings of providers other than Teams.
    online_meeting_url: Option<String>,
    is_reminder_on: bool,
    reminder_minutes_before_start: i64,
    last_modified_date_time: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OnlineMeeting {
    join_url: Option<String>,
}

#[derive(Deserialize)]
struct ResponseStatus {
    response: EventResponse,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    display_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DateTime {
    date_time: String,
    time_zone: String,
}

impl DateTime {
    /// Ms since the epoch; times are asked for in UTC.
    pub(crate) fn ms(&self) -> Result<i64> {
        anyhow::ensure!(
            self.time_zone.eq_ignore_ascii_case("UTC"),
            "a time in {}, not UTC",
            self.time_zone
        );
        ms(&self.date_time)
    }
}

impl CalendarSync {
    pub fn new(
        graph: Graph,
        account: String,
        dispatcher: Dispatcher,
        reader: Connection,
        poll_interval: Duration,
    ) -> Self {
        Self {
            graph,
            account,
            dispatcher,
            reader: Arc::new(Mutex::new(reader)),
            poll_interval,
        }
    }

    /// Runs until the process exits, retrying failures with backoff; at
    /// once when the user changes the calendar.
    pub async fn run(self) {
        let mut wake = self.dispatcher.hub().subscribe();
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.step(now_ms()).await {
                Ok(()) => {
                    backoff = Duration::from_secs(1);
                    self.wait(&mut wake).await;
                }
                Err(e) => {
                    tracing::warn!(account = self.account, "calendar sync failed: {e:#}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    /// One round, as of `now` (ms): syncs, pushes, and syncs again if it
    /// wrote an event, so that what was written shows as the provider has
    /// it (a series created, its occurrences).
    pub async fn step(&self, now: i64) -> Result<()> {
        self.sync(now).await?;
        if self.push().await? {
            self.sync(now).await?;
        }
        Ok(())
    }

    /// Waits out the poll interval, or until the user changes the
    /// calendar.
    async fn wait(&self, wake: &mut broadcast::Receiver<Arc<LogEntry>>) {
        let deadline = tokio::time::sleep(self.poll_interval);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return,
                entry = wake.recv() => match entry {
                    Ok(entry) if entry.source == Source::User && changes_calendar(&entry) => return,
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => return,
                    Err(RecvError::Closed) => panic!("the hub outlives sync tasks"),
                },
            }
        }
    }

    /// Gives Graph the answers to meeting requests asked for, and the
    /// events written, recording what it made of each; whether it wrote
    /// any event.
    pub async fn push(&self) -> Result<bool> {
        let reader = self.reader.clone();
        let account = self.account.clone();
        let wanted = tokio::task::spawn_blocking(move || {
            respond::wanted(&reader.lock().expect("reader poisoned"), &account)
        })
        .await??;
        for wanted in wanted {
            let refused = match respond::action(wanted.response) {
                None => Some(CANNOT_TAKE_BACK.to_owned()),
                Some(action) => {
                    let path = format!("/events/{}/{action}", wanted.external_id);
                    let body = serde_json::json!({ "sendResponse": true });
                    match self.graph.act(&path, &body).await? {
                        Changed::Done => None,
                        Changed::Gone => Some("the event is no longer on the calendar".to_owned()),
                        Changed::Refused(reason) => Some(reason),
                    }
                }
            };
            self.dispatcher
                .observe(
                    self.account.clone(),
                    ResponseGiven {
                        event: wanted.event,
                        response: wanted.response,
                        refused,
                    },
                )
                .await?;
        }
        let reader = self.reader.clone();
        let account = self.account.clone();
        let writes = tokio::task::spawn_blocking(move || {
            let mut conn = reader.lock().expect("reader poisoned");
            let tx = conn.transaction()?;
            write::wanted(&tx, &account)
        })
        .await??;
        let wrote = !writes.is_empty();
        for write in writes {
            let outcome = match &write.request {
                Request::Create(body) => self.graph.create("/events", body).await?,
                Request::Patch(path, body) => self.graph.patch(path, body).await?,
                Request::Delete(path) => match self.graph.delete(path).await? {
                    Changed::Done | Changed::Gone => Written::Gone,
                    Changed::Refused(reason) => Written::Refused(reason),
                },
                Request::Cancel(path) => {
                    let body = serde_json::json!({ "comment": "" });
                    match self.graph.act(&format!("{path}/cancel"), &body).await? {
                        Changed::Done | Changed::Gone => Written::Gone,
                        Changed::Refused(reason) => Written::Refused(reason),
                    }
                }
            };
            let outcome = match outcome {
                Written::Done(json) => Outcome::Written(json.to_string()),
                // One to change gone is gone; one to create is not.
                Written::Gone => match write.request {
                    Request::Create(_) => bail!("Graph answered 404 to creating an event"),
                    _ => Outcome::Gone,
                },
                Written::Refused(reason) => Outcome::Refused(reason),
            };
            self.dispatcher
                .observe(
                    self.account.clone(),
                    EventWritten {
                        target: write.target,
                        by: write.by,
                        outcome,
                    },
                )
                .await?;
        }
        Ok(wrote)
    }

    /// Records the calendar's changes since the last sync, as of `now`
    /// (ms); all of its window, the first time or once the window moves.
    pub async fn sync(&self, now: i64) -> Result<()> {
        let state = self.state().await?;
        let fresh = || {
            let (start, end) = (now - PAST_MS, now + FUTURE_MS);
            (self.start_link(start, end), start, end, true)
        };
        let (mut url, mut start, mut end, mut full) = match state {
            Some(s) if now - PAST_MS - s.start < MOVE_AFTER_MS => {
                (s.delta_link, s.start, s.end, false)
            }
            Some(_) => {
                tracing::info!(account = self.account, "moving the calendar window");
                fresh()
            }
            None => fresh(),
        };
        let mut items: Vec<serde_json::Value> = vec![];
        let delta_link = loop {
            let Some(page) = self.graph.delta::<serde_json::Value>(&url).await? else {
                if full {
                    bail!("Graph refused to list the calendar");
                }
                tracing::info!(
                    account = self.account,
                    "delta link expired; listing the calendar"
                );
                (url, start, end, full) = fresh();
                items.clear();
                continue;
            };
            items.extend(page.value);
            match (page.next_link, page.delta_link) {
                (Some(next), _) => url = next,
                (None, Some(link)) => break link,
                (None, None) => bail!("a calendar delta page has no link"),
            }
        };

        let mut events = vec![];
        let mut removed = vec![];
        for item in items {
            if item.get("@removed").is_some() {
                let id = item["id"].as_str().context("a removal without an id")?;
                removed.push(id.to_owned());
            } else {
                events.push(observed(item)?);
            }
        }
        // Nothing to record; the old delta link stays good.
        if !full && events.is_empty() && removed.is_empty() {
            return Ok(());
        }
        // The masters of the series whose occurrences changed, as they
        // may have: they say how a series recurs.
        let mut series = vec![];
        let mut masters: Vec<&str> = events.iter().filter_map(|e| e.series.as_deref()).collect();
        masters.sort_unstable();
        masters.dedup();
        for id in masters {
            // Gone since: its occurrences will be, at the next sync.
            let Some(master) = self
                .graph
                .fetch::<serde_json::Value>(&format!("/events/{id}"))
                .await?
            else {
                continue;
            };
            series.push(ObservedSeries {
                external_id: id.to_owned(),
                transaction_id: master["transactionId"].as_str().map(str::to_owned),
                raw: master.to_string(),
            });
        }
        let sync_state = SyncState {
            start,
            end,
            delta_link,
        };
        self.dispatcher
            .observe(
                self.account.clone(),
                CalendarObserved {
                    account_kind: uplink_email::m365::sync::KIND,
                    replace: full,
                    events,
                    removed,
                    series,
                    sync_state: serde_json::to_value(sync_state).expect("serializes"),
                },
            )
            .await
            .map(drop)
    }

    fn start_link(&self, start: i64, end: i64) -> String {
        let iso = |ms: i64| mail_parser::DateTime::from_timestamp(ms / 1000).to_rfc3339();
        self.graph.url(&format!(
            "/calendarView/delta?startDateTime={}&endDateTime={}",
            iso(start),
            iso(end)
        ))
    }

    async fn state(&self) -> Result<Option<SyncState>> {
        let reader = self.reader.clone();
        let account = self.account.clone();
        let state: Option<String> = tokio::task::spawn_blocking(move || {
            reader
                .lock()
                .expect("reader poisoned")
                .query_row(
                    "SELECT state FROM calendar_sync WHERE account_id = ?1",
                    [account],
                    |r| r.get(0),
                )
                .optional()
        })
        .await??;
        state
            .map(|s| serde_json::from_str(&s).with_context(|| format!("calendar sync state {s}")))
            .transpose()
    }
}

/// Whether the entry changed a calendar.
fn changes_calendar(entry: &LogEntry) -> bool {
    entry
        .events
        .iter()
        .any(|e| matches!(e, ServerEvent::CalendarChanged(_)))
}

/// An event of a delta page (or as Graph answers a write), as recorded.
pub(crate) fn observed(item: serde_json::Value) -> Result<ObservedEvent> {
    let raw = item.to_string();
    let event: Event = serde_json::from_value(item).with_context(|| format!("event {raw}"))?;
    Ok(ObservedEvent {
        start: event.start.ms()?,
        end: event.end.ms()?,
        series: event.series_master_id,
        exception: event.kind == "exception",
        subject: event.subject.unwrap_or_default(),
        location: event
            .location
            .and_then(|l| l.display_name)
            .unwrap_or_default(),
        preview: event.body_preview.unwrap_or_default(),
        all_day: event.is_all_day,
        cancelled: event.is_cancelled,
        show_as: event.show_as,
        response: event.response_status.response,
        join_url: event
            .online_meeting
            .and_then(|m| m.join_url)
            .or(event.online_meeting_url),
        reminder: event
            .is_reminder_on
            .then_some(event.reminder_minutes_before_start),
        modified: ms(&event.last_modified_date_time)?,
        external_id: event.id,
        transaction_id: event.transaction_id,
        raw,
    })
}

/// Ms since the epoch of an RFC 3339 time, to the second.
fn ms(time: &str) -> Result<i64> {
    let parsed =
        mail_parser::DateTime::parse_rfc3339(time).with_context(|| format!("time {time}"))?;
    Ok(parsed.to_timestamp() * 1000)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis()
        .try_into()
        .expect("fits")
}
