use crate::{
    mailbox::{
        self, EntryLabels, LabelChange, LabelsPushed, MailboxObserved, NewEntry, ObservedLabel,
        SetLabels,
    },
    parse::parse,
    store::Store,
};
use std::sync::Arc;
use tempfile::TempDir;
use uplink_protocol::{
    mail::{EntryId, LabelId, LabelState},
    ServerEvent, TxLogId,
};
use uplink_realtime::Hub;
use uplink_storage::rusqlite::{params, Connection};
use uplink_tx_log::{Dispatcher, Registry, UndoError};

pub(crate) struct Db {
    pub dir: TempDir,
    pub dispatcher: Dispatcher,
    pub hub: Arc<Hub>,
}

impl Db {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        mailbox::register(&mut registry);
        crate::compose::register(&mut registry);
        let hub = Arc::new(Hub::new(64));
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            hub.clone(),
        );
        Self {
            dir,
            dispatcher,
            hub,
        }
    }

    pub fn reader(&self) -> Connection {
        uplink_storage::open_reader(self.dir.path()).unwrap()
    }

    pub fn store(&self) -> Store {
        Store::mail(self.dir.path())
    }

    pub async fn observe(&self, account: &str, observation: MailboxObserved) -> TxLogId {
        self.dispatcher
            .observe(account.to_owned(), observation)
            .await
            .unwrap()
    }

    pub fn query<T: uplink_storage::rusqlite::types::FromSql>(&self, sql: &str) -> Vec<T> {
        self.reader()
            .prepare(sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// (entry external id, label external id, desired, observed), sorted.
    pub fn label_rows(&self) -> Vec<(String, String, bool, bool)> {
        self.reader()
            .prepare(
                "SELECT e.external_id, l.external_id, el.desired, el.observed
                 FROM entry_label el JOIN mail_entry e ON e.id = el.entry
                 JOIN label l ON l.id = el.label ORDER BY 1, 2",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    pub fn entry(&self, external_id: &str) -> EntryId {
        EntryId(
            self.reader()
                .query_row(
                    "SELECT id FROM mail_entry WHERE external_id = ?1",
                    [external_id],
                    |r| r.get(0),
                )
                .unwrap(),
        )
    }

    pub fn label(&self, account: &str, external_id: &str) -> LabelId {
        LabelId(
            self.reader()
                .query_row(
                    "SELECT id FROM label WHERE account_id = ?1 AND external_id = ?2",
                    [account, external_id],
                    |r| r.get(0),
                )
                .unwrap(),
        )
    }
}

pub(crate) fn raw(message_id: &str, from: &str, date: &str, subject: &str, body: &str) -> Vec<u8> {
    format!(
        "Message-ID: <{message_id}>\r\nFrom: {from}\r\nTo: me@example.com\r\n\
         Date: {date}\r\nSubject: {subject}\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

pub(crate) fn new_entry(store: &Store, external_id: &str, labels: &[&str], raw: &[u8]) -> NewEntry {
    NewEntry {
        external_id: external_id.to_owned(),
        thread_id: Some(format!("t-{external_id}")),
        received_at: 1_000,
        labels: labels.iter().map(|l| l.to_string()).collect(),
        variant: store.put(raw).unwrap(),
        size_bytes: raw.len() as i64,
        parsed: parse(raw),
    }
}

pub(crate) fn labels(ids: &[&str]) -> Option<Vec<ObservedLabel>> {
    Some(
        ids.iter()
            .map(|id| ObservedLabel {
                external_id: id.to_string(),
                name: id.to_string(),
                system: !id.starts_with("Label_"),
                folder: false,
            })
            .collect(),
    )
}

pub(crate) fn observed(
    labels: Option<Vec<ObservedLabel>>,
    added: Vec<NewEntry>,
    updated: Vec<(&str, &[&str])>,
    removed: &[&str],
) -> MailboxObserved {
    MailboxObserved {
        account_kind: "gmail",
        labels,
        added,
        updated: updated
            .into_iter()
            .map(|(external_id, labels)| EntryLabels {
                external_id: external_id.to_owned(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
            })
            .collect(),
        removed: removed.iter().map(|r| r.to_string()).collect(),
        sync_state: None,
    }
}

const DATE: &str = "Tue, 3 Nov 2026 10:00:00 +0000";
const ALL: &[&str] = &["INBOX", "UNREAD", "STARRED", "TRASH", "SPAM", "Label_1"];

fn row(entry: &str, label: &str, desired: bool, observed: bool) -> (String, String, bool, bool) {
    (entry.to_owned(), label.to_owned(), desired, observed)
}

#[tokio::test]
async fn new_entries_are_recorded_with_their_labels() {
    let db = Db::new();
    let store = db.store();
    let mut live = db.hub.subscribe();
    let id = db
        .observe(
            "a",
            observed(
                labels(ALL),
                vec![
                    new_entry(
                        &store,
                        "m1",
                        &["INBOX", "UNREAD"],
                        &raw("1@x", "s@x", DATE, "One", "first"),
                    ),
                    new_entry(
                        &store,
                        "m2",
                        &["INBOX"],
                        &raw("2@x", "s@x", DATE, "Two", "second"),
                    ),
                ],
                vec![],
                &[],
            ),
        )
        .await;
    assert_eq!(
        db.label_rows(),
        [
            row("m1", "INBOX", true, true),
            row("m1", "UNREAD", true, true),
            row("m2", "INBOX", true, true)
        ]
    );
    assert_eq!(
        db.query::<String>("SELECT subject FROM message ORDER BY id"),
        ["One", "Two"]
    );
    assert_eq!(db.query::<String>("SELECT kind FROM account"), ["gmail"]);

    let entry = live.recv().await.unwrap();
    assert_eq!(entry.id, id);
    assert_eq!(entry.description, "Received 2 messages");
    assert!(matches!(&entry.events[..], [
        ServerEvent::LabelListChanged(_),
        ServerEvent::EntriesAdded(added),
        ServerEvent::LabelsChanged(changed),
    ] if added.entries.len() == 2 && changed.changes.len() == 3));
}

#[tokio::test]
async fn copies_of_a_message_share_it() {
    let db = Db::new();
    let store = db.store();
    let direct = raw("1@x", "Alice <alice@x>", DATE, "Hi", "body");
    let via_list = format!(
        "Message-ID: <1@x>\r\nFrom: 'Alice' via L <list@lists.x>\r\n\
         X-Original-From: Alice <alice@x>\r\nList-Id: <l.lists.x>\r\n\
         Date: {DATE}\r\nSubject: [L] Hi\r\n\r\nbody\r\n"
    )
    .into_bytes();
    db.observe(
        "a",
        observed(
            labels(ALL),
            vec![new_entry(&store, "a1", &["INBOX"], &direct)],
            vec![],
            &[],
        ),
    )
    .await;
    db.observe(
        "b",
        observed(
            labels(ALL),
            vec![
                new_entry(&store, "b1", &["INBOX"], &via_list),
                // The same bytes again: the same variant.
                new_entry(&store, "b2", &["INBOX"], &direct),
            ],
            vec![],
            &[],
        ),
    )
    .await;
    assert_eq!(db.query::<i64>("SELECT count(*) FROM message"), [1]);
    assert_eq!(db.query::<i64>("SELECT count(*) FROM message_variant"), [2]);
    assert_eq!(
        db.query::<i64>("SELECT count(DISTINCT message) FROM mail_entry"),
        [1]
    );
    // The first copy stays the one shown.
    assert_eq!(db.query::<String>("SELECT subject FROM message"), ["Hi"]);
}

#[tokio::test]
async fn colliding_message_ids_stay_separate() {
    let db = Db::new();
    let store = db.store();
    let first = raw("1@x", "alice@x", DATE, "Real", "body");
    let other_sender = raw("1@x", "mallory@x", DATE, "Fake", "body");
    let other_date = raw(
        "1@x",
        "alice@x",
        "Wed, 4 Nov 2026 10:00:00 +0000",
        "Later",
        "body",
    );
    let no_id = b"From: alice@x\r\nSubject: None\r\n\r\nbody\r\n".to_vec();
    db.observe(
        "a",
        observed(
            labels(ALL),
            vec![
                new_entry(&store, "m1", &[], &first),
                new_entry(&store, "m2", &[], &other_sender),
                new_entry(&store, "m3", &[], &other_date),
                new_entry(&store, "m4", &[], &no_id),
            ],
            vec![],
            &[],
        ),
    )
    .await;
    let ids = db.query::<String>("SELECT message_id FROM message ORDER BY id");
    assert_eq!(ids[0], "1@x");
    for (synthetic, raw) in ids[1..].iter().zip([&other_sender, &other_date, &no_id]) {
        assert_eq!(
            *synthetic,
            format!(
                "{}@variant.uplink.invalid",
                crate::store::VariantHash::of(raw).0
            )
        );
    }
}

#[tokio::test]
async fn remote_label_changes_win_unless_a_local_change_is_pending() {
    let db = Db::new();
    let store = db.store();
    db.observe(
        "a",
        observed(
            labels(ALL),
            vec![
                new_entry(
                    &store,
                    "m1",
                    &["INBOX", "UNREAD"],
                    &raw("1@x", "s@x", DATE, "1", ""),
                ),
                new_entry(
                    &store,
                    "m2",
                    &["INBOX", "UNREAD"],
                    &raw("2@x", "s@x", DATE, "2", ""),
                ),
            ],
            vec![],
            &[],
        ),
    )
    .await;
    // The user archives m1 (pending), then the provider reports both read.
    let inbox = db.label("a", "INBOX");
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![LabelChange {
                entry: db.entry("m1"),
                label: inbox,
                desired: false,
            }],
        })
        .await
        .unwrap();
    db.observe(
        "a",
        observed(
            None,
            vec![],
            vec![("m1", &["INBOX", "STARRED"]), ("m2", &["INBOX"])],
            &[],
        ),
    )
    .await;
    assert_eq!(
        db.label_rows(),
        [
            row("m1", "INBOX", false, true),
            row("m1", "STARRED", true, true),
            row("m2", "INBOX", true, true),
        ]
    );
}

#[tokio::test]
async fn set_labels_undo_redo() {
    let db = Db::new();
    let store = db.store();
    db.observe(
        "a",
        observed(
            labels(ALL),
            vec![
                new_entry(&store, "m1", &["INBOX"], &raw("1@x", "s@x", DATE, "1", "")),
                new_entry(&store, "m2", &["INBOX"], &raw("2@x", "s@x", DATE, "2", "")),
                new_entry(&store, "m3", &[], &raw("3@x", "s@x", DATE, "3", "")),
            ],
            vec![],
            &[],
        ),
    )
    .await;
    let inbox = db.label("a", "INBOX");
    let archive = |entries: &[&str]| SetLabels {
        changes: entries
            .iter()
            .map(|e| LabelChange {
                entry: db.entry(e),
                label: inbox,
                desired: false,
            })
            .collect(),
    };
    let mut live = db.hub.subscribe();
    // m3 is not in the inbox: archiving it changes nothing.
    let archived = db
        .dispatcher
        .dispatch(archive(&["m1", "m2", "m3"]))
        .await
        .unwrap();
    assert_eq!(
        live.recv().await.unwrap().description,
        "Archived 2 messages"
    );
    assert_eq!(
        db.label_rows(),
        [
            row("m1", "INBOX", false, true),
            row("m2", "INBOX", false, true)
        ]
    );

    // m1 moves back to the inbox meanwhile; undo leaves it alone.
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![LabelChange {
                entry: db.entry("m1"),
                label: inbox,
                desired: true,
            }],
        })
        .await
        .unwrap();
    assert_eq!(
        live.recv().await.unwrap().description,
        "Moved 1 message to Inbox"
    );
    let undo = db.dispatcher.undo(archived).await.unwrap();
    let entry = live.recv().await.unwrap();
    assert_eq!(entry.description, "Moved 1 message to Inbox");
    assert!(
        matches!(&entry.events[..], [ServerEvent::LabelsChanged(c)] if c.changes == [LabelState {
            entry: db.entry("m2"), label: inbox, desired: true, observed: true,
        }])
    );
    assert_eq!(
        db.label_rows(),
        [
            row("m1", "INBOX", true, true),
            row("m2", "INBOX", true, true)
        ]
    );

    // Redo archives m2 only.
    db.dispatcher.undo(undo).await.unwrap();
    assert_eq!(live.recv().await.unwrap().description, "Archived 1 message");
    assert_eq!(
        db.label_rows(),
        [
            row("m1", "INBOX", true, true),
            row("m2", "INBOX", false, true)
        ]
    );
}

#[tokio::test]
async fn undo_refuses_once_the_message_is_gone() {
    let db = Db::new();
    let store = db.store();
    db.observe(
        "a",
        observed(
            labels(ALL),
            vec![new_entry(
                &store,
                "m1",
                &["INBOX"],
                &raw("1@x", "s@x", DATE, "1", ""),
            )],
            vec![],
            &[],
        ),
    )
    .await;
    let archived = db
        .dispatcher
        .dispatch(SetLabels {
            changes: vec![LabelChange {
                entry: db.entry("m1"),
                label: db.label("a", "INBOX"),
                desired: false,
            }],
        })
        .await
        .unwrap();
    db.observe("a", observed(None, vec![], vec![], &["m1"]))
        .await;
    assert!(db.label_rows().is_empty());
    assert_eq!(db.query::<i64>("SELECT count(*) FROM mail_entry"), [0]);
    // The message itself stays (invariant 4).
    assert_eq!(db.query::<i64>("SELECT count(*) FROM message"), [1]);
    assert!(matches!(
        db.dispatcher.undo(archived).await,
        Err(UndoError::Conflict { .. })
    ));
}

#[tokio::test]
async fn pushed_changes_become_observed() {
    let db = Db::new();
    let store = db.store();
    db.observe(
        "a",
        observed(
            labels(ALL),
            vec![new_entry(
                &store,
                "m1",
                &["INBOX"],
                &raw("1@x", "s@x", DATE, "1", ""),
            )],
            vec![],
            &[],
        ),
    )
    .await;
    let (m1, inbox, starred) = (
        db.entry("m1"),
        db.label("a", "INBOX"),
        db.label("a", "STARRED"),
    );
    let change = |label, desired| LabelChange {
        entry: m1,
        label,
        desired,
    };
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![change(inbox, false), change(starred, true)],
        })
        .await
        .unwrap();
    // The star is pushed, then unstarred again before the push is recorded.
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![change(starred, false)],
        })
        .await
        .unwrap();
    db.dispatcher
        .observe(
            "a".into(),
            LabelsPushed {
                changes: vec![change(inbox, false), change(starred, true)],
            },
        )
        .await
        .unwrap();
    assert_eq!(db.label_rows(), [row("m1", "STARRED", false, true)]);
}

