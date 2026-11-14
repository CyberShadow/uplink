//! uplink's side of the provider tests: a database, the mail service,
//! and an account's sync task.

use super::eventually;
use std::{sync::Arc, time::Duration};
use uplink_email::{
    account::{Provider, SyncTask},
    compose, mailbox,
    query::ConfiguredAccount,
    service::Mail,
};
use uplink_protocol::{
    compose::{DraftContent, DraftId, OutboxItem, Pending, SendState},
    mail::{EntryId, Identity, LabelId, MailProvider, MessageId},
    ServerResponse,
};
use uplink_realtime::Hub;
use uplink_storage::rusqlite::{Connection, OptionalExtension};
use uplink_tx_log::{Dispatcher, Registry};

/// The identities of an account of `address`, unnamed, then `others`.
pub fn own(address: &str, others: &[Identity]) -> Vec<Identity> {
    let own = Identity::Address {
        address: address.into(),
        name: None,
    };
    std::iter::once(own).chain(others.iter().cloned()).collect()
}

pub const SEND_DELAY: Duration = Duration::from_millis(500);

pub struct Uplink {
    pub dir: tempfile::TempDir,
    pub dispatcher: Dispatcher,
    pub mail: Mail,
    /// The account's identities besides its address.
    others: Vec<Identity>,
}

impl Uplink {
    /// An account `test` at `provider`.
    pub fn new(provider: MailProvider) -> Self {
        Self::with_identities(provider, vec![])
    }

