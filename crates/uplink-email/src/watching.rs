//! What a provider's watches tell its sync task (SPEC §8.14): that the
//! mailbox changed, so that it syncs before the poll; and whether they
//! are failing, so that the account records when its changes stop being
//! told as they happen, and when they are again.

use anyhow::{ensure, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use tokio::sync::{watch, Mutex, Notify};
use uplink_protocol::{mail::AccountChanged, ServerEvent};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{Observation, Observed};

/// What the watches tell, for the one task that waits on it.
pub struct Watching {
    changed: Notify,
    /// The watches failing, each by its name, with its error.
    failing: watch::Sender<BTreeMap<String, String>>,
    /// The task's view of `failing`.
    seen: Mutex<watch::Receiver<BTreeMap<String, String>>>,
}

/// What [`Watching::told`] tells.
#[derive(Debug, PartialEq, Eq)]
pub enum Told {
    /// The mailbox changed, or a watch (re)began, as it may have missed
    /// some changes.
    Changed,
    /// Why some watch is failing, or `None` once none is: of every watch
    /// failing, its error.
    Failing(Option<String>),
}

impl Default for Watching {
    fn default() -> Self {
        let (failing, seen) = watch::channel(BTreeMap::new());
        Self {
            changed: Notify::new(),
            failing,
            seen: Mutex::new(seen),
        }
    }
}

impl Watching {
    /// The mailbox changed, or a watch (re)began.
    pub fn changed(&self) {
        self.changed.notify_one();
    }

    /// `watch` failed, for `error`. Told at each failure: the task records
    /// only the start of a failure.
    pub fn failing(&self, watch: &str, error: String) {
        self.failing.send_modify(|failing| {
            failing.insert(watch.to_owned(), error);
        });
    }

    /// `watch` (re)began. Told at each beginning, so that a failure
    /// recorded before the process started ends with it.
    pub fn watching(&self, watch: &str) {
        self.failing.send_modify(|failing| {
            failing.remove(watch);
        });
        self.changed();
    }

    /// Resolves at the next tiding, or at once if one came since the last
    /// call.
    pub async fn told(&self) -> Told {
        let mut seen = self.seen.lock().await;
        tokio::select! {
            // A watch's beginning tells of both: its failure's end first.
            biased;
            seen_changed = seen.changed() => {
                seen_changed.expect("the sender is ours");
                let failing = seen.borrow_and_update();
                Told::Failing((!failing.is_empty()).then(|| {
                    failing.values().map(String::as_str).collect::<Vec<_>>().join("; ")
                }))
            }
            () = self.changed.notified() => Told::Changed,
        }
    }
}

/// `error` says why the account's changes are not told as they happen;
/// `None` once they are again.
#[derive(Debug, Serialize)]
pub struct WatchFailing {
    pub error: Option<String>,
    /// Unix ms.
    pub at: i64,
}

impl Observation for WatchFailing {
    const KIND: &'static str = "account.watch_failing";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let description = match &self.error {
            Some(error) => format!("Not watched, only polled: {error}"),
            None => "Watched again".to_owned(),
        };
        let updated = tx.execute(
            "UPDATE account SET watch_error = ?1,
               watch_failing_since = CASE WHEN ?1 IS NULL THEN NULL
                                          ELSE coalesce(watch_failing_since, ?2) END
             WHERE id = ?3",
            params![self.error, self.at, account],
        )?;
        ensure!(updated == 1, "account {account} is not recorded");
        Ok(Observed {
            description,
            events: vec![ServerEvent::AccountChanged(AccountChanged {
                account: account.to_owned(),
            })],
        })
    }
}

/// Whether the account is recorded as unwatched; `false` if it is not
/// recorded yet.
pub fn is_failing(conn: &Connection, account: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT watch_error IS NOT NULL FROM account WHERE id = ?1",
            [account],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failures_are_told_of_every_watch_until_all_begin() {
        let watching = Watching::default();
        watching.failing("Inbox", "refused".into());
        watching.failing("Alerts", "no such folder".into());
        assert_eq!(
            watching.told().await,
            Told::Failing(Some("no such folder; refused".into()))
        );
        watching.watching("Inbox");
        assert_eq!(
            watching.told().await,
            Told::Failing(Some("no such folder".into()))
        );
        // Its beginning tells of a change, as well.
        assert_eq!(watching.told().await, Told::Changed);
        watching.watching("Alerts");
        assert_eq!(watching.told().await, Told::Failing(None));
    }
}
