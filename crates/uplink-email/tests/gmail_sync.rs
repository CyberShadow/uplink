//! Gmail sync against a provider (emulated, or real: see `common`).

mod common;

use common::{eventually, message, Provider};
use std::{sync::Arc, time::Duration};
use uplink_email::{
    compose,
    gmail::{api::Gmail, oauth::Credentials, sync::Sync},
    mailbox,
    query::ConfiguredAccount,
    service::Mail,
    store::MailStore,
};
use uplink_protocol::{
    compose::{DraftContent, DraftId, OutboxState},
    mail::{EntryId, LabelId, MessageId},
    ServerResponse,
};
use uplink_realtime::Hub;
use uplink_storage::rusqlite::{Connection, OptionalExtension};
use uplink_tx_log::{Dispatcher, Registry};

const SEND_DELAY: Duration = Duration::from_millis(500);

struct Uplink {
    dir: tempfile::TempDir,
    dispatcher: Dispatcher,
    mail: Mail,
}

impl Uplink {
    fn new() -> Self {
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

    fn reader(&self) -> Connection {
        uplink_storage::open_reader(self.dir.path()).unwrap()
    }

    fn start_sync(&self, provider: &Provider) -> tokio::task::JoinHandle<()> {
        let http = reqwest::Client::new();
        let credentials =
            Credentials::new(http.clone(), provider.account.clone(), provider.key.clone());
        tokio::spawn(
            Sync {
                account: "test".into(),
                address: provider.account.address.clone(),
                gmail: Gmail::new(http, &provider.account.endpoints.api, credentials),
                dispatcher: self.dispatcher.clone(),
                reader: self.reader(),
                store: MailStore::new(self.dir.path()),
                poll_interval: Duration::from_millis(100),
            }
            .run(),
        )
    }

    /// The entry of the message whose subject starts with `tag`.
    fn entry(&self, tag: &str) -> Option<EntryId> {
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
    fn labels(&self, entry: EntryId) -> Vec<(String, bool, bool)> {
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

    fn label(&self, external_id: &str) -> LabelId {
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

    async fn outbox(&self) -> Vec<(i64, OutboxState)> {
        let ServerResponse::Outbox { items, .. } = self.mail.list_outbox().await.unwrap() else {
            panic!()
        };
        items.into_iter().map(|i| (i.id.0, i.state)).collect()
    }

    fn message_of(&self, entry: EntryId) -> (MessageId, String, Option<String>) {
        self.reader()
            .query_row(
                "SELECT m.id, m.message_id, e.thread_id FROM mail_entry e
                 JOIN message m ON m.id = e.message WHERE e.id = ?1",
                [entry.0],
                |r| Ok((MessageId(r.get(0)?), r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    fn external_id(&self, entry: EntryId) -> String {
        self.reader()
            .query_row(
                "SELECT external_id FROM mail_entry WHERE id = ?1",
                [entry.0],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Creates a draft to `to` and sends it.
    async fn send(&self, to: &str, subject: &str, in_reply_to: Option<MessageId>) -> i64 {
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

    fn log_len(&self) -> i64 {
        self.reader()
            .query_row("SELECT count(*) FROM tx_log", [], |r| r.get(0))
            .unwrap()
    }

    async fn arrived(&self, tag: &str) -> EntryId {
        eventually(&format!("{tag} to arrive"), || async { self.entry(tag) }).await
    }
}

fn label_set(labels: &[(String, bool, bool)]) -> Vec<&str> {
    labels.iter().map(|(l, _, _)| l.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn mirrors_the_mailbox_and_pushes_changes() {
    let uplink = Uplink::new();
    let provider = Provider::start(uplink.dir.path(), None).await;

    // Mail present before the first sync, and mail arriving afterwards.
    let first = provider.deliver(&message("first", "hello world")).await;
    let sync = uplink.start_sync(&provider);
    let first_entry = uplink.arrived("first").await;
    assert_eq!(
        uplink.labels(first_entry),
        [("INBOX".into(), true, true), ("UNREAD".into(), true, true)]
    );
    let second = provider
        .deliver(&message("second", "a quixotic body"))
        .await;
    let second_entry = uplink.arrived("second").await;

    // Archiving in uplink archives in Gmail; undo brings it back.
    let ServerResponse::Dispatched {
        tx_log_id: archived,
    } = uplink
        .mail
        .modify_labels(vec![first_entry], vec![], vec![uplink.label("INBOX")])
        .await
        .unwrap()
    else {
        panic!()
    };
    eventually("the archive to reach Gmail", || async {
        (provider.labels(&first).await == ["UNREAD"]).then_some(())
    })
    .await;
    eventually("the archive to be observed", || async {
        (uplink.labels(first_entry) == [("UNREAD".into(), true, true)]).then_some(())
    })
    .await;
    uplink.dispatcher.undo(archived).await.unwrap();
    eventually("the undo to reach Gmail", || async {
        (provider.labels(&first).await == ["INBOX", "UNREAD"]).then_some(())
    })
    .await;

    // A change made in Gmail shows up in uplink.
    provider.modify(&second, &["STARRED"], &["UNREAD"]).await;
    eventually("the star to be observed", || async {
        (label_set(&uplink.labels(second_entry)) == ["INBOX", "STARRED"]).then_some(())
    })
    .await;

    // Search covers subject, body and sender.
    for query in ["second", "quixot", "tester@uplink"] {
        let ServerResponse::Entries { entries, .. } =
            uplink.mail.search(query.into(), 10).await.unwrap()
        else {
            panic!()
        };
        assert!(
            entries.iter().any(|e| e.id == second_entry),
            "{query} does not find the message"
        );
    }

    // A restarted sync resumes from the recorded history id: idle polls
    // record nothing, and new mail still arrives.
    sync.abort();
    let _ = sync.await;
    let settled = uplink.log_len();
    let sync = uplink.start_sync(&provider);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(uplink.log_len(), settled, "the restarted sync re-synced");
    provider.deliver(&message("third", "after restart")).await;
    uplink.arrived("third").await;

    sync.abort();
    provider.clean_up().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resyncs_fully_once_history_expired() {
    let uplink = Uplink::new();
    if common::real() {
        eprintln!("skipped: needs an emulated account, whose history can expire");
        return;
    }
    let provider = Provider::start(uplink.dir.path(), Some(3)).await;
    let kept = provider.deliver(&message("kept", "")).await;
    let deleted = provider.deliver(&message("deleted", "")).await;
    let sync = uplink.start_sync(&provider);
    let kept_entry = uplink.arrived("kept").await;
    uplink.arrived("deleted").await;
    sync.abort();
    let _ = sync.await;

    // More changes than the history keeps, while uplink is away.
    for _ in 0..2 {
        provider.modify(&kept, &["STARRED"], &[]).await;
        provider.modify(&kept, &[], &["STARRED"]).await;
    }
    provider.modify(&kept, &["STARRED"], &["INBOX"]).await;
    provider.delete(&deleted).await;
    provider.deliver(&message("new", "")).await;
    let recorded: String = uplink
        .reader()
        .query_row(
            "SELECT json_extract(sync_state, '$.history_id') FROM account",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(provider.history_status(&recorded).await, 404);

    let sync = uplink.start_sync(&provider);
    uplink.arrived("new").await;
    eventually("the deletion to be observed", || async {
        uplink.entry("deleted").is_none().then_some(())
    })
    .await;
    assert_eq!(
        uplink.labels(kept_entry),
        [
            ("STARRED".into(), true, true),
            ("UNREAD".into(), true, true)
        ]
    );
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_mail_after_the_delay_unless_undone() {
    let uplink = Uplink::new();
    let provider = Provider::start(uplink.dir.path(), None).await;
    let me = provider.account.address.clone();
    provider.deliver(&message("original", "lunch?")).await;
    let sync = uplink.start_sync(&provider);
    let original_entry = uplink.arrived("original").await;
    let (original_message, _, original_thread) = uplink.message_of(original_entry);

    // A reply, to ourselves so that it arrives: queued for the delay, then
    // sent in the original's thread, and synced back as the same message.
    let nonce = common::nonce();
    let reply = format!("reply {nonce}");
    uplink.send(&me, &reply, Some(original_message)).await;
    assert!(matches!(
        &uplink.outbox().await[..],
        [(_, OutboxState::Queued)]
    ));
    let reply_entry = uplink.arrived("reply").await;
    let (_, reply_message_id, reply_thread) = uplink.message_of(reply_entry);
    provider.clean_up_later(uplink.external_id(reply_entry));
    assert_eq!(reply_thread, original_thread);
    let outbox_message_id: Option<String> = uplink
        .reader()
        .query_row("SELECT message_id FROM mail_outbox", [], |r| r.get(0))
        .optional()
        .unwrap();
    // Gone from the outbox once synced back; until then, it's the same
    // message.
    assert!(outbox_message_id.is_none_or(|id| id == reply_message_id));
    eventually("the outbox to empty", || async {
        uplink.outbox().await.is_empty().then_some(())
    })
    .await;
    let labels = uplink.labels(reply_entry);
    assert!(label_set(&labels).contains(&"SENT"), "{labels:?}");

    // Undone within the delay: back to drafts, and never sent.
    let undone = format!("undone {nonce}");
    let sent = uplink.send(&me, &undone, None).await;
    uplink
        .dispatcher
        .undo(uplink_protocol::TxLogId(sent))
        .await
        .unwrap();
    let ServerResponse::Drafts { drafts, .. } = uplink.mail.list_drafts().await.unwrap() else {
        panic!()
    };
    assert_eq!(drafts[0].content.subject, undone);
    tokio::time::sleep(SEND_DELAY * 3).await;
    assert!(uplink.outbox().await.is_empty());
    assert!(uplink.entry("undone").is_none());

    sync.abort();
    provider.clean_up().await;
}
