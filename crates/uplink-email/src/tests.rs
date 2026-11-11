use crate::{
    mailbox::{
        self, EntryLabels, LabelChange, LabelsPushed, LabelsRefused, MailboxObserved, NewEntry,
        ObservedLabel, SetLabels,
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
use uplink_tx_log::{push::PushFailing, Conflict, Dispatcher, Registry, UndoError};

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

    /// The mail accounts' outbox, as the server lists it.
    pub fn accounts_outbox(&self) -> Vec<uplink_protocol::compose::AccountOutbox> {
        let mut conn = self.reader();
        let tx = conn.transaction().unwrap();
        crate::outbox::outbox(&tx).unwrap()
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
        matches!(&entry.events[..], [ServerEvent::LabelsChanged(c), ServerEvent::OutboxChanged(o)]
            if c.changes == [LabelState {
                entry: db.entry("m2"), label: inbox, desired: true, observed: true,
            }] && o.accounts == ["a"])
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
async fn an_entry_is_in_one_folder() {
    let db = Db::new();
    let store = db.store();
    let folders = ["INBOX", "ARCHIVE", "TRASH", "Folder_1", "Folder_2"]
        .into_iter()
        .map(|id| ObservedLabel {
            external_id: id.into(),
            name: id.into(),
            system: !id.starts_with("Folder_"),
            folder: true,
        })
        .chain(labels(&["UNREAD"]).unwrap())
        .collect();
    db.observe(
        "a",
        observed(
            Some(folders),
            vec![new_entry(
                &store,
                "m1",
                &["INBOX", "UNREAD"],
                &raw("1@x", "s@x", DATE, "1", ""),
            )],
            vec![],
            &[],
        ),
    )
    .await;
    let m1 = db.entry("m1");
    let change = |label: &str, desired| LabelChange {
        entry: m1,
        label: db.label("a", label),
        desired,
    };
    let desired = |db: &Db| -> Vec<String> {
        db.label_rows()
            .into_iter()
            .filter(|r| r.2)
            .map(|r| r.1)
            .collect()
    };
    let mut live = db.hub.subscribe();

    // Archiving puts it in the Archive.
    let archived = db
        .dispatcher
        .dispatch(SetLabels {
            changes: vec![change("INBOX", false)],
        })
        .await
        .unwrap();
    assert_eq!(live.recv().await.unwrap().description, "Archived 1 message");
    assert_eq!(desired(&db), ["ARCHIVE", "UNREAD"]);
    // Undo takes it out again: a move, described as one.
    db.dispatcher.undo(archived).await.unwrap();
    assert_eq!(
        live.recv().await.unwrap().description,
        "Moved 1 message to Inbox"
    );
    assert_eq!(desired(&db), ["INBOX", "UNREAD"]);

    // Labelling with a folder moves it there; other labels stay.
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![change("Folder_1", true)],
        })
        .await
        .unwrap();
    assert_eq!(
        live.recv().await.unwrap().description,
        "Moved 1 message to 'Folder_1'"
    );
    assert_eq!(desired(&db), ["Folder_1", "UNREAD"]);
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![change("TRASH", true), change("UNREAD", false)],
        })
        .await
        .unwrap();
    live.recv().await.unwrap();
    assert_eq!(desired(&db), ["TRASH"]);

    // Into two folders at once: refused.
    let e = db
        .dispatcher
        .dispatch(SetLabels {
            changes: vec![change("Folder_1", true), change("Folder_2", true)],
        })
        .await
        .unwrap_err();
    assert_eq!(
        e.downcast::<Conflict>().unwrap().0,
        "a message is in one folder at a time"
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

/// A pending change names the op that made it, and the provider's refusal
/// of it, until it is reconciled or another op changes it (SPEC §6.6).
#[tokio::test]
async fn pending_changes_keep_their_op_and_refusal() {
    let db = Db::new();
    let store = db.store();
    db.observe(
        "a",
        observed(
            labels(ALL),
            vec![
                new_entry(&store, "m1", &["INBOX"], &raw("1@x", "s@x", DATE, "1", "")),
                new_entry(&store, "m2", &["INBOX"], &raw("2@x", "s@x", DATE, "2", "")),
            ],
            vec![],
            &[],
        ),
    )
    .await;
    let (m1, m2, inbox, starred) = (
        db.entry("m1"),
        db.entry("m2"),
        db.label("a", "INBOX"),
        db.label("a", "STARRED"),
    );
    let pending = || {
        db.query::<String>(
            "SELECT e.external_id || ' ' || l.external_id || ' ' || coalesce(desired_by, '-')
                    || ' ' || coalesce(refused, '-')
             FROM entry_label el JOIN mail_entry e ON e.id = el.entry
             JOIN label l ON l.id = el.label
             WHERE desired != observed OR desired_by IS NOT NULL OR refused IS NOT NULL
             ORDER BY 1",
        )
    };
    let change = |entry, label, desired| LabelChange {
        entry,
        label,
        desired,
    };
    let archived = db
        .dispatcher
        .dispatch(SetLabels {
            changes: vec![change(m1, inbox, false), change(m2, inbox, false)],
        })
        .await
        .unwrap()
        .0;
    let starred_by = db
        .dispatcher
        .dispatch(SetLabels {
            changes: vec![change(m1, starred, true)],
        })
        .await
        .unwrap()
        .0;
    assert_eq!(
        pending(),
        [
            format!("m1 INBOX {archived} -"),
            format!("m1 STARRED {starred_by} -"),
            format!("m2 INBOX {archived} -"),
        ]
    );

    // m1's archive is refused, m2's pushed; the star is refused too.
    let mut live = db.hub.subscribe();
    db.dispatcher
        .observe(
            "a".into(),
            LabelsRefused {
                changes: vec![change(m1, inbox, false), change(m1, starred, true)],
                reason: "no".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(&live.recv().await.unwrap().events[..],
        [ServerEvent::OutboxChanged(o)] if o.accounts == ["a"]));
    db.dispatcher
        .observe(
            "a".into(),
            LabelsPushed {
                changes: vec![change(m2, inbox, false)],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        pending(),
        [
            format!("m1 INBOX {archived} no"),
            format!("m1 STARRED {starred_by} no"),
        ]
    );

    // Refused changes are not pushed again.
    let reader = db.reader();
    let refused: i64 = reader
        .query_row(
            "SELECT count(*) FROM entry_label WHERE desired != observed AND refused IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(refused, 0);

    // Starring again is a change of its own, pushed anew; undoing the
    // archive leaves nothing pending of it.
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![change(m1, starred, false)],
        })
        .await
        .unwrap();
    let restarred = db
        .dispatcher
        .dispatch(SetLabels {
            changes: vec![change(m1, starred, true)],
        })
        .await
        .unwrap()
        .0;
    let undone = db.dispatcher.undo(TxLogId(archived)).await.unwrap().0;
    assert_eq!(
        pending(),
        [
            format!("m1 STARRED {restarred} -"),
            format!("m2 INBOX {undone} -"),
        ]
    );

    // The provider reporting them so reconciles them.
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
    assert!(pending().is_empty(), "{:?}", pending());
}

/// An account's failing pushes are recorded from when they start, and
/// cleared when they work again.
#[tokio::test]
async fn failing_pushes_are_recorded_from_the_first_failure() {
    let db = Db::new();
    db.observe("a", observed(labels(ALL), vec![], vec![], &[]))
        .await;
    let failing = || {
        db.reader()
            .query_row(
                "SELECT push_error, push_failing_since FROM account WHERE id = 'a'",
                [],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .unwrap()
    };
    for (error, at) in [("down", 10), ("still down", 20)] {
        db.dispatcher
            .observe(
                "a".into(),
                PushFailing {
                    error: Some(error.into()),
                    at,
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(failing(), (Some("still down".into()), Some(10)));
    db.dispatcher
        .observe(
            "a".into(),
            PushFailing {
                error: None,
                at: 30,
            },
        )
        .await
        .unwrap();
    assert_eq!(failing(), (None, None));
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
        mail_of(db)
    }

    /// The service over `db`, with accounts `a` and `b`.
    pub(crate) fn mail_of(db: &Db) -> Mail {
        Mail::new(
            db.dispatcher.clone(),
            db.reader(),
            db.dir.path(),
            vec![
                ConfiguredAccount {
                    id: "a".into(),
                    address: "a@x".into(),
                    provider: uplink_protocol::mail::MailProvider::Gmail,
                },
                ConfiguredAccount {
                    id: "b".into(),
                    address: "b@x".into(),
                    provider: uplink_protocol::mail::MailProvider::Gmail,
                },
            ],
            std::time::Duration::from_secs(10),
        )
    }

    fn subjects(response: &ServerResponse) -> Vec<String> {
        let ServerResponse::Conversations { conversations, .. } = response else {
            panic!("{response:?}")
        };
        conversations
            .iter()
            .map(|c| c.latest.subject.clone().unwrap())
            .collect()
    }

    /// The outbox lists, per account, what is pending by the op that made
    /// it, newest first, with what the provider refused and why the
    /// account is failing; refused changes are resolved by keeping what
    /// the provider has.
    #[tokio::test]
    async fn the_outbox_lists_pending_changes_by_op() {
        use crate::mailbox::LabelsRefused;
        use uplink_protocol::compose::{Failing, PendingChanges, PendingOp, Refusal};
        let db = Db::new();
        let mail = mail_with(&db).await;
        let (inbox, starred) = (db.label("a", "INBOX"), db.label("a", "STARRED"));
        let dispatched = |response: Result<ServerResponse, _>| match response {
            Ok(ServerResponse::Dispatched { tx_log_id }) => tx_log_id,
            other => panic!("{other:?}"),
        };
        let archived = dispatched(
            mail.modify_labels(vec![db.entry("m1"), db.entry("m2")], vec![], vec![inbox])
                .await,
        );
        let starred_op = dispatched(
            mail.modify_labels(vec![db.entry("m3")], vec![starred], vec![])
                .await,
        );
        db.dispatcher
            .observe(
                "a".into(),
                LabelsRefused {
                    changes: vec![LabelChange {
                        entry: db.entry("m1"),
                        label: inbox,
                        desired: false,
                    }],
                    reason: "no".into(),
                },
            )
            .await
            .unwrap();
        db.dispatcher
            .observe(
                "a".into(),
                PushFailing {
                    error: Some("down".into()),
                    at: 7,
                },
            )
            .await
            .unwrap();
        let changes = |a: &uplink_protocol::compose::AccountOutbox| match &a.pending {
            uplink_protocol::compose::Pending::Mail { changes, .. } => changes.clone(),
            other => panic!("{other:?}"),
        };
        let ts =
            |id: TxLogId| db.query::<i64>(&format!("SELECT ts FROM tx_log WHERE id = {}", id.0))[0];
        let op = |id, description: &str| {
            Some(PendingOp {
                id,
                description: description.into(),
                ts: ts(id),
            })
        };
        let accounts = db.accounts_outbox();
        let [a] = &accounts[..] else {
            panic!("{accounts:?}")
        };
        assert_eq!(a.account, "a");
        assert_eq!(
            a.failing,
            Some(Failing {
                error: "down".into(),
                since: 7
            })
        );
        assert_eq!(
            changes(a),
            [
                PendingChanges {
                    op: op(starred_op, "Starred 1 message"),
                    entries: 1,
                    refused: vec![],
                },
                PendingChanges {
                    op: op(archived, "Archived 2 messages"),
                    entries: 2,
                    refused: vec![Refusal {
                        reason: "no".into(),
                        entries: 1
                    }],
                },
            ]
        );

        // Keeping what Gmail has puts m1 back in the Inbox, as an op.
        let kept = dispatched(mail.keep_observed("a".into(), Some(archived)).await);
        assert_eq!(
            db.query::<String>(&format!(
                "SELECT description FROM tx_log WHERE id = {}",
                kept.0
            )),
            ["Moved 1 message to Inbox"]
        );
        let accounts = db.accounts_outbox();
        assert_eq!(
            changes(&accounts[0])[1],
            PendingChanges {
                op: op(archived, "Archived 2 messages"),
                entries: 1,
                refused: vec![],
            }
        );
        assert!(matches!(
            mail.keep_observed("a".into(), Some(archived)).await,
            Err(uplink_protocol::ServerError {
                code: ErrorCode::Conflict,
                ..
            })
        ));
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
    async fn accounts_report_the_first_listings_progress() {
        use crate::mailbox::SyncState;
        use uplink_protocol::mail::ListingProgress;
        let db = Db::new();
        let mail = mail_with(&db).await;
        let listing = |mail: &Mail| {
            let mail = mail.clone();
            async move {
                let ServerResponse::Accounts { accounts, .. } = mail.list_accounts().await.unwrap()
                else {
                    panic!()
                };
                (accounts[0].synced, accounts[0].listing)
            }
        };
        assert_eq!(listing(&mail).await, (false, None));
        let mut state = observed(None, vec![], vec![], &[]);
        state.sync_state = Some(SyncState::Listing {
            state: serde_json::json!({}),
            total: Some(12),
        });
        db.observe("a", state).await;
        assert_eq!(
            listing(&mail).await,
            (
                false,
                Some(ListingProgress {
                    stored: 5,
                    total: 12
                })
            )
        );
        let mut state = observed(None, vec![], vec![], &[]);
        state.sync_state = Some(SyncState::Synced(serde_json::json!({})));
        db.observe("a", state).await;
        assert_eq!(listing(&mail).await, (true, None));
    }

    /// A list's rows are conversations, each of the entries of a thread in
    /// the list's scope, listed by its newest whether or not that matches.
    #[tokio::test]
    async fn lists_are_of_conversations() {
        use uplink_protocol::mail::{Conversation, Sender};
        let db = Db::new();
        let store = db.store();
        // (id, thread, labels, sender, received_at)
        let mail: &[(&str, &str, &[&str], &str, i64)] = &[
            ("x1", "t1", &["INBOX"], "Alice <alice@x>", 1000),
            ("x2", "t1", &["SENT"], "a@x", 2000),
            ("x3", "t1", &["INBOX", "UNREAD"], "Bob <bob@x>", 3000),
            ("y1", "t2", &["INBOX"], "Alice <ALICE@x>", 1500),
            ("z1", "t3", &["TRASH"], "Carol <carol@x>", 4000),
            ("z2", "t3", &["INBOX"], "Carol <carol@x>", 500),
            ("w1", "t4", &["SENT"], "a@x", 5000),
        ];
        let entries = mail
            .iter()
            .map(|&(id, thread, labels, from, received_at)| {
                let mut e = new_entry(
                    &store,
                    id,
                    labels,
                    &raw(&format!("{id}@x"), from, DATE, id, &format!("body of {id}")),
                );
                e.thread_id = Some(thread.into());
                e.received_at = received_at;
                e
            })
            .collect();
        db.observe(
            "a",
            observed(
                labels(&["INBOX", "UNREAD", "SENT", "TRASH", "SPAM"]),
                entries,
                vec![],
                &[],
            ),
        )
        .await;
        let mail = mail_of(&db);
        let ids = |response: ServerResponse| -> Vec<Vec<String>> {
            let ServerResponse::Conversations { conversations, .. } = response else {
                panic!("{response:?}")
            };
            conversations
                .iter()
                .map(|c: &Conversation| {
                    let mut ids: Vec<_> = c.earlier.iter().map(|m| m.id).collect();
                    ids.push(c.latest.id);
                    let external = |id: EntryId| {
                        db.reader()
                            .query_row(
                                "SELECT external_id FROM mail_entry WHERE id = ?1",
                                [id.0],
                                |r| r.get::<_, String>(0),
                            )
                            .unwrap()
                    };
                    ids.into_iter().map(external).collect()
                })
                .collect()
        };
        let list = |label: &str, after, limit| {
            mail.list_entries(
                EntryList::Label {
                    label: db.label("a", label),
                },
                after,
                limit,
            )
        };
        // Bob's reply puts t1 first; z1, in Trash, is not part of t3 here.
        assert_eq!(
            ids(list("INBOX", None, 10).await.unwrap()),
            [vec!["x1", "x2", "x3"], vec!["y1"], vec!["z2"]]
        );
        assert_eq!(
            ids(list("SENT", None, 10).await.unwrap()),
            [vec!["w1"], vec!["x1", "x2", "x3"]]
        );
        assert_eq!(ids(list("TRASH", None, 10).await.unwrap()), [vec!["z1"]]);
        let all = || {
            mail.list_entries(
                EntryList::AllMail {
                    account: "a".into(),
                },
                None,
                10,
            )
        };
        assert_eq!(
            ids(all().await.unwrap()),
            [vec!["w1"], vec!["x1", "x2", "x3"], vec!["y1"], vec!["z2"]]
        );
        // Page by page, each conversation once.
        for list in [
            EntryList::Label {
                label: db.label("a", "INBOX"),
            },
            EntryList::AllMail {
                account: "a".into(),
            },
        ] {
            let mut paged = vec![];
            let mut after = None;
            loop {
                let response = mail.list_entries(list.clone(), after, 1).await.unwrap();
                let ServerResponse::Conversations { next, .. } = &response else {
                    panic!()
                };
                after = *next;
                paged.extend(ids(response));
                if after.is_none() {
                    break;
                }
            }
            let ServerResponse::Conversations { conversations, .. } =
                mail.list_entries(list.clone(), None, 10).await.unwrap()
            else {
                panic!()
            };
            assert_eq!(paged.len(), conversations.len(), "{list:?}");
        }
        // Senders once each, as first seen; attachments of any.
        let ServerResponse::Conversations { conversations, .. } =
            list("INBOX", None, 1).await.unwrap()
        else {
            panic!()
        };
        let sender = |name: Option<&str>, addr: &str| Sender {
            name: name.map(Into::into),
            addr: Some(addr.into()),
        };
        assert_eq!(
            conversations[0].senders,
            [
                sender(Some("Alice"), "alice@x"),
                sender(None, "a@x"),
                sender(Some("Bob"), "bob@x")
            ]
        );
        assert_eq!(conversations[0].earlier[1].labels, [db.label("a", "SENT")]);
        // Search finds the conversation of a match.
        let ServerResponse::Conversations { conversations, .. } =
            mail.search("x2".into(), 10).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].latest.id, db.entry("x3"));
        // A conversation's entries, Trash's too, oldest first.
        let ServerResponse::Entries { entries, .. } =
            mail.list_conversation(db.entry("z2")).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(
            entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            [db.entry("z2"), db.entry("z1")]
        );
        assert_eq!(
            mail.list_conversation(EntryId(999)).await.unwrap_err().code,
            ErrorCode::NotFound
        );
        // Counts are of conversations: t1, t2 and t3 in the Inbox, one unread.
        let ServerResponse::Accounts { accounts, .. } = mail.list_accounts().await.unwrap() else {
            panic!()
        };
        let inbox = accounts[0]
            .labels
            .iter()
            .find(|l| l.external_id == "INBOX")
            .unwrap();
        assert_eq!((inbox.total, inbox.unread), (3, 1));
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
        let ServerResponse::Conversations {
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
        let ServerResponse::Conversations {
            next: None,
            conversations,
            ..
        } = rest
        else {
            panic!()
        };
        let entries: Vec<_> = conversations.into_iter().map(|c| c.latest).collect();
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
    async fn the_inbox_of_every_account() {
        let db = Db::new();
        let mail = mail_with(&db).await;
        let store = db.store();
        let entry = |id: &str, labels: &[&str], received_at| {
            let mut e = new_entry(
                &store,
                id,
                labels,
                &raw(&format!("{id}@x"), "s@x", DATE, &format!("In b {id}"), ""),
            );
            e.received_at = received_at;
            e
        };
        let entries = vec![
            entry("b1", &["INBOX"], 3500),
            entry("b2", &["UNREAD"], 4500),
            entry("b3", &["INBOX", "TRASH"], 5500),
        ];
        db.observe(
            "b",
            observed(labels(&["INBOX", "UNREAD", "TRASH"]), entries, vec![], &[]),
        )
        .await;
        let inbox = |after, limit| mail.list_entries(EntryList::Inbox, after, limit);
        assert_eq!(
            subjects(&inbox(None, 10).await.unwrap()),
            [
                "Subject 5",
                "Subject 4",
                "In b b1",
                "Subject 3",
                "Subject 2",
                "Subject 1"
            ]
        );
        let ServerResponse::Conversations { next, .. } = inbox(None, 3).await.unwrap() else {
            panic!()
        };
        assert_eq!(
            subjects(&inbox(next, 10).await.unwrap()),
            ["Subject 3", "Subject 2", "Subject 1"]
        );
    }

    #[tokio::test]
    async fn an_entry_raw_as_its_account_has_it() {
        let db = Db::new();
        let mail = mail_with(&db).await;
        let raw = mail.raw_entry(db.entry("m2")).await.unwrap();
        assert!(String::from_utf8(raw)
            .unwrap()
            .starts_with("Message-ID: <2@x>\r\n"));
        assert_eq!(
            mail.raw_entry(EntryId(999)).await.unwrap_err().code,
            ErrorCode::NotFound
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
        let ServerResponse::Entries { entries, .. } =
            mail.list_message_entries(message).await.unwrap()
        else {
            panic!()
        };
        let mut accounts: Vec<_> = entries.iter().map(|e| e.account.as_str()).collect();
        accounts.sort();
        assert_eq!(accounts, ["a", "b"]);
        assert!(entries.iter().all(|e| e.message == message));
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
/// `thread`: the thread of message `id`, if any.
fn fill(conn: &mut Connection, messages: i64, thread: fn(i64) -> Option<String>) {
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
                                  to_addrs, cc_addrs, senders, snippet, has_attachments)
             VALUES (?1, ?1 || '@x', ?1, ?2, 's@x', '[]', '[]', '[]', ?3, 0)",
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
                "INSERT INTO mail_entry (account_id, external_id, message, variant, received_at,
                                         thread_id)
                 VALUES (?1, ?2, ?2, ?2, ?2 * 1000, ?3) RETURNING id",
                params![account, id, thread(id)],
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
    fill(&mut conn, 6000, |_| None);
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
            let page = query::conversations(&mut conn, &list, after, 250).unwrap();
            listed.extend(page.conversations.iter().map(|c| c.latest.id.0));
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
            .map(|c| c.latest.id.0)
            .collect();
        assert_eq!(found, expected, "{words}");
    }
}

/// Threads of up to 7 messages, of one account each (`fill` alternates
/// accounts), and some messages by themselves.
fn threads(id: i64) -> Option<String> {
    (id % 11 != 0).then(|| format!("t{}-{}", id % 2, id / 14))
}

/// Threaded lists, walked or sorted, list the conversations a plain model
/// of them does, page by page.
#[test]
fn large_threaded_lists_page_in_order() {
    use crate::query;
    use std::collections::{BTreeMap, BTreeSet};
    use uplink_protocol::mail::EntryList;

    let dir = tempfile::tempdir().unwrap();
    let mut conn = uplink_storage::open(dir.path()).unwrap();
    fill(&mut conn, 6000, threads);
    struct Row {
        id: i64,
        account: String,
        thread: Option<String>,
        received_at: i64,
        labels: BTreeSet<String>,
    }
    let mut rows: Vec<Row> = conn
        .prepare("SELECT id, account_id, thread_id, received_at FROM mail_entry")
        .unwrap()
        .query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                account: r.get(1)?,
                thread: r.get(2)?,
                received_at: r.get(3)?,
                labels: BTreeSet::new(),
            })
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for row in &mut rows {
        row.labels = conn
            .prepare(
                "SELECT l.external_id FROM entry_label el JOIN label l ON l.id = el.label
                 WHERE el.entry = ?1 AND el.desired",
            )
            .unwrap()
            .query_map([row.id], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    }
    let label = |name: &str| -> i64 {
        conn.query_row(
            "SELECT id FROM label WHERE account_id = 'a' AND external_id = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    };
    let hidden = |r: &Row| r.labels.contains("TRASH") || r.labels.contains("SPAM");
    // Each list: its scope, and what puts a conversation on it.
    type Test<'a> = (
        EntryList,
        Box<dyn Fn(&Row) -> bool + 'a>,
        Box<dyn Fn(&Row) -> bool + 'a>,
    );
    let tests: Vec<Test> = vec![
        // 3000 entries: walked.
        (
            EntryList::Label {
                label: LabelId(label("INBOX")),
            },
            Box::new(|r: &Row| !hidden(r)),
            Box::new(|r: &Row| r.account == "a" && r.labels.contains("INBOX")),
        ),
        // 600: sorted.
        (
            EntryList::Label {
                label: LabelId(label("UNREAD")),
            },
            Box::new(|r: &Row| !hidden(r)),
            Box::new(|r: &Row| r.account == "a" && r.labels.contains("UNREAD")),
        ),
        // 60: sorted, of Trash alone.
        (
            EntryList::Label {
                label: LabelId(label("TRASH")),
            },
            Box::new(|r: &Row| r.labels.contains("TRASH")),
            Box::new(|r: &Row| r.account == "a" && r.labels.contains("TRASH")),
        ),
        (
            EntryList::AllMail {
                account: "b".into(),
            },
            Box::new(|r: &Row| !hidden(r)),
            Box::new(|r: &Row| r.account == "b"),
        ),
        // Every account's: walked.
        (
            EntryList::Inbox,
            Box::new(|r: &Row| !hidden(r)),
            Box::new(|r: &Row| r.labels.contains("INBOX")),
        ),
    ];
    for (list, scope, matches) in tests {
        // Conversations by their newest entry, newest first, with their
        // entries oldest first.
        let mut conversations: BTreeMap<(String, String), Vec<&Row>> = BTreeMap::new();
        for row in rows.iter().filter(|r| scope(r)) {
            let key = match &row.thread {
                Some(t) => (row.account.clone(), t.clone()),
                None => (row.account.clone(), format!("#{}", row.id)),
            };
            conversations.entry(key).or_default().push(row);
        }
        let mut expected: Vec<Vec<i64>> = conversations
            .into_values()
            .filter(|c| c.iter().any(|r| matches(r)))
            .map(|mut c| {
                c.sort_by_key(|r| (r.received_at, r.id));
                c.iter().map(|r| r.id).collect()
            })
            .collect();
        expected.sort_by_key(|c| {
            let latest = &rows[rows
                .iter()
                .position(|r| r.id == *c.last().unwrap())
                .unwrap()];
            std::cmp::Reverse((latest.received_at, latest.id))
        });
        let mut listed = vec![];
        let mut after = None;
        loop {
            let page = query::conversations(&mut conn, &list, after, 250).unwrap();
            listed.extend(page.conversations.iter().map(|c| {
                let mut ids: Vec<i64> = c.earlier.iter().map(|m| m.id.0).collect();
                ids.push(c.latest.id.0);
                ids
            }));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(listed.len(), expected.len(), "{list:?}");
        assert_eq!(listed, expected, "{list:?}");
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
    fill(&mut conn, MESSAGES, threads);
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
        query::conversations(
            conn,
            &EntryList::Label {
                label: LabelId(inbox),
            },
            None,
            100,
        )
        .unwrap()
        .conversations
        .len()
    });
    time("All Mail, first page", &mut |conn| {
        query::conversations(
            conn,
            &EntryList::AllMail {
                account: "a".into(),
            },
            None,
            100,
        )
        .unwrap()
        .conversations
        .len()
    });
    time("accounts and counts", &mut |conn| {
        query::accounts(
            conn,
            &[query::ConfiguredAccount {
                id: "a".into(),
                address: "a@x".into(),
                provider: uplink_protocol::mail::MailProvider::Gmail,
            }],
        )
        .unwrap()
        .1
        .len()
    });
    eprintln!("slowest: {slowest:?}");
}

/// Messages stored before `has_attachments` was recorded get it from their
/// .eml once, before the dispatcher takes the writer.
#[tokio::test]
async fn attachments_are_indexed_for_messages_stored_before() {
    use crate::{mailbox::index_attachments, parse::tests::WITH_PARTS, query};
    use uplink_protocol::mail::EntryList;

    let db = Db::new();
    let store = db.store();
    db.observe(
        "a",
        observed(
            labels(&["INBOX"]),
            vec![
                new_entry(
                    &store,
                    "plain",
                    &["INBOX"],
                    &raw(
                        "p@x",
                        "s@x",
                        "Mon, 9 Nov 2026 10:00:00 +0000",
                        "plain",
                        "hi",
                    ),
                ),
                new_entry(&store, "parts", &["INBOX"], WITH_PARTS.as_bytes()),
            ],
            vec![],
            &[],
        ),
    )
    .await;
    let mut conn = uplink_storage::open(db.dir.path()).unwrap();
    let listed = |conn: &mut Connection| -> Vec<(String, bool)> {
        let inbox = db.label("a", "INBOX");
        let mut entries: Vec<_> =
            query::conversations(conn, &EntryList::Label { label: inbox }, None, 10)
                .unwrap()
                .conversations
                .into_iter()
                .map(|c| c.latest)
                .map(|e| {
                    (
                        e.snippet.chars().take(2).collect::<String>(),
                        e.has_attachments,
                    )
                })
                .collect();
        entries.sort();
        entries
    };
    let recorded = listed(&mut conn);
    conn.execute("UPDATE message SET has_attachments = NULL", [])
        .unwrap();
    assert_eq!(index_attachments(&mut conn, &store).unwrap(), 2);
    assert_eq!(listed(&mut conn), recorded);
    assert_eq!(
        recorded.iter().filter(|(_, has)| *has).count(),
        1,
        "{recorded:?}"
    );
    assert_eq!(index_attachments(&mut conn, &store).unwrap(), 0);
}

/// A provider whose sync takes `steps` steps, recording its calls.
struct Stepping {
    steps: std::sync::atomic::AtomicU32,
    calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl crate::account::Provider for Stepping {
    const KIND: &'static str = "gmail";

    async fn sync(&self, _: &crate::account::Account) -> anyhow::Result<bool> {
        self.calls.lock().unwrap().push("sync");
        Ok(self.steps.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) > 1)
    }

    async fn push(&self, _: &crate::account::Account) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push("push");
        Ok(())
    }

    async fn send(
        &self,
        _: &crate::account::Account,
        _: crate::account::Outgoing<'_>,
    ) -> Result<(), crate::account::SendError> {
        unreachable!("nothing is queued")
    }
}

/// A long sync (a first listing, of days) does not hold up pushes: they
/// go between its steps, which follow each other without waiting for the
/// poll interval.
#[tokio::test]
async fn pushes_go_between_the_steps_of_a_long_sync() {
    let db = Db::new();
    let calls = Arc::new(std::sync::Mutex::new(vec![]));
    let task = crate::account::SyncTask::new(
        Stepping {
            steps: 3.into(),
            calls: calls.clone(),
        },
        "a".into(),
        "a@x".into(),
        db.dispatcher.clone(),
        db.reader(),
        db.dir.path(),
        std::time::Duration::from_secs(3600),
    );
    let run = tokio::spawn(task.run());
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while calls.lock().unwrap().len() < 6 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:?}",
            calls.lock().unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // Done, it waits for the poll interval.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        *calls.lock().unwrap(),
        ["sync", "push", "sync", "push", "sync", "push"]
    );
    run.abort();
}

/// A provider whose pushes fail `failures` times, then work.
struct Failing {
    failures: std::sync::atomic::AtomicU32,
}

impl crate::account::Provider for Failing {
    const KIND: &'static str = "gmail";

    async fn sync(&self, _: &crate::account::Account) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn push(&self, _: &crate::account::Account) -> anyhow::Result<()> {
        let left = self.failures.load(std::sync::atomic::Ordering::SeqCst);
        if left > 0 {
            self.failures
                .store(left - 1, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("down {left}");
        }
        Ok(())
    }

    async fn send(
        &self,
        _: &crate::account::Account,
        _: crate::account::Outgoing<'_>,
    ) -> Result<(), crate::account::SendError> {
        unreachable!("nothing is queued")
    }
}

/// The task records when an account starts failing and when it recovers,
/// not each retry.
#[tokio::test(start_paused = true)]
async fn the_task_records_failing_as_it_starts_and_stops() {
    let db = Db::new();
    db.observe("a", observed(labels(ALL), vec![], vec![], &[]))
        .await;
    let task = crate::account::SyncTask::new(
        Failing { failures: 3.into() },
        "a".into(),
        "a@x".into(),
        db.dispatcher.clone(),
        db.reader(),
        db.dir.path(),
        std::time::Duration::from_secs(3600),
    );
    let run = tokio::spawn(task.run());
    // 1 + 2 + 4 s of backoff.
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    run.abort();
    assert_eq!(
        db.query::<String>(
            "SELECT description FROM tx_log WHERE op_type = 'account.push_failing' ORDER BY id"
        ),
        [
            "Changes are not reaching the provider: down 3",
            "Changes are reaching the provider again"
        ]
    );
}