#[tokio::test]
async fn labels_are_replaced_by_a_listing() {
    let db = Db::new();
    let store = db.store();
    db.observe(
        "a",
        observed(
            labels(ALL),
            vec![new_entry(
                &store,
                "m1",
                &["Label_1"],
                &raw("1@x", "s@x", DATE, "1", ""),
            )],
            vec![],
            &[],
        ),
    )
    .await;
    let mut renamed = labels(&["INBOX", "UNREAD", "STARRED"]).unwrap();
    renamed.push(ObservedLabel {
        external_id: "Label_2".into(),
        name: "New".into(),
        system: false,
        folder: false,
    });
    db.observe("a", observed(Some(renamed), vec![], vec![], &[]))
        .await;
    assert_eq!(
        db.query::<String>("SELECT name FROM label ORDER BY name"),
        ["INBOX", "New", "STARRED", "UNREAD"]
    );
    // Membership of the deleted label went with it.
    assert!(db.label_rows().is_empty());
    // An unchanged listing changes nothing.
    let mut live = db.hub.subscribe();
    db.observe(
        "a",
        observed(
            labels(&["INBOX", "UNREAD", "STARRED"]).map(|mut l| {
                l.push(ObservedLabel {
                    external_id: "Label_2".into(),
                    name: "New".into(),
                    system: false,
                    folder: false,
                });
                l
            }),
            vec![],
            vec![],
            &[],
        ),
    )
    .await;
    let entry = live.recv().await.unwrap();
    assert_eq!(entry.description, "Synced");
    assert!(entry.events.is_empty());
}

