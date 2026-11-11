//! Watching an account's Inbox by IDLE (RFC 2177), on a connection of its
//! own, so that new mail is synced as it arrives rather than at the next
//! poll (SPEC §8.10). IDLE tells of the selected folder only: the others
//! are polled.

use super::session::{self, Connection};
use crate::mail_server::sign_in::SignIn;
use anyhow::{bail, Result};
use async_imap::extensions::idle::IdleResponse;
use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, task::JoinHandle};
use tokio_rustls::TlsConnector;
use uplink_config::ImapAccountConfig;

/// How long an IDLE lasts before it is renewed: under RFC 2177's 29
/// minutes, and short enough that a connection lost without a word (a NAT
/// forgetting it) is found and made anew in good time.
const RENEWAL: Duration = Duration::from_secs(10 * 60);
/// How long after a failure the watch begins anew; polling goes on
/// meanwhile.
const RETRY: Duration = Duration::from_secs(30);

/// The watch, for as long as it is kept.
pub struct Watcher {
    changed: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Watcher {
    pub fn start(config: ImapAccountConfig, sign_in: SignIn, tls: TlsConnector) -> Self {
        let changed = Arc::new(Notify::new());
        let task = tokio::spawn(watch(config, sign_in, tls, changed.clone()));
        Self { changed, task }
    }

    /// Resolves once the server tells of a change in the Inbox, or once
    /// the watch (re)begins, as it may have missed some; at once if that
    /// happened since the last call.
    pub async fn changed(&self) {
        self.changed.notified().await
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn watch(
    config: ImapAccountConfig,
    sign_in: SignIn,
    tls: TlsConnector,
    changed: Arc<Notify>,
) {
    loop {
        let connection = match session::connect(&config, &sign_in, &tls).await {
            Ok(connection) => connection,
            Err(e) => {
                tracing::warn!(account = config.id, "watching the Inbox: {e:#}");
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        if !connection.idle {
            tracing::info!(
                account = config.id,
                "{} cannot IDLE: the Inbox is polled",
                config.imap.host
            );
            return;
        }
        let Err(e) = idle(connection, &changed).await;
        tracing::warn!(account = config.id, "watching the Inbox: {e:#}");
        tokio::time::sleep(RETRY).await;
    }
}

/// IDLEs on the Inbox until the connection fails.
async fn idle(connection: Connection, changed: &Notify) -> Result<std::convert::Infallible> {
    let mut session = connection.session;
    session.select("INBOX").await?;
    // What changed while there was no watch.
    changed.notify_one();
    loop {
        let mut handle = session.idle();
        handle.init().await?;
        {
            // Dropping the stop source would end the wait.
            let (wait, _stop) = handle.wait_with_timeout(RENEWAL);
            match wait.await? {
                IdleResponse::NewData(_) => changed.notify_one(),
                IdleResponse::Timeout => {}
                // Never stopped by hand: the stream ended.
                IdleResponse::ManualInterrupt => bail!("the server closed the connection"),
            }
        }
        session = handle.done().await?;
    }
}
