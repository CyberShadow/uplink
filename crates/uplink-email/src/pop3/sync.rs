//! A POP3 account's sync (SPEC §8.12): mail is downloaded, and is
//! uplink's from then on. The server has no folders or flags to keep in
//! step, so labels are uplink's alone, as Gmail's are (none a folder):
//! changes to them take effect as they are pushed, and sent mail is filed
//! in Sent by uplink.

use super::client::{self, Client};
use crate::{
    account::{Account, Outgoing, Provider, SendError, SyncTask},
    compose::{now_ms, SentCopy},
    mail_server::{
        self,
        sign_in::{self, SignIn},
        smtp,
    },
    mailbox::{self, LabelChange, LabelsPushed, MailboxObserved, NewEntry, ObservedLabel},
};
use anyhow::Result;
use mail_parser::{HeaderName, MessageParser};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::Duration};
use tokio_rustls::TlsConnector;
use uplink_config::Pop3AccountConfig;
use uplink_protocol::mail::LabelKind;
use uplink_storage::seal::MasterKey;
use uplink_tx_log::Dispatcher;

pub const KIND: &str = "pop3";
/// How often to poll for new mail: less often than IMAP, as each poll
/// signs in anew, which servers may limit.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Messages downloaded in a step: a first sync takes many, between which
/// pushes and sends go.
const BATCH: usize = 50;

/// External ids: of mail downloaded, by its unique id (UIDL), and of mail
/// sent, by its Message-ID.
const DOWNLOADED: &str = "uidl:";
const SENT_COPY: &str = "sent:";

/// The account's labels: Gmail's system labels, but for what POP3 cannot
/// tell (drafts, importance, categories).
const LABELS: [(&str, &str); 6] = [
    ("INBOX", "Inbox"),
    ("UNREAD", "Unread"),
    ("STARRED", "Starred"),
    ("SENT", "Sent"),
    ("SPAM", "Spam"),
    ("TRASH", "Trash"),
];

/// `account.sync_state` of a POP3 account. What has been downloaded is
/// the account's entries, so only whether it is all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncState {
    /// Whether the first sync is still downloading the maildrop.
    listing: bool,
}

/// The sync task of `config`'s account, signing in with what `key`
/// unseals, storing mail under `data_root`.
pub fn task(
    config: &Pop3AccountConfig,
    key: Arc<MasterKey>,
    http: reqwest::Client,
    dispatcher: Dispatcher,
    data_root: &Path,
) -> Result<SyncTask<Pop3Sync>> {
    Ok(SyncTask::new(
        Pop3Sync::new(
            config.clone(),
            SignIn::of(config.login(), key, http),
            sign_in::tls(vec![])?,
        ),
        config.id.clone(),
        config.identities(),
        dispatcher,
        uplink_storage::open_reader(data_root)?,
        data_root,
        POLL_INTERVAL,
    ))
}

pub struct Pop3Sync {
    config: Pop3AccountConfig,
    sign_in: SignIn,
    tls: TlsConnector,
}

impl Provider for Pop3Sync {
    const KIND: &'static str = KIND;
    /// Sent mail is filed by uplink, once the server accepted it.
    const FILES_SENT: bool = false;

    /// Downloads the maildrop's messages not yet downloaded, newest first;
    /// then, if the account says so, deletes those stored from the
    /// server. A session sees the maildrop as it was when it began (RFC
    /// 1939 §8), and deletes only as it quits.
    async fn sync(&self, account: &Account) -> Result<bool> {
        let state = account.sync_state::<SyncState>().await?;
        let mut client = client::connect(&self.config, &self.sign_in, &self.tls).await?;
        let listed = client.uidl().await?;
        let known = account.known().await?;
        let (stored, mut new): (Vec<_>, Vec<_>) = listed
            .iter()
            .partition(|(_, uid)| known.contains(&format!("{DOWNLOADED}{uid}")));
        // Listed oldest first.
        new.reverse();
        let more = new.len() > BATCH;
        new.truncate(BATCH);

        let mut added = vec![];
        for (number, uid) in &new {
            added.push(self.download(&mut client, account, *number, uid).await?);
        }
        // Oldest first, so the newest are the last recorded.
        added.sort_by_key(|e| e.received_at);
        let sync_state = SyncState {
            listing: state.as_ref().is_none_or(|s| s.listing) && more,
        };
        if !added.is_empty() || state.as_ref() != Some(&sync_state) {
            let value = serde_json::to_value(&sync_state).expect("sync state serializes");
            account
                .observe(MailboxObserved {
                    account_kind: KIND,
                    labels: state.is_none().then(labels),
                    rekeyed: vec![],
                    added,
                    updated: vec![],
                    removed: vec![],
                    sync_state: Some(if sync_state.listing {
                        mailbox::SyncState::Listing {
                            state: value,
                            total: Some(listed.len().try_into()?),
                        }
                    } else {
                        mailbox::SyncState::Synced(value)
                    }),
                })
                .await?;
        }
        if self.config.delete_after_download {
            // Those stored before, and now: all recorded.
            for (number, _) in stored.into_iter().chain(new) {
                client.dele(*number).await?;
            }
        }
        client.quit().await?;
        Ok(more)
    }

