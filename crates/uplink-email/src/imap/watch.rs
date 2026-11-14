//! Watching an account's folders, so that changes are synced as they
//! happen rather than at the next poll (SPEC §8.10): every folder by NOTIFY
//! (RFC 5465), on one connection, where the server has it; else by IDLE
//! (RFC 2177), which tells of the selected folder only, the Inbox and the
//! folders the account names, each on a connection of its own. The others
//! are polled.

use super::{
    session::{self, Connection, Session},
    sync::path,
};
use crate::mail_server::sign_in::SignIn;
use crate::watching::{Told, Watching};
use anyhow::{bail, Result};
use async_imap::{extensions::idle::IdleResponse, types::NameAttribute};
use futures_util::TryStreamExt;
use std::{fmt, sync::Arc, time::Duration};
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsConnector;
use uplink_config::MailServer;

/// How long an IDLE lasts before it is renewed: under RFC 2177's 29
/// minutes, and short enough that a connection lost without a word (a NAT
/// forgetting it) is found and made anew in good time.
const RENEWAL: Duration = Duration::from_secs(10 * 60);
/// How long after a failure a watch begins anew; polling goes on
/// meanwhile.
const RETRY: Duration = Duration::from_secs(30);
/// What NOTIFY is asked to tell of: in every folder of the user's, mail
/// arriving and going and flags changing; and folders made, renamed and
/// deleted.
const NOTIFY: &str = "NOTIFY SET (personal (MessageNew MessageExpunge FlagChange MailboxName))";

/// The watches, for as long as they are kept.
pub struct Watcher {
    watching: Arc<Watching>,
    task: JoinHandle<()>,
}

/// What is watched: the account's mailbox at IMAP `server`, as `user`;
/// besides the Inbox, `folders`, by their paths (as [`path`] gives them).
#[derive(Clone)]
pub struct Watched {
    pub account: String,
    pub server: MailServer,
    pub user: String,
    pub folders: Vec<String>,
}

impl Watched {
    async fn connect(&self, sign_in: &SignIn, tls: &TlsConnector) -> Result<Connection> {
        session::connect(&self.server, &self.user, sign_in, tls).await
    }
}

impl Watcher {
    pub fn start(watched: Watched, sign_in: SignIn, tls: TlsConnector) -> Self {
        let watching = Arc::new(Watching::default());
        let task = tokio::spawn(watch(watched, sign_in, tls, watching.clone()));
        Self { watching, task }
    }

    /// Resolves once the server tells of a change in a folder watched, or
    /// once a watch (re)begins, as it may have missed some, or fails; at
    /// once if that happened since the last call.
    pub async fn told(&self) -> Told {
        self.watching.told().await
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Its watches with it.
        self.task.abort();
    }
}

/// What one connection watches.
enum Watch {
    /// Every folder, by NOTIFY.
    Every,
    Inbox,
    /// The folder of this path (as [`path`] gives it).
    Folder(String),
}

impl fmt::Display for Watch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Watch::Every => f.write_str("every folder"),
            Watch::Inbox => f.write_str("the Inbox"),
            Watch::Folder(path) => write!(f, "folder {path}"),
        }
    }
}

/// Learns what the server can do, and keeps the watches it allows.
async fn watch(watched: Watched, sign_in: SignIn, tls: TlsConnector, watching: Arc<Watching>) {
    let account = &watched.account;
    let host = &watched.server.host;
    // Before what the server can do is known, the one watch is learning it.
    const LEARNING: &str = "the server";
    let connection = loop {
        match watched.connect(&sign_in, &tls).await {
            Ok(connection) => break connection,
            Err(e) => {
                tracing::warn!(account, "watching for changes: {e:#}");
                watching.failing(LEARNING, format!("{e:#}"));
                tokio::time::sleep(RETRY).await;
            }
        }
    };
    // Its failure ends with the watches' beginning, if any: else, the
    // account is polled as it is to be.
    watching.watching(LEARNING);
    // NOTIFY tells while the client waits for it, which is by IDLE.
    if !connection.idle {
        tracing::info!(account, "{host} cannot IDLE: its folders are polled");
        return;
    }
    let watches = if connection.notify {
        if !watched.folders.is_empty() {
            tracing::info!(
                account,
                "{host} tells of every folder (NOTIFY): `watch` is not needed"
            );
        }
        vec![Watch::Every]
    } else {
        std::iter::once(Watch::Inbox)
            .chain(watched.folders.iter().cloned().map(Watch::Folder))
            .collect()
    };
    let mut first = Some(connection);
    let mut watches_kept = JoinSet::new();
    for watch in watches {
        watches_kept.spawn(keep(
            watched.clone(),
            sign_in.clone(),
            tls.clone(),
            watching.clone(),
            first.take(),
            watch,
        ));
    }
    while let Some(ended) = watches_kept.join_next().await {
        ended.expect("a watch goes on until stopped");
    }
}

/// Keeps `watch`, on `connection` at first, until stopped.
async fn keep(
    watched: Watched,
    sign_in: SignIn,
    tls: TlsConnector,
    watching: Arc<Watching>,
    mut connection: Option<Connection>,
    watch: Watch,
) {
    let name = watch.to_string();
    loop {
        let connected = match connection.take() {
            Some(connection) => Ok(connection),
            None => watched.connect(&sign_in, &tls).await,
        };
        let Err(e) = match connected {
            Ok(connection) => listen(connection.session, &watch, &name, &watching).await,
            Err(e) => Err(e),
        };
        tracing::warn!(account = watched.account, "watching {watch}: {e:#}");
        watching.failing(&name, format!("{watch}: {e:#}"));
        tokio::time::sleep(RETRY).await;
    }
}

/// Tells of the changes the server tells of, until the connection fails.
async fn listen(
    mut session: Session,
    watch: &Watch,
    name: &str,
    watching: &Watching,
) -> Result<std::convert::Infallible> {
    match watch {
        Watch::Every => {
            // Of a folder not selected, NOTIFY tells by STATUS, in which a
            // flag changed shows only as its HIGHESTMODSEQ, given once
            // CONDSTORE is enabled (RFC 5465 §5.2).
            session.run_command_and_check_ok("ENABLE CONDSTORE").await?;
            session.run_command_and_check_ok(NOTIFY).await?
        }
        Watch::Inbox => {
            session.select("INBOX").await?;
        }
        Watch::Folder(path) => {
            let name = mailbox(&mut session, path).await?;
            session.select(&name).await?;
        }
    }
    watching.watching(name);
    loop {
        let mut handle = session.idle();
        handle.init().await?;
        {
            // Dropping the stop source would end the wait.
            let (wait, _stop) = handle.wait_with_timeout(RENEWAL);
            match wait.await? {
                IdleResponse::NewData(_) => watching.changed(),
                IdleResponse::Timeout => {}
                // Never stopped by hand: the stream ended.
                IdleResponse::ManualInterrupt => bail!("the server closed the connection"),
            }
        }
        session = handle.done().await?;
    }
}

/// The name of the mailbox of folder `path`.
async fn mailbox(session: &mut Session, folder: &str) -> Result<String> {
    let names: Vec<_> = session
        .list(Some(""), Some("*"))
        .await?
        .try_collect()
        .await?;
    for name in &names {
        if !name.attributes().contains(&NameAttribute::NoSelect) && path(name)? == folder {
            return Ok(name.name().to_owned());
        }
    }
    bail!("the server has no folder {folder}")
}
