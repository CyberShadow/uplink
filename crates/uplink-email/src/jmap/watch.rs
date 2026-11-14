//! Watching an account's emails and mailboxes by the event source (RFC
//! 8620 §7.3), so that changes are synced as they happen rather than at
//! the next poll (SPEC §8.13).

use super::api::Jmap;
use crate::watching::{Told, Watching};
use anyhow::{Context, Result};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;

/// How often the server is asked to ping; it may ping less often
/// (Stalwart: every 30 s at least).
const PING: Duration = Duration::from_secs(60);
/// How long the stream may say nothing before the connection is taken for
/// lost without a word (a NAT forgetting it) and made anew.
const SILENCE: Duration = Duration::from_secs(5 * 60);
/// How long after a failure the watch begins anew; polling goes on
/// meanwhile.
const RETRY: Duration = Duration::from_secs(30);

/// What the watch is called, of the account's one.
const WATCH: &str = "event source";

/// The watch, for as long as it is kept.
pub struct Watcher {
    watching: Arc<Watching>,
    task: JoinHandle<()>,
}

impl Watcher {
    pub fn start(account: String, jmap: Arc<Jmap>) -> Self {
        let watching = Arc::new(Watching::default());
        let task = tokio::spawn(watch(account, jmap, watching.clone()));
        Self { watching, task }
    }

    /// Resolves once the server tells of a change, or once the watch
    /// (re)begins, as it may have missed some, or fails; at once if that
    /// happened since the last call.
    pub async fn told(&self) -> Told {
        self.watching.told().await
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn watch(account: String, jmap: Arc<Jmap>, watching: Arc<Watching>) {
    loop {
        let Err(e) = listen(&jmap, &watching).await;
        tracing::warn!(account, "watching for changes: {e:#}");
        watching.failing(WATCH, format!("{e:#}"));
        tokio::time::sleep(RETRY).await;
    }
}

/// Tells of the changes the event source tells of, until it fails.
async fn listen(jmap: &Jmap, watching: &Watching) -> Result<std::convert::Infallible> {
    let mut response = jmap.event_source("Email,Mailbox", PING).await?;
    watching.watching(WATCH);
    let mut events = Events::default();
    loop {
        let chunk = tokio::time::timeout(SILENCE, response.chunk())
            .await
            .context("the event source fell silent")?
            .context("reading the event source")?
            .context("the server closed the event source")?;
        for event in events.push(&chunk) {
            if event == "state" {
                watching.changed();
            }
        }
    }
}

/// Server-sent events (HTML's `text/event-stream`), as far as uplink reads
/// them: each event's type.
#[derive(Default)]
struct Events {
    /// What has come of the event not yet ended.
    pending: Vec<u8>,
}

impl Events {
    /// The types of the events that `chunk` ends.
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        // Lines end in CRLF or LF (or CR alone, which no server sends).
        self.pending
            .extend(chunk.iter().copied().filter(|&b| b != b'\r'));
        let mut types = vec![];
        while let Some(end) = self.pending.windows(2).position(|w| w == b"\n\n") {
            let event: Vec<u8> = self.pending.drain(..end + 2).collect();
            let event = String::from_utf8_lossy(&event);
            let kind = event
                .lines()
                .find_map(|line| line.strip_prefix("event:"))
                .map_or("message", str::trim);
            types.push(kind.to_owned());
        }
        types
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_read_across_chunks() {
        let mut events = Events::default();
        assert!(events.push(b"event: ping\r\ndata: {}\r").is_empty());
        assert_eq!(events.push(b"\n\r\nevent: state\ndata: {\"a\""), ["ping"]);
        assert_eq!(events.push(b": 1}\n\ndata: x\n\n"), ["state", "message"]);
    }
}