#[tokio::test]
async fn unknown_labels_are_refused() {
    let db = Db::new();
    let store = db.store();
    let result = db
        .dispatcher
        .observe(
            "a".into(),
            observed(
                labels(ALL),
                vec![new_entry(
                    &store,
                    "m1",
                    &["Label_9"],
                    &raw("1@x", "s@x", DATE, "1", ""),
                )],
                vec![],
                &[],
            ),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("label Label_9"));
    assert_eq!(db.query::<i64>("SELECT count(*) FROM message"), [0]);
}

pub(crate) mod service {
    use super::*;
    use crate::{query::ConfiguredAccount, service::Mail};
    use uplink_protocol::{
        mail::{EntryList, ListPosition},
        ErrorCode, ServerResponse,
    };

    pub(crate) async fn mail_with(db: &Db) -> Mail {
        let store = db.store();
        db.observe(
            "a",
            observed(
                labels(ALL),
                (1..=5)
                    .map(|i| {
                        let mut e = new_entry(
                            &store,
                            &format!("m{i}"),
                            if i % 2 == 0 {
                                &["INBOX"]
                            } else {
                                &["INBOX", "UNREAD"]
                            },
                            &raw(
                                &format!("{i}@x"),
                                "Sender <s@x>",
                                DATE,
                                &format!("Subject {i}"),
                                &format!("body number{i} common"),
                            ),
                        );
                        e.received_at = i * 1000;
                        e
                    })
                    .collect(),
                vec![],
                &[],
            ),
        )
        .await;
        db.observe(
            "b",
            observed(labels(&["INBOX", "UNREAD"]), vec![], vec![], &[]),
        )
        .await;
        Mail::new(
            db.dispatcher.clone(),
            db.reader(),
            db.dir.path(),
            vec![
                ConfiguredAccount {
                    id: "a".into(),
                    address: "a@x".into(),
                },
                ConfiguredAccount {
                    id: "b".into(),
                    address: "b@x".into(),
                },
            ],
            std::time::Duration::from_secs(10),
        )
    }

