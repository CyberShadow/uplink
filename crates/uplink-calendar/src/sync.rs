//! An account's calendar sync (SPEC §10.1): a Graph calendar view of a
//! window around today, kept up to date through its delta link.
//!
//! Graph expands series in a calendar view: it lists each occurrence, as
//! changed if it was, and leaves out those cancelled. A delta link holds
//! its window, so as days pass the window is moved by listing it anew.

use crate::{CalendarObserved, ObservedEvent};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use uplink_config::M365AccountConfig;
use uplink_email::m365::api::Graph;
use uplink_protocol::calendar::{EventResponse, ShowAs};
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
struct DateTime {
    date_time: String,
    time_zone: String,
}

impl DateTime {
    /// Ms since the epoch; times are asked for in UTC.
    fn ms(&self) -> Result<i64> {
        anyhow::ensure!(
            self.time_zone.eq_ignore_ascii_case("UTC"),
            "a time in {}, not UTC",
            self.time_zone
        );
        let time = mail_parser::DateTime::parse_rfc3339(&self.date_time)
            .with_context(|| format!("dateTime {}", self.date_time))?;
        Ok(time.to_timestamp() * 1000)
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

    /// Runs until the process exits, retrying failures with backoff.
    pub async fn run(self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.sync(now_ms()).await {
                Ok(()) => {
                    backoff = Duration::from_secs(1);
                    tokio::time::sleep(self.poll_interval).await;
                }
                Err(e) => {
                    tracing::warn!(account = self.account, "calendar sync failed: {e:#}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
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

/// An event of a delta page, as recorded.
fn observed(item: serde_json::Value) -> Result<ObservedEvent> {
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
        external_id: event.id,
        raw,
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis()
        .try_into()
        .expect("fits")
}
