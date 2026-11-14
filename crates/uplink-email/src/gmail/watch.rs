//! Watching an account's mailbox by Gmail's push notifications, published
//! to a Cloud Pub/Sub topic and pulled from it, so that changes are synced
//! as they happen rather than at the next poll (SPEC §8.1).

use super::{api::Gmail, pubsub::PubSub};
use crate::watching::{Told, Watching};
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use tokio::{task::JoinHandle, time::Instant};

/// Who Gmail publishes as.
const GMAIL_PUBLISHER: &str = "serviceAccount:gmail-api-push@system.gserviceaccount.com";
/// How often the watch is renewed: Google recommends daily, and it lapses
/// after 7 days.
const RENEWAL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long after a failure the watch begins anew.
const RETRY: Duration = Duration::from_secs(30);

/// Where an account's changes are published: in the project of its OAuth
/// client, as Gmail requires.
pub struct Push {
    pub pubsub: PubSub,
    /// `projects/<project>/topics/<id>`
    pub topic: String,
    /// `projects/<project>/subscriptions/<id>`
    pub subscription: String,
}

impl Push {
    /// Account `id`'s topic and subscription in `project`.
    pub fn new(pubsub: PubSub, project: &str, id: &str) -> Self {
        Self {
            pubsub,
            topic: format!("projects/{project}/topics/uplink-{id}"),
            subscription: format!("projects/{project}/subscriptions/uplink-{id}"),
        }
    }
}

/// The watch, for as long as it is kept.
pub struct Watcher {
    watching: Arc<Watching>,
    task: JoinHandle<()>,
}

/// What the watch is called, of the account's one.
const WATCH: &str = "push";

impl Watcher {
    /// While there is no watch, the mailbox is polled every
    /// `unwatched_poll`.
    pub fn start(account: String, gmail: Gmail, push: Arc<Push>, unwatched_poll: Duration) -> Self {
        let watching = Arc::new(Watching::default());
        let task = tokio::spawn(watch(
            account,
            gmail,
            push,
            unwatched_poll,
            watching.clone(),
        ));
        Self { watching, task }
    }

    /// Resolves once Gmail tells of a change, or once the watch (re)begins,
    /// as it may have missed some; while there is no watch, every
    /// `unwatched_poll`; or once the watch fails. At once if that happened
    /// since the last call.
    pub async fn told(&self) -> Told {
        self.watching.told().await
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn watch(
    account: String,
    gmail: Gmail,
    push: Arc<Push>,
    unwatched_poll: Duration,
    watching: Arc<Watching>,
) {
    loop {
        let Err(e) = listen(&gmail, &push, &watching).await;
        tracing::warn!(
            account,
            "watching for changes: {e:#}; polling every {unwatched_poll:?} meanwhile"
        );
        watching.failing(WATCH, format!("{e:#}"));
        let retry = Instant::now() + RETRY;
        while Instant::now() < retry {
            tokio::time::sleep(unwatched_poll).await;
            watching.changed();
        }
    }
}

/// Sets the watch up and tells of the changes it tells of, until it fails.
async fn listen(
    gmail: &Gmail,
    push: &Push,
    watching: &Watching,
) -> Result<std::convert::Infallible> {
    let pubsub = &push.pubsub;
    pubsub.create_topic(&push.topic).await?;
    pubsub.allow_publisher(&push.topic, GMAIL_PUBLISHER).await?;
    pubsub
        .create_subscription(&push.subscription, &push.topic)
        .await?;
    gmail.watch(&push.topic).await?;
    let mut renewal = Instant::now() + RENEWAL;
    watching.watching(WATCH);
    loop {
        if Instant::now() >= renewal {
            gmail.watch(&push.topic).await?;
            renewal = Instant::now() + RENEWAL;
        }
        let pulled = pubsub.pull(&push.subscription).await?;
        if !pulled.is_empty() {
            pubsub.acknowledge(&push.subscription, &pulled).await?;
            watching.changed();
        }
    }
}