    fn subjects(response: &ServerResponse) -> Vec<String> {
        let ServerResponse::Entries { entries, .. } = response else {
            panic!("{response:?}")
        };
        entries.iter().map(|e| e.subject.clone().unwrap()).collect()
    }

    #[tokio::test]
    async fn accounts_with_label_counts() {
        let db = Db::new();
        let mail = mail_with(&db).await;
        let ServerResponse::Accounts { as_of, accounts } = mail.list_accounts().await.unwrap()
        else {
            panic!()
        };
        assert_eq!(as_of, TxLogId(2));
        let inbox = accounts[0]
            .labels
            .iter()
            .find(|l| l.external_id == "INBOX")
            .unwrap();
        assert_eq!((inbox.total, inbox.unread), (5, 3));
        assert_eq!(accounts[1].address, "b@x");
        assert!(!accounts[0].synced);
    }

    #[tokio::test]
    async fn entries_page_newest_first() {
        let db = Db::new();
        let mail = mail_with(&db).await;
        let inbox = db.label("a", "INBOX");
        let first = mail
            .list_entries(EntryList::Label { label: inbox }, None, 2)
            .await
            .unwrap();
        assert_eq!(subjects(&first), ["Subject 5", "Subject 4"]);
        let ServerResponse::Entries {
            next: Some(next), ..
        } = first
        else {
            panic!()
        };
        assert_eq!(
            next,
            ListPosition {
                received_at: 4000,
                entry: db.entry("m4")
            }
        );
        let rest = mail
            .list_entries(EntryList::Label { label: inbox }, Some(next), 10)
            .await
            .unwrap();
        assert_eq!(subjects(&rest), ["Subject 3", "Subject 2", "Subject 1"]);
        let ServerResponse::Entries {
            next: None,
            entries,
            ..
        } = rest
        else {
            panic!()
        };
        assert_eq!(entries[0].labels, [inbox, db.label("a", "UNREAD")]);
        assert_eq!(entries[0].snippet, "body number3 common");
        assert_eq!(
            mail.list_entries(
                EntryList::Label {
                    label: LabelId(999)
                },
                None,
                2
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            mail.list_entries(EntryList::Label { label: inbox }, None, 0)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
    }

    #[tokio::test]
    async fn search_matches_word_prefixes() {
        let db = Db::new();
        let mail = mail_with(&db).await;
        assert_eq!(
            subjects(&mail.search("number2".into(), 10).await.unwrap()),
            ["Subject 2"]
        );
        assert_eq!(
            subjects(&mail.search("COMM sender".into(), 2).await.unwrap()),
            ["Subject 5", "Subject 4"]
        );
        assert_eq!(
            subjects(&mail.search("subject 3".into(), 10).await.unwrap()),
            ["Subject 3"]
        );
        // Query syntax is not interpreted.
        assert!(subjects(&mail.search("\"a OR b\" NEAR(".into(), 10).await.unwrap()).is_empty());
        assert_eq!(
            mail.search("  ".into(), 10).await.unwrap_err().code,
            ErrorCode::Invalid
        );
    }

    #[tokio::test]
    async fn message_content() {
        let db = Db::new();
        let mail = mail_with(&db).await;
        let message = message_of(&db, "m2");
        let ServerResponse::Message { message } = mail.load_message(message).await.unwrap() else {
            panic!()
        };
        assert_eq!(message.message_id, "2@x");
        assert_eq!(message.from_name.as_deref(), Some("Sender"));
        assert_eq!(message.body_text, "body number2 common\r\n");
        assert_eq!(
            mail.load_message(uplink_protocol::mail::MessageId(999))
                .await
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }

    #[tokio::test]
    async fn trash_and_spam_hide_entries_elsewhere() {
        let db = Db::new();
        let mail = mail_with(&db).await;
        let (inbox, trash, spam, label) = (
            db.label("a", "INBOX"),
            db.label("a", "TRASH"),
            db.label("a", "SPAM"),
            db.label("a", "Label_1"),
        );
        let (m1, m2, m3) = (db.entry("m1"), db.entry("m2"), db.entry("m3"));
        mail.modify_labels(vec![m1, m2], vec![label], vec![])
            .await
            .unwrap();
        mail.modify_labels(vec![m1], vec![trash], vec![])
            .await
            .unwrap();
        mail.modify_labels(vec![m3], vec![spam], vec![])
            .await
            .unwrap();
        let list = |label| mail.list_entries(EntryList::Label { label }, None, 10);
        assert_eq!(
            subjects(&list(inbox).await.unwrap()),
            ["Subject 5", "Subject 4", "Subject 2"]
        );
        assert_eq!(subjects(&list(label).await.unwrap()), ["Subject 2"]);
        assert_eq!(subjects(&list(trash).await.unwrap()), ["Subject 1"]);
        assert_eq!(subjects(&list(spam).await.unwrap()), ["Subject 3"]);
        let all_mail = |account: &str| {
            mail.list_entries(
                EntryList::AllMail {
                    account: account.into(),
                },
                None,
                10,
            )
        };
        assert_eq!(
            subjects(&all_mail("a").await.unwrap()),
            ["Subject 5", "Subject 4", "Subject 2"]
        );
        assert!(subjects(&all_mail("b").await.unwrap()).is_empty());
        assert_eq!(all_mail("c").await.unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(
            subjects(&mail.search("common".into(), 10).await.unwrap()),
            ["Subject 5", "Subject 4", "Subject 2"]
        );
        let ServerResponse::Accounts { accounts, .. } = mail.list_accounts().await.unwrap() else {
            panic!()
        };
        let counts = |id: LabelId| {
            let l = accounts[0].labels.iter().find(|l| l.id == id).unwrap();
            (l.total, l.unread)
        };
        // m1 and m3 were the unread ones besides m5.
        assert_eq!(counts(inbox), (3, 1));
        assert_eq!(counts(label), (1, 0));
        assert_eq!(counts(trash), (1, 1));
        assert_eq!(counts(spam), (1, 1));
    }

    #[tokio::test]
    async fn message_entries_of_every_account() {
        let db = Db::new();
        let mail = mail_with(&db).await;
        let store = db.store();
        // A copy of message 2 in account b.
        db.observe(
            "b",
            observed(
                labels(&["INBOX", "UNREAD"]),
                vec![new_entry(
                    &store,
                    "b2",
                    &["INBOX"],
                    &raw(
                        "2@x",
                        "Sender <s@x>",
                        DATE,
                        "Subject 2",
                        "body number2 common",
                    ),
                )],
                vec![],
                &[],
            ),
        )
        .await;
        let message = message_of(&db, "m2");
        let ServerResponse::Entries { entries, next, .. } =
            mail.list_message_entries(message).await.unwrap()
        else {
            panic!()
        };
        let mut accounts: Vec<_> = entries.iter().map(|e| e.account.as_str()).collect();
        accounts.sort();
        assert_eq!(accounts, ["a", "b"]);
        assert!(entries.iter().all(|e| e.message == message));
        assert_eq!(next, None);
        assert_eq!(
            mail.list_message_entries(uplink_protocol::mail::MessageId(999))
                .await
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }

    fn message_of(db: &Db, external_id: &str) -> uplink_protocol::mail::MessageId {
        uplink_protocol::mail::MessageId(
            db.reader()
                .query_row(
                    "SELECT message FROM mail_entry WHERE external_id = ?1",
                    [external_id],
                    |r| r.get(0),
                )
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn modify_labels() {
        let db = Db::new();
        let mail = mail_with(&db).await;
        let (inbox, unread) = (db.label("a", "INBOX"), db.label("a", "UNREAD"));
        let response = mail
            .modify_labels(
                vec![db.entry("m1"), db.entry("m2")],
                vec![],
                vec![inbox, unread],
            )
            .await
            .unwrap();
        assert!(matches!(response, ServerResponse::Dispatched { .. }));
        assert_eq!(
            db.label_rows()
                .iter()
                .filter(|r| r.0 == "m1" || r.0 == "m2")
                .cloned()
                .collect::<Vec<_>>(),
            [
                row("m1", "INBOX", false, true),
                row("m1", "UNREAD", false, true),
                row("m2", "INBOX", false, true)
            ]
        );

        let other = db.label("b", "INBOX");
        let code = |r: Result<ServerResponse, uplink_protocol::ServerError>| r.unwrap_err().code;
        assert_eq!(
            code(
                mail.modify_labels(vec![db.entry("m1")], vec![other], vec![])
                    .await
            ),
            ErrorCode::Invalid
        );
        assert_eq!(
            code(
                mail.modify_labels(vec![db.entry("m1")], vec![inbox], vec![other])
                    .await
            ),
            ErrorCode::Invalid
        );
        assert_eq!(
            code(
                mail.modify_labels(vec![db.entry("m1")], vec![inbox], vec![inbox])
                    .await
            ),
            ErrorCode::Invalid
        );
        assert_eq!(
            code(
                mail.modify_labels(vec![EntryId(999)], vec![inbox], vec![])
                    .await
            ),
            ErrorCode::NotFound
        );
        assert_eq!(
            code(
                mail.modify_labels(vec![db.entry("m1")], vec![LabelId(999)], vec![])
                    .await
            ),
            ErrorCode::NotFound
        );
    }
}

/// Fills an empty database with `messages` messages over accounts `a` and
/// `b` (even and odd ids), as a mailbox looks to the queries: some words
/// very common and most rare (word `i` drawn with weight about 1/i), most
/// messages in the Inbox, a fifth unread, one in fifty in Trash.
fn fill(conn: &mut Connection, messages: i64) {
    let words: Vec<String> = (0..5000).map(|i| format!("w{i}")).collect();
    let mut seed = 1u64;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut word = || {
        let u = (random() % 1_000_000) as f64 / 1_000_000.0;
        &words[((5000f64.powf(u)) as usize - 1).min(4999)]
    };
    let tx = conn.transaction().unwrap();
    for account in ["a", "b"] {
        tx.execute(
            "INSERT INTO account (id, kind) VALUES (?1, 'gmail')",
            [account],
        )
        .unwrap();
        for label in ["INBOX", "UNREAD", "TRASH", "SPAM"] {
            tx.execute(
                "INSERT INTO label (account_id, external_id, name, system) VALUES (?1, ?2, ?2, 1)",
                [account, label],
            )
            .unwrap();
        }
    }
    for id in 1..=messages {
        let subject: Vec<&str> = (0..6).map(|_| word().as_str()).collect();
        let body: Vec<&str> = (0..80).map(|_| word().as_str()).collect();
        let (subject, body) = (subject.join(" "), body.join(" "));
        tx.execute(
            "INSERT INTO message (id, message_id, preferred_variant, subject, from_addr,
                                  to_addrs, cc_addrs, senders, snippet)
             VALUES (?1, ?1 || '@x', ?1, ?2, 's@x', '[]', '[]', '[]', ?3)",
            params![id, subject, &body[..60]],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO message_variant (sha256, message, source, size_bytes)
             VALUES (?1, ?1, 'a', 1)",
            [id],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO message_fts (rowid, subject, from_addr, from_name, to_addrs, body_text)
             VALUES (?1, ?2, 's@x', '', '', ?3)",
            params![id, subject, body],
        )
        .unwrap();
        let account = if id % 2 == 0 { "a" } else { "b" };
        let entry: i64 = tx
            .query_row(
                "INSERT INTO mail_entry (account_id, external_id, message, variant, received_at)
                 VALUES (?1, ?2, ?2, ?2, ?2 * 1000) RETURNING id",
                params![account, id],
                |r| r.get(0),
            )
            .unwrap();
        // Most in the Inbox, a fifth unread, a few in Trash.
        let mut labels = vec!["INBOX"];
        if id % 5 == 0 {
            labels.push("UNREAD");
        }
        if id % 50 == 0 {
            labels = vec!["TRASH"];
        }
        for label in labels {
            tx.execute(
                "INSERT INTO entry_label (entry, label, desired, observed)
                 SELECT ?1, id, 1, 1 FROM label WHERE account_id = ?2 AND external_id = ?3",
                params![entry, account, label],
            )
            .unwrap();
        }
    }
    tx.commit().unwrap();
    conn.execute_batch("ANALYZE").unwrap();
}

/// Lists and searches with many candidates walk entries newest first
/// rather than sorting the candidates; both give the order plain queries
/// do, page by page.
#[test]
fn large_lists_and_searches_page_in_order() {
    use crate::query;
    use uplink_protocol::mail::EntryList;

    let dir = tempfile::tempdir().unwrap();
    let mut conn = uplink_storage::open(dir.path()).unwrap();
    fill(&mut conn, 6000);
    let plain = |conn: &Connection, sql: &str| -> Vec<i64> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let unhidden = "NOT EXISTS (SELECT 1 FROM entry_label hl JOIN label h ON h.id = hl.label
                                WHERE hl.entry = e.id AND hl.desired
                                  AND h.external_id IN ('TRASH', 'SPAM'))";
    let order = "ORDER BY e.received_at DESC, e.id DESC";
    let label = |conn: &Connection, name: &str| -> i64 {
        conn.query_row(
            "SELECT id FROM label WHERE account_id = 'a' AND external_id = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    };
    let (inbox, unread) = (label(&conn, "INBOX"), label(&conn, "UNREAD"));
    for (list, sql) in [
        // 3000 entries: walked.
        (
            EntryList::Label {
                label: LabelId(inbox),
            },
            format!(
                "SELECT e.id FROM mail_entry e JOIN entry_label el ON el.entry = e.id
                 WHERE el.label = {inbox} AND el.desired AND {unhidden} {order}"
            ),
        ),
        // 600: sorted.
        (
            EntryList::Label {
                label: LabelId(unread),
            },
            format!(
                "SELECT e.id FROM mail_entry e JOIN entry_label el ON el.entry = e.id
                 WHERE el.label = {unread} AND el.desired AND {unhidden} {order}"
            ),
        ),
        (
            EntryList::AllMail {
                account: "b".into(),
            },
            format!(
                "SELECT e.id FROM mail_entry e WHERE e.account_id = 'b' AND {unhidden} {order}"
            ),
        ),
    ] {
        let mut listed = vec![];
        let mut after = None;
        loop {
            let page = query::entries(&mut conn, &list, after, 250).unwrap();
            listed.extend(page.entries.iter().map(|e| e.id.0));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(listed, plain(&conn, &sql), "{list:?}");
    }
    // Common words (walked), rare ones (sorted), and both.
    for words in ["w0", "w1 w2", "w4321", "w12 w3000"] {
        let fts = words
            .split(' ')
            .map(|w| format!("\"{w}\"*"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut expected = plain(
            &conn,
            &format!(
                "SELECT e.id FROM message_fts f JOIN mail_entry e ON e.message = f.rowid
                 WHERE message_fts MATCH '{fts}' AND {unhidden} {order}"
            ),
        );
        expected.truncate(100);
        let found: Vec<i64> = query::search(&mut conn, words, 100)
            .unwrap()
            .1
            .iter()
            .map(|e| e.id.0)
            .collect();
        assert_eq!(found, expected, "{words}");
    }
}

/// How fast search and listing are over a mailbox of realistic size (SPEC
/// §14 M10: under 100 ms). Slow to set up, so run on demand:
/// `cargo test --release -p uplink-email -- --ignored --nocapture speed`.
#[test]
#[ignore]
fn speed_of_search_and_listing() {
    use crate::query;
    use std::time::Instant;
    use uplink_protocol::mail::EntryList;

    const MESSAGES: i64 = 50_000;
    let dir = tempfile::tempdir().unwrap();
    let mut conn = uplink_storage::open(dir.path()).unwrap();
    let started = Instant::now();
    fill(&mut conn, MESSAGES);
    eprintln!("set up {MESSAGES} messages in {:?}", started.elapsed());

    let inbox: i64 = conn
        .query_row(
            "SELECT id FROM label WHERE account_id = 'a' AND external_id = 'INBOX'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut slowest = std::time::Duration::ZERO;
    let mut time = |what: &str, f: &mut dyn FnMut(&mut Connection) -> usize| {
        // The first run warms the page cache, as a running server's is.
        f(&mut conn);
        let started = Instant::now();
        let found = f(&mut conn);
        let took = started.elapsed();
        slowest = slowest.max(took);
        eprintln!("{what}: {found} in {took:?}");
    };
    for query in ["w0", "w1 w2", "w4321", "w12 w3000", "w"] {
        time(&format!("search {query:?}"), &mut |conn| {
            query::search(conn, query, 100).unwrap().1.len()
        });
    }
    time("Inbox, first page", &mut |conn| {
        query::entries(
            conn,
            &EntryList::Label {
                label: LabelId(inbox),
            },
            None,
            100,
        )
        .unwrap()
        .entries
        .len()
    });
    time("All Mail, first page", &mut |conn| {
        query::entries(
            conn,
            &EntryList::AllMail {
                account: "a".into(),
            },
            None,
            100,
        )
        .unwrap()
        .entries
        .len()
    });
    time("accounts and counts", &mut |conn| {
        query::accounts(
            conn,
            &[query::ConfiguredAccount {
                id: "a".into(),
                address: "a@x".into(),
            }],
        )
        .unwrap()
        .1
        .len()
    });
    eprintln!("slowest: {slowest:?}");
}