    /// An account `test` at `provider`, of identities `others` besides its
    /// address.
    pub fn with_identities(provider: MailProvider, others: Vec<Identity>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        mailbox::register(&mut registry);
        compose::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(1024)),
        );
        let mail = Mail::new(
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
            dir.path(),
            vec![ConfiguredAccount {
                id: "test".into(),
                address: "alice@example.com".into(),
                identities: own("alice@example.com", &others),
                provider,
            }],
            SEND_DELAY,
        );
        Self {
            dir,
            dispatcher,
            mail,
            others,
        }
    }

    pub fn reader(&self) -> Connection {
        uplink_storage::open_reader(self.dir.path()).unwrap()
    }

    /// Runs `provider`'s sync task for the account, polling quickly.
    pub fn start<P: Provider>(&self, provider: P, address: &str) -> tokio::task::JoinHandle<()> {
        self.start_polling(provider, address, Duration::from_millis(100))
    }

    /// As [`start`](Self::start), polling every `poll_interval`.
    pub fn start_polling<P: Provider>(
        &self,
        provider: P,
        address: &str,
        poll_interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.task(provider, address, poll_interval).run())
    }

    /// `provider`'s sync task for the account, not running: for a test to
    /// take its steps.
    pub fn task<P: Provider>(
        &self,
        provider: P,
        address: &str,
        poll_interval: Duration,
    ) -> SyncTask<P> {
        SyncTask::new(
            provider,
            "test".into(),
            own(address, &self.others),
            self.dispatcher.clone(),
            self.reader(),
            self.dir.path(),
            poll_interval,
        )
    }

    /// How many entries the account has.
    pub fn entries(&self) -> i64 {
        self.reader()
            .query_row("SELECT count(*) FROM mail_entry", [], |r| r.get(0))
            .unwrap()
    }

    /// The entry of the message whose subject starts with `tag`.
    pub fn entry(&self, tag: &str) -> Option<EntryId> {
        self.reader()
            .query_row(
                "SELECT e.id FROM mail_entry e JOIN message m ON m.id = e.message
                 WHERE m.subject LIKE ?1 || ' %'",
                [tag],
                |r| Ok(EntryId(r.get(0)?)),
            )
            .optional()
            .unwrap()
    }

    /// The entry's labels, as (external id, desired, observed).
    pub fn labels(&self, entry: EntryId) -> Vec<(String, bool, bool)> {
        self.reader()
            .prepare(
                "SELECT l.external_id, el.desired, el.observed
                 FROM entry_label el JOIN label l ON l.id = el.label
                 WHERE el.entry = ?1 ORDER BY 1",
            )
            .unwrap()
            .query_map([entry.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// Whether the account's first listing is done.
    pub fn synced(&self) -> bool {
        self.reader()
            .query_row("SELECT synced FROM account WHERE id = 'test'", [], |r| {
                r.get(0)
            })
            .optional()
            .unwrap()
            .unwrap_or(false)
    }

    /// The description of the last log entry of `kind`, if any.
    pub fn logged(&self, kind: &str) -> Option<String> {
        self.reader()
            .query_row(
                "SELECT description FROM tx_log WHERE op_type = ?1 ORDER BY id DESC LIMIT 1",
                [kind],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    }

    /// The op that made the entry's label pending, and the provider's
    /// refusal of it, if any.
    pub fn pending(&self, entry: EntryId, label: LabelId) -> (Option<i64>, Option<String>) {
        self.reader()
            .query_row(
                "SELECT desired_by, refused FROM entry_label WHERE entry = ?1 AND label = ?2",
                [entry.0, label.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    pub fn label(&self, external_id: &str) -> LabelId {
        LabelId(
            self.reader()
                .query_row(
                    "SELECT id FROM label WHERE external_id = ?1",
                    [external_id],
                    |r| r.get(0),
                )
                .unwrap(),
        )
    }

    pub async fn outbox(&self) -> Vec<(i64, SendState)> {
        self.sends()
            .await
            .into_iter()
            .map(|i| (i.id.0, i.state))
            .collect()
    }

    /// The outbox's sends.
    pub async fn sends(&self) -> Vec<OutboxItem> {
        let mut conn = self.reader();
        let tx = conn.transaction().unwrap();
        uplink_email::outbox::outbox(&tx)
            .unwrap()
            .into_iter()
            .flat_map(|a| match a.pending {
                Pending::Mail { sends, .. } => sends,
                Pending::Matrix { .. } | Pending::Calendar { .. } => {
                    unreachable!("a mail account's mail")
                }
            })
            .collect()
    }

    pub fn message_of(&self, entry: EntryId) -> (MessageId, String, Option<String>) {
        self.reader()
            .query_row(
                "SELECT m.id, m.message_id, e.thread_id FROM mail_entry e
                 JOIN message m ON m.id = e.message WHERE e.id = ?1",
                [entry.0],
                |r| Ok((MessageId(r.get(0)?), r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    pub fn external_id(&self, entry: EntryId) -> String {
        self.reader()
            .query_row(
                "SELECT external_id FROM mail_entry WHERE id = ?1",
                [entry.0],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Creates a draft to `to` and sends it.
    pub async fn send(&self, to: &str, subject: &str, in_reply_to: Option<MessageId>) -> i64 {
        self.send_from("", to, subject, in_reply_to).await
    }

    /// Creates a draft from `from` (the account's address if empty) to
    /// `to`, and sends it.
    pub async fn send_from(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        in_reply_to: Option<MessageId>,
    ) -> i64 {
        let id = DraftId(subject.into());
        self.mail
            .create_draft(
                id.clone(),
                "test".into(),
                DraftContent {
                    from: from.into(),
                    to: to.into(),
                    cc: String::new(),
                    bcc: String::new(),
                    subject: subject.into(),
                    body: "<p>Sent by the <b>uplink</b> tests.</p>".into(),
                },
                in_reply_to,
            )
            .await
            .unwrap();
        let ServerResponse::Dispatched { tx_log_id } = self.mail.send_draft(id).await.unwrap()
        else {
            panic!()
        };
        tx_log_id.0
    }

    pub fn log_len(&self) -> i64 {
        self.reader()
            .query_row("SELECT count(*) FROM tx_log", [], |r| r.get(0))
            .unwrap()
    }

    /// The length of the log, once it stops growing.
    pub async fn settled(&self) -> i64 {
        let mut len = self.log_len();
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            match self.log_len() {
                now if now == len => return len,
                now => len = now,
            }
        }
    }

    pub async fn arrived(&self, tag: &str) -> EntryId {
        eventually(&format!("{tag} to arrive"), || async { self.entry(tag) }).await
    }
}

pub fn label_set(labels: &[(String, bool, bool)]) -> Vec<&str> {
    labels.iter().map(|(l, _, _)| l.as_str()).collect()
}
