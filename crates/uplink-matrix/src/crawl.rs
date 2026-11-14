//! Fetching rooms' whole history in the background, to be searched (SPEC
//! §9.2): of rooms small enough, the gaps of their timelines are filled,
//! the latest history first, as mail's first listing goes newest first.

use crate::{
    backfill,
    rooms::{self, GapToFill},
    sync::{read, retry},
};
use anyhow::{Context, Result};
use matrix_sdk::Client;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::broadcast::{self, error::RecvError};
use uplink_protocol::{matrix::GapId, LogEntry};
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::{Dispatcher, Observation};

/// The pause between pages, which spares the homeserver: Element's
/// crawler pauses 3 seconds between pages of 100.
const PACE: Duration = Duration::from_millis(1500);
/// How long a gap whose filling failed is left before it is tried again;
/// meanwhile, the others are filled.
const RETRY_FAILED: Duration = Duration::from_secs(600);

pub struct Crawler {
    pub account: String,
    pub client: Client,
    pub dispatcher: Dispatcher,
    pub reader: Arc<Mutex<Connection>>,
    /// Of rooms of at most this many joined members.
    pub members: u64,
    pub wake: broadcast::Receiver<Arc<LogEntry>>,
}

impl Crawler {
    /// Fills gaps until the process exits.
    pub async fn run(mut self) {
        let mut failed: HashMap<GapId, Instant> = HashMap::new();
        loop {
            failed.retain(|_, at| at.elapsed() < RETRY_FAILED);
            let skip: Vec<GapId> = failed.keys().copied().collect();
            let next = retry(&self.account, "finding a gap to fill", || {
                let (reader, account, members, skip) = (
                    self.reader.clone(),
                    self.account.clone(),
                    self.members,
                    skip.clone(),
                );
                async move {
                    read(&reader, move |conn| {
                        rooms::gap_to_fill(conn, &account, members, &skip)
                    })
                    .await
                }
            })
            .await;
            let Some(gap) = next else {
                self.wait().await;
                continue;
            };
            match self.fill(&gap).await {
                Ok(()) => tokio::time::sleep(PACE).await,
                Err(e) => {
                    tracing::warn!(
                        account = self.account,
                        "filling gap {} of {}: {e:#}",
                        gap.gap.0,
                        gap.room_id
                    );
                    failed.insert(gap.gap, Instant::now());
                }
            }
        }
    }

    async fn fill(&self, gap: &GapToFill) -> Result<()> {
        let room_id = <&matrix_sdk::ruma::RoomId>::try_from(gap.room_id.as_str())?;
        let room = self
            .client
            .get_room(room_id)
            .with_context(|| format!("the client does not have room {room_id}"))?;
        backfill::fill(
            &self.dispatcher,
            self.account.clone(),
            &room,
            gap.room,
            gap.gap,
            &gap.before,
        )
        .await
    }

    /// Until a sync may have left gaps, or a room grown small enough, or a
    /// gap that failed is to be tried again.
    async fn wait(&mut self) {
        let deadline = tokio::time::sleep(RETRY_FAILED);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return,
                entry = self.wake.recv() => match entry {
                    Ok(entry) if entry.op_type == rooms::Synced::KIND => return,
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => return,
                    Err(RecvError::Closed) => panic!("the hub outlives sync tasks"),
                },
            }
        }
    }
}