    /// The labels are uplink's: what the user changes is so at once.
    async fn push(&self, account: &Account) -> Result<()> {
        let pending = account.pending().await?;
        if pending.is_empty() {
            return Ok(());
        }
        let changes = pending
            .into_iter()
            .map(|p| LabelChange {
                entry: p.entry,
                label: p.label,
                desired: p.desired,
            })
            .collect();
        account.observe(LabelsPushed { changes }).await
    }

    /// Submits the message by SMTP, then files it in Sent.
    async fn send(&self, account: &Account, mail: Outgoing<'_>) -> Result<SentCopy, SendError> {
        smtp::send(
            self.config.login(),
            &self.config.smtp,
            &self.sign_in,
            &self.tls,
            mail.raw,
        )
        .await?;
        // If it cannot be filed, it stays listed as waiting for its copy,
        // which is so.
        if let Err(e) = file_sent(account, &mail).await {
            tracing::warn!(
                account = self.config.id,
                "filing sent mail {} in Sent: {e:#}",
                mail.message_id
            );
        }
        Ok(SentCopy::OurMessageId)
    }
}

impl Pop3Sync {
    /// Reaching `config`'s servers with `tls`.
    pub fn new(config: Pop3AccountConfig, sign_in: SignIn, tls: TlsConnector) -> Self {
        Self {
            config,
            sign_in,
            tls,
        }
    }

    async fn download(
        &self,
        client: &mut Client,
        account: &Account,
        number: u32,
        uid: &str,
    ) -> Result<NewEntry> {
        let raw = client.retr(number).await?;
        let received_at = received_at(&raw).unwrap_or_else(now_ms);
        let thread_id = mail_server::thread_of(&raw);
        let (variant, parsed, size_bytes) = account.store_raw(raw).await?;
        Ok(NewEntry {
            external_id: format!("{DOWNLOADED}{uid}"),
            thread_id,
            received_at,
            labels: vec!["INBOX".into(), "UNREAD".into()],
            variant,
            size_bytes,
            parsed,
        })
    }
}

fn labels() -> Vec<ObservedLabel> {
    LABELS
        .into_iter()
        .map(|(external_id, name)| ObservedLabel {
            external_id: external_id.into(),
            name: name.into(),
            system: true,
            kind: LabelKind::Tag,
        })
        .collect()
}

/// The sent message, as its entry in Sent; joined to the message sent
/// when the send is recorded (SPEC §8.6).
async fn file_sent(account: &Account, mail: &Outgoing<'_>) -> Result<()> {
    let thread_id = mail_server::thread_of(mail.raw);
    let (variant, parsed, size_bytes) = account.store_raw(mail.raw.to_vec()).await?;
    account
        .observe(MailboxObserved {
            account_kind: KIND,
            labels: None,
            rekeyed: vec![],
            added: vec![NewEntry {
                external_id: format!("{SENT_COPY}{}", mail.message_id),
                thread_id,
                received_at: now_ms(),
                labels: vec!["SENT".into()],
                variant,
                size_bytes,
                parsed,
            }],
            updated: vec![],
            removed: vec![],
            sync_state: None,
        })
        .await
}

/// When the message was delivered, in unix ms: as the server's Received
/// header says (the first, which the last hop adds); for mail not
/// delivered so (imported), its Date; else none.
fn received_at(raw: &[u8]) -> Option<i64> {
    let message = MessageParser::new()
        .header_received(HeaderName::Received)
        .with_date_headers()
        .parse_headers(raw)?;
    let date = message
        .received_all()
        .next()
        .and_then(|r| r.date())
        .or_else(|| message.date().copied())?;
    Some(date.to_timestamp() * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_was_received_when_the_last_hop_says() {
        let raw = b"Received: from b by c; Tue, 3 Nov 2026 10:00:05 +0000\r\n\
                    Received: from a by b; Tue, 3 Nov 2026 10:00:01 +0000\r\n\
                    Date: Tue, 3 Nov 2026 09:00:00 +0000\r\n\r\nbody";
        assert_eq!(received_at(raw), Some(1_793_700_005_000));
        let imported = b"Date: Tue, 3 Nov 2026 09:00:00 +0000\r\n\r\nbody";
        assert_eq!(received_at(imported), Some(1_793_696_400_000));
        assert_eq!(received_at(b"Subject: none\r\n\r\nbody"), None);
    }
}
