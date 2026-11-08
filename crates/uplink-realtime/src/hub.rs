use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::broadcast;
use uplink_protocol::{Cursor, Epoch, Resume, ServerEvent};

/// Fans out [`ServerEvent`]s to all connected sessions and keeps the most
/// recent ones for replay to reconnecting clients (SPEC §5.3).
///
/// Every published event is assigned the next cursor. Cursors are
/// per-process; the [`Epoch`] distinguishes processes, so that a client
/// reconnecting after a restart is told to resync rather than being
/// replayed an unrelated stream.
pub struct Hub {
    epoch: Epoch,
    capacity: usize,
    state: Mutex<State>,
    live: broadcast::Sender<Arc<Published>>,
}

struct State {
    /// Cursor of the most recently published event; 0 before the first.
    head: Cursor,
    /// The last `capacity` events, oldest first.
    recent: VecDeque<Arc<Published>>,
}

#[derive(Debug)]
pub struct Published {
    pub cursor: Cursor,
    pub event: ServerEvent,
}

pub struct Subscription {
    /// Cursor of the most recent event at subscription time.
    pub cursor: Cursor,
    /// Events after the resume cursor, if the requested resume was
    /// possible. `None` means the client must resync.
    pub replay: Option<Vec<Arc<Published>>>,
    /// Every event published after `cursor`.
    pub live: broadcast::Receiver<Arc<Published>>,
}

impl Hub {
    /// `capacity` bounds both the replay buffer and how far a session may
    /// fall behind the live stream.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "hub capacity must be positive");
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before 1970")
            .as_nanos() as Epoch;
        Self {
            epoch,
            capacity,
            state: Mutex::new(State {
                head: 0,
                recent: VecDeque::with_capacity(capacity),
            }),
            live: broadcast::channel(capacity).0,
        }
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// Publishes `event` and returns its cursor. Events reach subscribers
    /// in the order of `publish` calls.
    pub fn publish(&self, event: ServerEvent) -> Cursor {
        let mut state = self.state.lock().expect("hub state poisoned");
        state.head += 1;
        let published = Arc::new(Published {
            cursor: state.head,
            event,
        });
        if state.recent.len() == self.capacity {
            state.recent.pop_front();
        }
        state.recent.push_back(published.clone());
        // An error only means there are no sessions to deliver to.
        let _ = self.live.send(published);
        state.head
    }

    /// Subscribes to events published from now on, and, if `resume` can be
    /// honoured, collects the events the client missed. Both happen under
    /// the same lock as `publish`, so replay and live stream neither
    /// overlap nor leave a gap.
    pub fn subscribe(&self, resume: Option<Resume>) -> Subscription {
        let state = self.state.lock().expect("hub state poisoned");
        let live = self.live.subscribe();
        let replay = resume.and_then(|r| self.replay_after(&state, r));
        Subscription {
            cursor: state.head,
            replay,
            live,
        }
    }

    fn replay_after(&self, state: &State, resume: Resume) -> Option<Vec<Arc<Published>>> {
        if resume.epoch != self.epoch || resume.cursor > state.head {
            return None;
        }
        // Oldest cursor still buffered (head + 1 when nothing is).
        let oldest = state.recent.front().map_or(state.head + 1, |p| p.cursor);
        if resume.cursor + 1 < oldest {
            return None;
        }
        Some(
            state
                .recent
                .iter()
                .filter(|p| p.cursor > resume.cursor)
                .cloned()
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uplink_protocol::TxLogId;

    fn event(n: i64) -> ServerEvent {
        ServerEvent::TxLogEntryUndone {
            id: TxLogId(n),
            undone_by: TxLogId(n + 1),
        }
    }

    fn cursors(replay: &[Arc<Published>]) -> Vec<Cursor> {
        replay.iter().map(|p| p.cursor).collect()
    }

    fn resume(hub: &Hub, cursor: Cursor) -> Option<Resume> {
        Some(Resume {
            epoch: hub.epoch(),
            cursor,
        })
    }

    #[test]
    fn cursors_are_sequential_from_one() {
        let hub = Hub::new(8);
        assert_eq!(hub.publish(event(1)), 1);
        assert_eq!(hub.publish(event(2)), 2);
    }

    #[test]
    fn fresh_subscription_is_not_resumed() {
        let hub = Hub::new(8);
        hub.publish(event(1));
        let sub = hub.subscribe(None);
        assert_eq!(sub.cursor, 1);
        assert!(sub.replay.is_none());
    }

    #[test]
    fn resume_replays_missed_events() {
        let hub = Hub::new(8);
        for n in 1..=4 {
            hub.publish(event(n));
        }
        let sub = hub.subscribe(resume(&hub, 2));
        assert_eq!(sub.cursor, 4);
        assert_eq!(cursors(&sub.replay.unwrap()), vec![3, 4]);
    }

    #[test]
    fn resume_at_head_replays_nothing() {
        let hub = Hub::new(8);
        hub.publish(event(1));
        let sub = hub.subscribe(resume(&hub, 1));
        assert_eq!(cursors(&sub.replay.unwrap()), Vec::<Cursor>::new());
        // Also before anything was ever published.
        let hub = Hub::new(8);
        assert!(hub.subscribe(resume(&hub, 0)).replay.is_some());
    }

    #[test]
    fn resume_from_other_epoch_requires_resync() {
        let hub = Hub::new(8);
        hub.publish(event(1));
        let sub = hub.subscribe(Some(Resume {
            epoch: hub.epoch() + 1,
            cursor: 1,
        }));
        assert!(sub.replay.is_none());
    }

    #[test]
    fn resume_beyond_head_requires_resync() {
        let hub = Hub::new(8);
        hub.publish(event(1));
        assert!(hub.subscribe(resume(&hub, 2)).replay.is_none());
    }

    #[test]
    fn resume_past_evicted_events_requires_resync() {
        let hub = Hub::new(2);
        for n in 1..=4 {
            hub.publish(event(n));
        }
        // Buffer holds 3 and 4: resuming from 2 is complete, from 1 is not.
        assert_eq!(
            cursors(&hub.subscribe(resume(&hub, 2)).replay.unwrap()),
            vec![3, 4]
        );
        assert!(hub.subscribe(resume(&hub, 1)).replay.is_none());
    }

    #[tokio::test]
    async fn live_stream_starts_after_subscription_cursor() {
        let hub = Hub::new(8);
        hub.publish(event(1));
        let mut sub = hub.subscribe(None);
        hub.publish(event(2));
        let p = sub.live.recv().await.unwrap();
        assert_eq!(p.cursor, 2);
        assert_eq!(p.event, event(2));
    }
}
