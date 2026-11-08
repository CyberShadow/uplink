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
    compose::{DraftContent, DraftId, OutboxState},
    mail::{EntryId, LabelId, MessageId},
    ServerResponse,
};
use uplink_realtime::Hub;
use uplink_storage::rusqlite::{Connection, OptionalExtension};
use uplink_tx_log::{Dispatcher, Registry};

pub const SEND_DELAY: Duration = Duration::from_millis(500);

pub struct Uplink {
    pub dir: tempfile::TempDir,
    pub dispatcher: Dispatcher,
    pub mail: Mail,
}

impl Uplink {
    pub fn new() -> Self {
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
            }],
            SEND_DELAY,
        );
        Self {
            dir,
            dispatcher,
            mail,
        }
    }

    pub fn reader(&self) -> Connection {
        uplink_storage::open_reader(self.dir.path()).unwrap()
    }

    /// Runs `provider`'s sync task for the account, polling quickly.
    pub fn start<P: Provider>(&self, provider: P, address: &str) -> tokio::task::JoinHandle<()> {
        tokio::spawn(
            SyncTask::new(
                provider,
                "test".into(),
                address.into(),
                self.dispatcher.clone(),
                self.reader(),
                self.dir.path(),
                Duration::from_millis(100),
            )
            .run(),
        )
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

    pub async fn outbox(&self) -> Vec<(i64, OutboxState)> {
        let ServerResponse::Outbox { items, .. } = self.mail.list_outbox().await.unwrap() else {
            panic!()
        };
        items.into_iter().map(|i| (i.id.0, i.state)).collect()
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
        let id = DraftId(subject.into());
        self.mail
            .create_draft(
                id.clone(),
                "test".into(),
                DraftContent {
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
