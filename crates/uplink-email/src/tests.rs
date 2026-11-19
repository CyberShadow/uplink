use crate::{
    mailbox::{
        self, EntryLabels, LabelChange, MailLabels, MailboxObserved, NewEntry, ObservedLabel,
        SetLabels,
    },
    parse::parse,
};
use std::sync::Arc;
use tempfile::TempDir;
use uplink_protocol::{
    mail::{EntryId, LabelId, LabelKind, LabelState},
    ServerEvent, TxLogId,
};
use uplink_realtime::Hub;
use uplink_storage::rusqlite::{params, Connection};
use uplink_storage::store::Store;
use uplink_tx_log::{
    push::PushFailing,
    reconcile::Ended,
    reconciler::{Claimed, NewKey, Push, Pushed},
    Conflict, Dispatcher, Registry, UndoError,
};

/// The fields of `changes`.
fn keys(changes: &[LabelChange]) -> Vec<(EntryId, LabelId)> {
    changes.iter().map(|c| (c.entry, c.label)).collect()
}

/// The identities of an account of `address` alone, unnamed.
pub(crate) fn own(address: &str) -> Vec<uplink_protocol::mail::Identity> {
    vec![uplink_protocol::mail::Identity::Address {
        address: address.into(),
        name: None,
    }]
}

/// How many of `placed` are in each label, and of each account.
#[allow(clippy::type_complexity)]
fn totals(placed: &[crate::query::Placed]) -> (Vec<(LabelId, u64)>, Vec<(String, u64)>) {
    let mut labels = std::collections::BTreeMap::<LabelId, u64>::new();
    let mut accounts = std::collections::BTreeMap::<String, u64>::new();
    for p in placed {
        for label in &p.labels {
            *labels.entry(*label).or_default() += 1;
        }
        *accounts.entry(p.account.clone()).or_default() += 1;
    }
    (labels.into_iter().collect(), accounts.into_iter().collect())
}

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
        crate::image_rules::register(&mut registry);
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

    /// Records `observation`, by requests sent now.
    pub async fn observe(&self, account: &str, observation: MailboxObserved) -> TxLogId {
        let as_of = uplink_tx_log::as_of(&self.reader()).unwrap();
        self.dispatcher
            .observe(
                account.to_owned(),
                MailboxObserved {
                    as_of,
                    ..observation
                },
            )
            .await
            .unwrap()
    }

    /// The reconciler claims the pushes of `changes`, which end so.
    pub async fn push(&self, account: &str, changes: Vec<LabelChange>, ended: Ended) {
        self.dispatcher
            .observe(
                account.to_owned(),
                Claimed::<MailLabels>::new(changes.iter().map(|c| (c.entry, c.label)).collect()),
            )
            .await
            .unwrap();
        self.dispatcher
            .observe(
                account.to_owned(),
                Pushed::<MailLabels> {
                    pushes: vec![Push {
                        fields: keys(&changes),
                        ended,
                        rekeyed: None,
                    }],
                },
            )
            .await
            .unwrap();
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
                kind: LabelKind::Tag,
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
        rekeyed: vec![],
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
        // As `Db::observe` records it.
        as_of: TxLogId(0),
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
                uplink_storage::store::VariantHash::of(raw).0
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
            shown: false,
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

/// Lists show conversations: a change says how many its messages are of,
/// unless each is of one of its own.
#[tokio::test]
async fn set_labels_counts_conversations() {
    let db = Db::new();
    let store = db.store();
    let threaded = |id: &str, n: &str| NewEntry {
        thread_id: Some("t".into()),
        ..new_entry(
            &store,
            id,
            &["INBOX", "UNREAD"],
            &raw(&format!("{n}@x"), "s@x", DATE, n, ""),
        )
    };
    db.observe(
        "a",
        observed(
            labels(ALL),
            vec![
                threaded("m1", "1"),
                threaded("m2", "2"),
                new_entry(
                    &store,
                    "m3",
                    &["INBOX", "UNREAD"],
                    &raw("3@x", "s@x", DATE, "3", ""),
                ),
            ],
            vec![],
            &[],
        ),
    )
    .await;
    let change = |label: &str, entries: &[&str]| SetLabels {
        changes: entries
            .iter()
            .map(|e| LabelChange {
                entry: db.entry(e),
                label: db.label("a", label),
                desired: false,
            })
            .collect(),
        shown: false,
    };
    let mut live = db.hub.subscribe();
    db.dispatcher
        .dispatch(change("UNREAD", &["m1", "m2"]))
        .await
        .unwrap();
    assert_eq!(
        live.recv().await.unwrap().description,
        "Marked 2 messages in 1 conversation read"
    );
    db.dispatcher
        .dispatch(change("INBOX", &["m1", "m2", "m3"]))
        .await
        .unwrap();
    assert_eq!(
        live.recv().await.unwrap().description,
        "Archived 3 messages in 2 conversations"
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
        shown: false,
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
            shown: false,
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
            kind: LabelKind::Folder,
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
            shown: false,
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
            shown: false,
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
            shown: false,
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
            shown: false,
        })
        .await
        .unwrap_err();
    assert_eq!(
        e.downcast::<Conflict>().unwrap().0,
        "a message is in one folder at a time"
    );
}

#[tokio::test]
async fn an_entry_is_in_mailboxes_or_in_one_folder() {
    let db = Db::new();
    let store = db.store();
    let label = |id: &str, kind| ObservedLabel {
        external_id: id.into(),
        name: id.into(),
        system: !id.starts_with("mailbox:"),
        kind,
    };
    let places = ["INBOX", "mailbox:x", "mailbox:y"]
        .map(|id| label(id, LabelKind::Mailbox))
        .into_iter()
        .chain(["TRASH", "ARCHIVE"].map(|id| label(id, LabelKind::Folder)))
        .chain(labels(&["UNREAD"]).unwrap())
        .collect();
    db.observe(
        "a",
        observed(
            Some(places),
            vec![new_entry(
                &store,
                "m1",
                &["INBOX", "UNREAD", "mailbox:x"],
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
    let set = |changes| SetLabels {
        changes,
        shown: false,
    };
    let mut live = db.hub.subscribe();

    // Trashing takes it out of every mailbox, as one move; undo puts it
    // back in them.
    let trashed = db
        .dispatcher
        .dispatch(set(vec![change("TRASH", true)]))
        .await
        .unwrap();
    assert_eq!(
        live.recv().await.unwrap().description,
        "Moved 1 message to Trash"
    );
    assert_eq!(desired(&db), ["TRASH", "UNREAD"]);
    db.dispatcher.undo(trashed).await.unwrap();
    live.recv().await.unwrap();
    assert_eq!(desired(&db), ["INBOX", "UNREAD", "mailbox:x"]);

    // Archived, it is still in its other mailbox; out of its last one, it
    // is in the Archive, which another mailbox takes it out of.
    db.dispatcher
        .dispatch(set(vec![change("INBOX", false)]))
        .await
        .unwrap();
    assert_eq!(live.recv().await.unwrap().description, "Archived 1 message");
    assert_eq!(desired(&db), ["UNREAD", "mailbox:x"]);
    db.dispatcher
        .dispatch(set(vec![change("mailbox:x", false)]))
        .await
        .unwrap();
    live.recv().await.unwrap();
    assert_eq!(desired(&db), ["ARCHIVE", "UNREAD"]);
    db.dispatcher
        .dispatch(set(vec![change("mailbox:y", true)]))
        .await
        .unwrap();
    assert_eq!(
        live.recv().await.unwrap().description,
        "Labelled 1 message 'mailbox:y'"
    );
    assert_eq!(desired(&db), ["UNREAD", "mailbox:y"]);

    // Into a folder and a mailbox at once: refused.
    let e = db
        .dispatcher
        .dispatch(set(vec![change("TRASH", true), change("mailbox:x", true)]))
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
            shown: false,
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
async fn pushed_changes_become_observed_as_read_back() {
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
            shown: false,
        })
        .await
        .unwrap();
    // The star is pushed, then unstarred again before the push ends.
    let claimed = vec![change(inbox, false), change(starred, true)];
    db.dispatcher
        .observe(
            "a".into(),
            Claimed::<MailLabels>::new(claimed.iter().map(|c| (c.entry, c.label)).collect()),
        )
        .await
        .unwrap();
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![change(starred, false)],
            shown: false,
        })
        .await
        .unwrap();
    db.dispatcher
        .observe(
            "a".into(),
            Pushed::<MailLabels> {
                pushes: vec![Push {
                    fields: keys(&claimed),
                    ended: Ended::Applied,
                    rekeyed: None,
                }],
            },
        )
        .await
        .unwrap();
    // Applied, not yet seen so.
    assert_eq!(
        db.label_rows(),
        [
            row("m1", "INBOX", false, true),
            row("m1", "STARRED", false, false)
        ]
    );
    db.observe("a", observed(None, vec![], vec![("m1", &["STARRED"])], &[]))
        .await;
    assert_eq!(db.label_rows(), [row("m1", "STARRED", false, true)]);
    // The unstar is pushed after the star.
    let reconciling = mailbox::reconciling(&db.reader(), "a").unwrap();
    let wanted: Vec<_> = reconciling
        .iter()
        .filter_map(|r| r.field.wanted().map(|w| (r.key.1, *w)))
        .collect();
    assert_eq!(wanted, [(starred, false)]);
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
                    || ' ' || coalesce(decision ->> '$.refused', '-')
             FROM entry_label el JOIN mail_entry e ON e.id = el.entry
             JOIN label l ON l.id = el.label
             WHERE desired != observed OR desired_by IS NOT NULL OR decision IS NOT NULL
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
            shown: false,
        })
        .await
        .unwrap()
        .0;
    let starred_by = db
        .dispatcher
        .dispatch(SetLabels {
            changes: vec![change(m1, starred, true)],
            shown: false,
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

    // m1's archive is refused, m2's pushed and seen so; the star is
    // refused too.
    let mut live = db.hub.subscribe();
    db.push(
        "a",
        vec![change(m1, inbox, false), change(m1, starred, true)],
        Ended::NotApplied {
            refused: Some("no".into()),
        },
    )
    .await;
    live.recv().await.unwrap();
    assert!(matches!(&live.recv().await.unwrap().events[..],
        [ServerEvent::OutboxChanged(o)] if o.accounts == ["a"]));
    db.push("a", vec![change(m2, inbox, false)], Ended::Applied)
        .await;
    db.observe("a", observed(None, vec![], vec![("m2", &[])], &[]))
        .await;
    assert_eq!(
        pending(),
        [
            format!("m1 INBOX {archived} no"),
            format!("m1 STARRED {starred_by} no"),
        ]
    );

    // Refused changes are not pushed again.
    let reconciling = mailbox::reconciling(&db.reader(), "a").unwrap();
    assert!(reconciling.iter().all(|r| r.field.wanted().is_none()));

    // Starring again is a change of its own, pushed anew; undoing the
    // archive leaves nothing pending of it.
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![change(m1, starred, false)],
            shown: false,
        })
        .await
        .unwrap();
    let restarred = db
        .dispatcher
        .dispatch(SetLabels {
            changes: vec![change(m1, starred, true)],
            shown: false,
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

/// The labels the reconciler would push now, as (entry, label, value).
fn wanted(db: &Db) -> Vec<(EntryId, LabelId, bool)> {
    mailbox::reconciling(&db.reader(), "a")
        .unwrap()
        .iter()
        .filter_map(|r| r.field.wanted().map(|w| (r.key.0, r.key.1, *w)))
        .collect()
}

/// Undoing a change whose push's outcome is unknown: it may have landed,
/// so the undo is pushed, and a sync seeing it landed does not take it
/// for another client's change.
#[tokio::test]
async fn an_undo_after_a_push_of_unknown_outcome_is_pushed() {
    let db = Db::new();
    archived(&db).await;
    let (m1, inbox) = (db.entry("m1"), db.label("a", "INBOX"));
    let archive = TxLogId(db.query::<i64>("SELECT max(id) FROM tx_log")[0]);
    let change = |desired| LabelChange {
        entry: m1,
        label: inbox,
        desired,
    };
    db.push("a", vec![change(false)], Ended::Unknown { at_ms: 0 })
        .await;
    db.dispatcher.undo(archive).await.unwrap();
    // Nothing differs, yet the archive may have landed.
    assert_eq!(db.label_rows(), [row("m1", "INBOX", true, true)]);
    assert_eq!(wanted(&db), [(m1, inbox, true)]);
    // It did: still the undo is wanted.
    db.observe("a", observed(None, vec![], vec![("m1", &[])], &[]))
        .await;
    assert_eq!(wanted(&db), [(m1, inbox, true)]);
    assert_eq!(db.label_rows(), [row("m1", "INBOX", true, true)]);
}

/// A push that fails partway records how each request ended: those that
/// went through await their read-back, and an undo of one is pushed.
#[tokio::test]
async fn a_failing_push_keeps_the_outcomes_of_those_before() {
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
    let (m1, m2, inbox) = (db.entry("m1"), db.entry("m2"), db.label("a", "INBOX"));
    let change = |entry, desired| LabelChange {
        entry,
        label: inbox,
        desired,
    };
    let archive = db
        .dispatcher
        .dispatch(SetLabels {
            changes: vec![change(m1, false), change(m2, false)],
            shown: false,
        })
        .await
        .unwrap()
        .0;
    db.dispatcher
        .observe(
            "a".into(),
            Claimed::<MailLabels>::new(vec![(m1, inbox), (m2, inbox)]),
        )
        .await
        .unwrap();
    for (entry, ended) in [
        (m1, Ended::Applied),
        (m2, Ended::NotApplied { refused: None }),
    ] {
        db.dispatcher
            .observe(
                "a".into(),
                Pushed::<MailLabels> {
                    pushes: vec![Push {
                        fields: keys(&[change(entry, false)]),
                        ended,
                        rekeyed: None,
                    }],
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(wanted(&db), [(m2, inbox, false)], "m1 awaits its read-back");
    db.dispatcher.undo(TxLogId(archive)).await.unwrap();
    assert_eq!(wanted(&db), [(m1, inbox, true)], "m2 never left the Inbox");
}

/// A push that moved an entry without telling its new key leaves the key
/// unknown: nothing is pushed or read back to the key gone until a sync
/// finds the entry again (SPEC §6.6.5).
#[tokio::test]
async fn an_entry_moved_to_an_unknown_key_waits_for_a_sync() {
    let db = Db::new();
    archived(&db).await;
    let (m1, inbox, starred) = (
        db.entry("m1"),
        db.label("a", "INBOX"),
        db.label("a", "STARRED"),
    );
    let key = |external_id: Option<&str>| NewKey {
        entity: m1,
        external_id: external_id.map(str::to_owned),
    };
    let change = |desired| LabelChange {
        entry: m1,
        label: inbox,
        desired,
    };
    let reconciling = || mailbox::reconciling(&db.reader(), "a").unwrap();
    let push_moved = |desired, to: Option<&'static str>| {
        let dispatcher = db.dispatcher.clone();
        async move {
            dispatcher
                .observe("a".into(), Claimed::<MailLabels>::new(vec![(m1, inbox)]))
                .await
                .unwrap();
            dispatcher
                .observe(
                    "a".into(),
                    Pushed::<MailLabels> {
                        pushes: vec![Push {
                            fields: keys(&[change(desired)]),
                            ended: Ended::Applied,
                            rekeyed: Some(key(to)),
                        }],
                    },
                )
                .await
                .unwrap();
        }
    };

    push_moved(false, None).await;
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![LabelChange {
                entry: m1,
                label: starred,
                desired: true,
            }],
            shown: false,
        })
        .await
        .unwrap();
    assert!(reconciling().iter().all(|r| !r.target.key_known));
    // Found again by a sync: known, and the star is wanted.
    db.observe(
        "a",
        MailboxObserved {
            rekeyed: vec![mailbox::Rekeyed {
                from: "m1".into(),
                to: "m1'".into(),
            }],
            ..observed(None, vec![], vec![("m1'", &[])], &[])
        },
    )
    .await;
    assert!(reconciling().iter().all(|r| r.target.key_known));
    assert_eq!(wanted(&db), [(m1, starred, true)]);

    // A move that tells the key re-keys the entry at once.
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![change(true)],
            shown: false,
        })
        .await
        .unwrap();
    push_moved(true, Some("m1''")).await;
    assert_eq!(
        db.query::<String>("SELECT external_id FROM mail_entry"),
        ["m1''"]
    );
    assert!(reconciling().iter().all(|r| r.target.key_known));
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

/// The outbox tells of every mail account whose pushes fail, of each
/// provider: it told of Gmail's and M365's alone.
#[tokio::test]
async fn failing_pushes_are_in_the_outbox_of_every_provider() {
    let db = Db::new();
    for kind in crate::KINDS {
        db.observe(
            kind,
            MailboxObserved {
                account_kind: kind,
                ..observed(labels(ALL), vec![], vec![], &[])
            },
        )
        .await;
        db.dispatcher
            .observe(
                kind.into(),
                PushFailing {
                    error: Some("down".into()),
                    at: 10,
                },
            )
            .await
            .unwrap();
    }
    let mut conn = db.reader();
    let tx = conn.transaction().unwrap();
    let failing: Vec<String> = crate::outbox::outbox(&tx)
        .unwrap()
        .into_iter()
        .filter(|account| account.failing.is_some())
        .map(|account| account.account)
        .collect();
    let mut kinds: Vec<String> = crate::KINDS.iter().map(|k| k.to_string()).collect();
    kinds.sort();
    assert_eq!(failing, kinds);
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
        kind: LabelKind::Tag,
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
                    kind: LabelKind::Tag,
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
        mail::{ListPosition, ListScope},
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
                    identities: own("a@x"),
                    provider: uplink_protocol::mail::MailProvider::Gmail,
                },
                ConfiguredAccount {
                    id: "b".into(),
                    address: "b@x".into(),
                    identities: own("b@x"),
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
        db.push(
            "a",
            vec![LabelChange {
                entry: db.entry("m1"),
                label: inbox,
                desired: false,
            }],
            Ended::NotApplied {
                refused: Some("no".into()),
            },
        )
        .await;
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
            mail.list_entries(format!("account:a label:{label}"), after, limit)
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
        let all = || mail.list_entries("account:a".into(), None, 10);
        assert_eq!(
            ids(all().await.unwrap()),
            [vec!["w1"], vec!["x1", "x2", "x3"], vec!["y1"], vec!["z2"]]
        );
        // A negation is of an entry: x1, in t1, is read, though x3 is
        // not. Of the conversation, `has:(…)`, as home's buckets have it
        // of those before them (SPEC §12.13).
        let query = |q: &str| mail.list_entries(format!("account:a {q}"), None, 10);
        assert_eq!(
            ids(query("in:inbox -is:unread").await.unwrap()),
            [vec!["x1", "x2", "x3"], vec!["y1"], vec!["z2"]]
        );
        assert_eq!(
            ids(query("in:inbox -has:(is:unread)").await.unwrap()),
            [vec!["y1"], vec!["z2"]]
        );
        assert_eq!(
            ids(query("has:(from:bob) from:alice").await.unwrap()),
            [vec!["x1", "x2", "x3"]]
        );
        // What it holds is in its own scope: z1, in Trash, is t3's there,
        // though t3 is listed as the list's scope has it.
        assert_eq!(ids(query("has:(in:trash)").await.unwrap()), [vec!["z2"]]);
        assert_eq!(
            ids(query("has:(-is:unread) has:(is:unread)").await.unwrap()),
            [vec!["x1", "x2", "x3"]]
        );
        // Page by page, each conversation once.
        for list in ["account:a in:inbox", "account:a"] {
            let mut paged = vec![];
            let mut after = None;
            loop {
                let response = mail.list_entries(list.into(), after, 1).await.unwrap();
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
                mail.list_entries(list.into(), None, 10).await.unwrap()
            else {
                panic!()
            };
            assert_eq!(paged.len(), conversations.len(), "{list}");
        }
        // Counted, and resolved to their entries, as listed.
        for list in [
            "account:a label:INBOX",
            "account:a",
            "account:a label:TRASH",
            "x2",
        ] {
            let query = uplink_query::parse(list, 0).unwrap();
            let accounts = [ConfiguredAccount {
                id: "a".into(),
                address: "a@x".into(),
                identities: own("a@x"),
                provider: uplink_protocol::mail::MailProvider::Gmail,
            }];
            let mut conn = db.reader();
            let tx = conn.transaction().unwrap();
            let unknown = || crate::query::Unknown::False;
            let count = crate::query::count(&tx, &accounts, &query, unknown()).unwrap();
            let mut matching = crate::query::matching(&tx, &accounts, &query, unknown()).unwrap();
            let placed = crate::query::placed(&tx, &accounts, &query, unknown(), None).unwrap();
            // Of some of them, those the query matches, as it places them.
            let some: Vec<i64> = placed
                .iter()
                .step_by(2)
                .flat_map(|p| {
                    let thread = p.key.split_once('/').map(|(_, thread)| thread.to_owned());
                    let entry = p.key.strip_prefix('#').map(|id| id.parse::<i64>().unwrap());
                    tx.query_row(
                        "SELECT id FROM mail_entry WHERE thread_id = ?1 OR id = ?2 LIMIT 1",
                        uplink_storage::rusqlite::params![thread, entry],
                        |r| r.get::<_, i64>(0),
                    )
                })
                .collect();
            let among = uplink_query::Among {
                entries: some.clone(),
                ..Default::default()
            };
            let restricted =
                crate::query::placed(&tx, &accounts, &query.clone().among(among), unknown(), None)
                    .unwrap();
            assert_eq!(
                restricted,
                placed.iter().step_by(2).cloned().collect::<Vec<_>>(),
                "{list}"
            );
            // Of no entries, of none: as of what changed but mail.
            let of_none = crate::query::placed(
                &tx,
                &accounts,
                &query.clone().among(uplink_query::Among::default()),
                unknown(),
                None,
            )
            .unwrap();
            assert_eq!(of_none, vec![], "{list}");
            let places = totals(&placed);
            drop(tx);
            let ServerResponse::Conversations { conversations, .. } =
                mail.list_entries(list.into(), None, 10).await.unwrap()
            else {
                panic!()
            };
            let mut listed: Vec<Vec<EntryId>> = conversations
                .iter()
                .map(|c| {
                    c.earlier
                        .iter()
                        .map(|m| m.id)
                        .chain([c.latest.id])
                        .collect()
                })
                .collect();
            // A conversation is in each label of its entries listed.
            let mut labels = std::collections::BTreeMap::<LabelId, u64>::new();
            for c in &conversations {
                let of: std::collections::BTreeSet<LabelId> = c
                    .earlier
                    .iter()
                    .flat_map(|m| m.labels.iter().copied())
                    .chain(c.latest.labels.iter().copied())
                    .collect();
                for label in of {
                    *labels.entry(label).or_default() += 1;
                }
            }
            let accounts: Vec<(String, u64)> = if conversations.is_empty() {
                vec![]
            } else {
                vec![("a".into(), conversations.len() as u64)]
            };
            assert_eq!(places, (labels.into_iter().collect(), accounts), "{list}");
            matching.sort();
            listed.sort();
            assert_eq!(count, listed.len() as u64, "{list}");
            assert_eq!(matching, listed, "{list}");
        }
        // Of what is unseen (SPEC §12.15), those whose places were not seen
        // since their latest entry came: here the Inbox's are seen.
        {
            let query = uplink_query::parse("account:a", 0).unwrap();
            let accounts = [ConfiguredAccount {
                id: "a".into(),
                address: "a@x".into(),
                identities: own("a@x"),
                provider: uplink_protocol::mail::MailProvider::Gmail,
            }];
            let places = |unseen: Option<&crate::query::Seen>| {
                let mut conn = db.reader();
                let tx = conn.transaction().unwrap();
                let unknown = crate::query::Unknown::False;
                totals(&crate::query::placed(&tx, &accounts, &query, unknown, unseen).unwrap())
            };
            // Of each entry unseen, its conversation's key.
            let unseen = |seen: &crate::query::Seen| {
                let mut conn = db.reader();
                let tx = conn.transaction().unwrap();
                let unknown = crate::query::Unknown::False;
                let unseen =
                    crate::query::unseen(&tx, &accounts, &query, unknown, seen, 500).unwrap();
                let mut keys: Vec<String> = unseen.into_iter().map(|(key, _)| key).collect();
                keys.sort();
                keys
            };
            let none_seen = crate::query::Seen::new();
            let all = places(None);
            // Mail that reached the provider before uplink began to sync
            // the account is history, not news, however late it is stored.
            assert_eq!(places(Some(&none_seen)), (vec![], vec![]));
            assert_eq!(unseen(&none_seen), Vec::<String>::new());
            let news_since = |at: i64| {
                uplink_storage::open(db.dir.path())
                    .unwrap()
                    .execute("UPDATE account SET news_since = ?1 WHERE id = 'a'", [at])
                    .unwrap();
            };
            news_since(0);
            assert_eq!(places(Some(&none_seen)), all);
            // Each entry of each conversation listed, by its conversation.
            let mut conn = db.reader();
            let tx = conn.transaction().unwrap();
            let unknown = crate::query::Unknown::False;
            let mut every: Vec<String> = crate::query::matching(&tx, &accounts, &query, unknown)
                .unwrap()
                .into_iter()
                .flat_map(|entries| {
                    let keys = crate::query::conversation_keys(&tx, &entries[..1]).unwrap();
                    std::iter::repeat_n(keys[0].clone(), entries.len())
                })
                .collect();
            every.sort();
            drop(tx);
            assert_eq!(unseen(&none_seen), every);
            let inbox = db.label("a", "INBOX");
            let latest: i64 = db
                .reader()
                .query_row("SELECT max(id) FROM mail_entry", [], |r| r.get(0))
                .unwrap();
            let seen = crate::query::Seen::from([(format!("label:{}", inbox.0), latest)]);
            let in_inbox = all.0.iter().find(|(l, _)| *l == inbox).unwrap().1;
            let unseen_placed = places(Some(&seen));
            assert!(unseen_placed.0.iter().all(|(l, _)| *l != inbox));
            assert_eq!(unseen_placed.1, [("a".to_owned(), all.1[0].1 - in_inbox)]);
            // Of a conversation in the Inbox, no entry is unseen, though not
            // in it itself; of others, every one.
            let left = unseen(&seen);
            assert!(!left.is_empty() && left.len() < every.len());
            let mut conn = db.reader();
            let tx = conn.transaction().unwrap();
            let in_inbox: std::collections::HashSet<String> = crate::query::placed(
                &tx,
                &accounts,
                &uplink_query::parse("account:a label:INBOX", 0).unwrap(),
                crate::query::Unknown::False,
                None,
            )
            .unwrap()
            .into_iter()
            .map(|p| p.key)
            .collect();
            assert_eq!(
                left,
                every
                    .iter()
                    .filter(|key| !in_inbox.contains(*key))
                    .cloned()
                    .collect::<Vec<_>>()
            );
            // Seen in Home, none is.
            let home = crate::query::Seen::from([("home".to_owned(), latest)]);
            assert_eq!(unseen(&home), Vec::<String>::new());
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
            mail.list_entries("x2".into(), None, 10).await.unwrap()
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
            .list_entries("account:a in:inbox".into(), None, 2)
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
            .list_entries("account:a in:inbox".into(), Some(next), 10)
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
        for (query, message) in [
            ("label:nosuch", "no label nosuch"),
            ("account:c", "no account c"),
            ("a OR", "OR needs a term after it"),
        ] {
            let error = mail.list_entries(query.into(), None, 2).await.unwrap_err();
            assert_eq!(error.code, ErrorCode::Invalid);
            assert_eq!(error.message, message);
        }
        assert_eq!(
            mail.list_entries("in:inbox".into(), None, 0)
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
        let search = |query: &str, limit| mail.list_entries(query.into(), None, limit);
        assert_eq!(
            subjects(&search("number2", 10).await.unwrap()),
            ["Subject 2"]
        );
        assert_eq!(
            subjects(&search("COMM sender", 2).await.unwrap()),
            ["Subject 5", "Subject 4"]
        );
        assert_eq!(
            subjects(&search("subject 3", 10).await.unwrap()),
            ["Subject 3"]
        );
        // A phrase is exact; FTS5's own syntax is text.
        assert!(subjects(&search("\"numb\"", 10).await.unwrap()).is_empty());
        assert!(subjects(&search("\"a OR b\" NEAR", 10).await.unwrap()).is_empty());
        // No terms: every account's mail.
        assert_eq!(subjects(&search("  ", 10).await.unwrap()).len(), 5);
    }

    /// Each operator of the query language (SPEC §8.7), over the service.
    #[tokio::test]
    async fn query_operators() {
        use crate::parse::tests::WITH_PARTS;
        let db = Db::new();
        let store = db.store();
        let day = |date: &str| uplink_query::parse(&format!("after:{date}"), 0).unwrap();
        let at = |date: &str| match day(date) {
            uplink_query::Query::Term(uplink_query::Term::After(at)) => at,
            q => panic!("{q:?}"),
        };
        let entry = |id: &str, labels: &[&str], date: &str, raw: &[u8]| {
            let mut e = new_entry(&store, id, labels, raw);
            e.received_at = at(date) + 3_600_000;
            e
        };
        let message = |id: &str, from: &str, to: &str, cc: &str, subject: &str, body: &str| {
            format!(
                "Message-ID: <{id}@x>\r\nFrom: {from}\r\nTo: {to}\r\nCc: {cc}\r\n\
                 Date: {DATE}\r\nSubject: {subject}\r\n\r\n{body}\r\n"
            )
            .into_bytes()
        };
        let a = vec![
            entry(
                "o1",
                &["INBOX"],
                "2026-01-15",
                &message(
                    "o1",
                    "Alice <alice@x>",
                    "a@x",
                    "carol@y",
                    "Quarterly report",
                    "numbers",
                ),
            ),
            entry(
                "o2",
                &["SENT", "STARRED"],
                "2026-03-01",
                &message("o2", "A <a@x>", "bob@z", "dave@z", "Lunch", "tomorrow?"),
            ),
            entry(
                "o3",
                &["INBOX", "UNREAD", "Label_1"],
                "2026-03-02",
                &message(
                    "o3",
                    "Bob <bob@z>",
                    "A <A@X>",
                    "dave@z",
                    "Re: Lunch",
                    "sure",
                ),
            ),
            entry("parts", &["INBOX"], "2026-05-01", WITH_PARTS.as_bytes()),
        ];
        db.observe(
            "a",
            observed(
                labels(&[
                    "INBOX", "SENT", "UNREAD", "STARRED", "TRASH", "SPAM", "Label_1",
                ]),
                a,
                vec![],
                &[],
            ),
        )
        .await;
        let b = vec![entry(
            "b1",
            &["INBOX"],
            "2026-02-01",
            &message("b1", "alice@x", "b@x", "a@x", "Hello b", "hi"),
        )];
        db.observe("b", observed(labels(&["INBOX"]), b, vec![], &[]))
            .await;
        let mail = mail_of(&db);
        let all = [
            "Report",
            "Re: Lunch",
            "Lunch",
            "Hello b",
            "Quarterly report",
        ];
        for (query, expected) in [
            ("from:alice", &["Hello b", "Quarterly report"][..]),
            ("from:me", &["Lunch"]),
            // To or Cc, of the entry's account, whatever its case.
            ("to:me", &["Re: Lunch", "Hello b", "Quarterly report"]),
            ("to:alice OR to:b@x", &["Hello b"]),
            ("to:carol", &["Quarterly report"]),
            ("cc:carol", &["Quarterly report"]),
            ("cc:dave -from:me", &["Re: Lunch"]),
            // b1's Cc, a@x, is not account b's.
            ("cc:me", &[]),
            ("subject:lunch", &["Re: Lunch", "Lunch"]),
            ("body:lunch", &[]),
            ("lunch -re", &["Lunch"]),
            ("\"quarterly report\"", &["Quarterly report"]),
            ("\"report quarterly\"", &[]),
            ("\"quart\"", &[]),
            ("label:label_1", &["Re: Lunch"]),
            ("tag:Label_1 OR is:starred", &["Re: Lunch", "Lunch"]),
            ("in:sent", &["Lunch"]),
            ("is:unread", &["Re: Lunch"]),
            (
                "account:a is:read in:inbox",
                &["Report", "Quarterly report"],
            ),
            ("NOT in:inbox", &["Lunch"]),
            ("has:attachment", &["Report"]),
            ("-has:attachment account:b", &["Hello b"]),
            ("after:2026-03-01", &["Report", "Re: Lunch", "Lunch"]),
            ("before:2026-03-01", &["Hello b", "Quarterly report"]),
            ("after:2026/2/1 before:2026-03-02", &["Lunch", "Hello b"]),
            ("older_than:1d", &all),
            ("newer_than:1d", &[]),
            ("larger:1M", &[]),
            ("smaller:1M", &all),
            ("id:<o2@x>", &["Lunch"]),
            ("rfc822msgid:o2@x OR id:o3@x", &["Re: Lunch", "Lunch"]),
            ("account:B@X", &["Hello b"]),
            (
                "(from:alice OR from:bob) -account:b",
                &["Re: Lunch", "Quarterly report"],
            ),
        ] {
            let response = mail.list_entries(query.into(), None, 10).await.unwrap();
            assert_eq!(subjects(&response), expected, "{query}");
        }
        let error = mail
            .list_entries("category:social".into(), None, 10)
            .await
            .unwrap_err();
        assert_eq!(
            (error.code, error.message.as_str()),
            (ErrorCode::Invalid, "no label CATEGORY_SOCIAL")
        );
    }

    /// `me` is every identity of the entry's account (SPEC §3.2): its
    /// address, its others, and any address at a domain of one.
    #[tokio::test]
    async fn me_is_every_identity() {
        let db = Db::new();
        let store = db.store();
        let message = |id: &str, from: &str, to: &str, cc: &str| {
            let raw = format!(
                "Message-ID: <{id}@x>\r\nFrom: {from}\r\nTo: {to}\r\nCc: {cc}\r\n\
                 Date: {DATE}\r\nSubject: {id}\r\n\r\nhi\r\n"
            );
            new_entry(&store, id, &["INBOX"], raw.as_bytes())
        };
        let entries = vec![
            message("own", "a@x", "bob@z", "carol@z"),
            message("alias", "bob@z", "Shop <SHOP@x>", "carol@z"),
            message("domain", "bob@z", "carol@z", "anything@Example.ORG"),
            message("sub", "bob@z", "anything@sub.example.org", "carol@z"),
            message("sent", "Me <someone@example.org>", "bob@z", ""),
            message("theirs", "bob@z", "carol@z", "dave@x"),
        ];
        db.observe("a", observed(labels(&["INBOX"]), entries, vec![], &[]))
            .await;
        let accounts = [crate::query::ConfiguredAccount {
            id: "a".into(),
            address: "a@x".into(),
            identities: vec![
                uplink_protocol::mail::Identity::Address {
                    address: "a@x".into(),
                    name: None,
                },
                uplink_protocol::mail::Identity::Address {
                    address: "shop@x".into(),
                    name: None,
                },
                uplink_protocol::mail::Identity::Domain {
                    domain: "example.org".into(),
                    name: None,
                },
            ],
            provider: uplink_protocol::mail::MailProvider::Gmail,
        }];
        let mut conn = db.reader();
        for (query, expected) in [
            ("from:me", &["own", "sent"][..]),
            ("to:me", &["alias", "domain"]),
            ("cc:me", &["domain"]),
        ] {
            let query = uplink_query::parse(query, 0).unwrap();
            let tx = conn.transaction().unwrap();
            let page = crate::query::conversations(
                &tx,
                &accounts,
                &query,
                crate::query::Unknown::Refuse,
                None,
                10,
            )
            .unwrap();
            let mut subjects: Vec<_> = page
                .conversations
                .iter()
                .map(|c| c.latest.subject.clone().unwrap())
                .collect();
            subjects.sort();
            assert_eq!(subjects, expected, "{query:?}");
        }
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
        let inbox = |after, limit| mail.list_entries("in:inbox".into(), after, limit);
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
    async fn rules_show_remote_images_unasked() {
        use uplink_protocol::mail::{ImageRule, ImageRuleChanged};
        let db = Db::new();
        let mail = mail_with(&db).await;
        let message = message_of(&db, "m2");
        let rules = || async {
            let ServerResponse::Message { message } = mail.load_message(message).await.unwrap()
            else {
                panic!()
            };
            message.image_rules
        };
        let sender = |address: &str| ImageRule::Sender {
            address: address.into(),
        };
        let domain = |domain: &str| ImageRule::Domain {
            domain: domain.into(),
        };
        let host = |host: &str| ImageRule::Host { host: host.into() };
        assert_eq!(rules().await, []);
        let mut live = db.hub.subscribe();
        // Of any case.
        let ServerResponse::Dispatched { tx_log_id } = mail
            .always_show_images(sender(" S@X "), true)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(rules().await, [sender("s@x")]);
        let entry = live.recv().await.unwrap();
        assert_eq!(entry.description, "Always showing images from s@x");
        assert_eq!(
            entry.events,
            [ServerEvent::ImageRuleChanged(ImageRuleChanged {
                rule: sender("s@x"),
                always: true
            })]
        );
        // Once.
        assert_eq!(
            mail.always_show_images(sender("s@x"), true)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        db.dispatcher.undo(tx_log_id).await.unwrap();
        live.recv().await.unwrap();
        assert_eq!(rules().await, []);
        assert_eq!(
            mail.always_show_images(sender("s@x"), false)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        // Anyone's at the domain, exactly; images at a host, whoever's.
        mail.always_show_images(domain("X"), true).await.unwrap();
        mail.always_show_images(domain("sub.x"), true)
            .await
            .unwrap();
        mail.always_show_images(host("img.t.example"), true)
            .await
            .unwrap();
        let entry = live.recv().await.unwrap();
        assert_eq!(entry.description, "Always showing images from anyone at x");
        assert_eq!(rules().await, [domain("x")]);
        assert_eq!(
            crate::image_rules::applying(
                &db.reader(),
                Some("A@SUB.X"),
                ["t.example", "img.t.example"]
            )
            .unwrap(),
            [domain("sub.x"), host("img.t.example")]
        );
        for rule in [
            sender("nobody"),
            sender("a@b@c"),
            domain("a@b"),
            host(" "),
            host("a;b"),
        ] {
            assert_eq!(
                mail.always_show_images(rule.clone(), true)
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::Invalid,
                "{rule:?}"
            );
        }
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
        let list = |query: &str| mail.list_entries(query.into(), None, 10);
        let scope = |query: &'static str| async move {
            let ServerResponse::Conversations { scope, .. } = list(query).await.unwrap() else {
                panic!()
            };
            scope
        };
        assert_eq!(
            subjects(&list("account:a in:inbox").await.unwrap()),
            ["Subject 5", "Subject 4", "Subject 2"]
        );
        assert_eq!(
            subjects(&list("account:a label:Label_1").await.unwrap()),
            ["Subject 2"]
        );
        assert_eq!(
            subjects(&list("account:a in:trash").await.unwrap()),
            ["Subject 1"]
        );
        assert_eq!(
            subjects(&list("account:a in:spam").await.unwrap()),
            ["Subject 3"]
        );
        assert_eq!(
            subjects(&list("account:a").await.unwrap()),
            ["Subject 5", "Subject 4", "Subject 2"]
        );
        assert!(subjects(&list("account:b").await.unwrap()).is_empty());
        assert_eq!(
            subjects(&list("common").await.unwrap()),
            ["Subject 5", "Subject 4", "Subject 2"]
        );
        // Naming Trash or Spam lists theirs instead; in:anywhere, all.
        assert_eq!(
            subjects(&list("common in:trash").await.unwrap()),
            ["Subject 1"]
        );
        assert_eq!(
            subjects(&list("common -in:trash").await.unwrap()),
            ["Subject 5", "Subject 4", "Subject 2"]
        );
        assert_eq!(
            subjects(&list("in:trash OR in:spam").await.unwrap()),
            ["Subject 3", "Subject 1"]
        );
        assert_eq!(subjects(&list("in:anywhere").await.unwrap()).len(), 5);
        // Account b has neither.
        assert_eq!(
            scope("in:inbox").await,
            ListScope::Except {
                labels: vec![trash, spam]
            }
        );
        assert_eq!(
            scope("in:trash").await,
            ListScope::Within {
                labels: vec![trash]
            }
        );
        assert_eq!(
            scope("in:anywhere in:trash").await,
            ListScope::Except { labels: vec![] }
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

    /// Actions on conversations change each of their entries, but for
    /// marking unread and starring, which take the latest (SPEC §12.8).
    #[tokio::test]
    async fn act_on_conversations() {
        use uplink_protocol::mail::ConversationAction as A;
        let db = Db::new();
        let mail = mail_with(&db).await;
        let act = |conversations: &[&[&str]], action: A| {
            let mail = mail.clone();
            let conversations = conversations
                .iter()
                .map(|c| c.iter().map(|id| db.entry(id)).collect())
                .collect();
            async move {
                mail.act_on_conversations(conversations, action, false)
                    .await
            }
        };
        let rows = |label: &str| -> Vec<String> {
            db.label_rows()
                .into_iter()
                .filter(|r| r.1 == label && r.2)
                .map(|r| r.0)
                .collect()
        };
        // m1, m3 and m5 are unread; m2, the latest of [m2, m1] by when it
        // came, whichever order they are given in.
        act(&[&["m2", "m1"], &["m4"]], A::Read { read: false })
            .await
            .unwrap();
        assert_eq!(rows("UNREAD"), ["m1", "m2", "m3", "m4", "m5"]);
        act(&[&["m1", "m2"], &["m3"]], A::Read { read: true })
            .await
            .unwrap();
        assert_eq!(rows("UNREAD"), ["m4", "m5"]);
        // A star for the latest of each conversation without one.
        act(&[&["m1", "m2"]], A::Star { star: true }).await.unwrap();
        act(&[&["m1", "m2"], &["m3"]], A::Star { star: true })
            .await
            .unwrap();
        assert_eq!(rows("STARRED"), ["m2", "m3"]);
        act(&[&["m1", "m2", "m3"]], A::Star { star: false })
            .await
            .unwrap();
        assert!(rows("STARRED").is_empty());
        act(&[&["m1", "m2"]], A::Spam { spam: true }).await.unwrap();
        act(&[&["m1", "m2"]], A::Archive).await.unwrap();
        assert_eq!(rows("SPAM"), ["m1", "m2"]);
        assert_eq!(rows("INBOX"), ["m3", "m4", "m5"]);
        // Not spam: back in the Inbox.
        act(&[&["m1", "m2"]], A::Spam { spam: false })
            .await
            .unwrap();
        assert!(rows("SPAM").is_empty());
        assert_eq!(rows("INBOX"), ["m1", "m2", "m3", "m4", "m5"]);
        let label = db.label("a", "Label_1");
        act(&[&["m1"], &["m2"]], A::Label { label, add: true })
            .await
            .unwrap();
        assert_eq!(rows("Label_1"), ["m1", "m2"]);
        let inbox = db.label("a", "INBOX");
        act(
            &[&["m2"]],
            A::Move {
                from: label,
                to: inbox,
            },
        )
        .await
        .unwrap();
        act(
            &[&["m1"]],
            A::Move {
                from: inbox,
                to: label,
            },
        )
        .await
        .unwrap();
        assert_eq!(rows("Label_1"), ["m1"]);
        assert_eq!(rows("INBOX"), ["m2", "m3", "m4", "m5"]);
        act(&[&["m5"]], A::Trash).await.unwrap();
        assert_eq!(rows("TRASH"), ["m5"]);

        let code = |r: Result<ServerResponse, uplink_protocol::ServerError>| r.unwrap_err().code;
        let other = db.label("b", "INBOX");
        assert_eq!(code(act(&[], A::Archive).await), ErrorCode::Conflict);
        assert_eq!(code(act(&[&[]], A::Archive).await), ErrorCode::Invalid);
        assert_eq!(
            code(
                act(
                    &[&["m1"]],
                    A::Label {
                        label: other,
                        add: true
                    }
                )
                .await
            ),
            ErrorCode::Invalid
        );
        assert_eq!(
            code(
                act(
                    &[&["m1"]],
                    A::Move {
                        from: inbox,
                        to: inbox
                    }
                )
                .await
            ),
            ErrorCode::Invalid
        );
        assert_eq!(
            code(
                mail.act_on_conversations(vec![vec![EntryId(999)]], A::Archive, false)
                    .await
            ),
            ErrorCode::NotFound
        );
        // The account has no IMPORTANT to mark.
        assert_eq!(
            code(act(&[&["m1"]], A::Important { important: true }).await),
            ErrorCode::Invalid
        );
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

/// The conversations `query` lists, with accounts `a` and `b`.
fn list(
    conn: &mut Connection,
    query: &str,
    after: Option<uplink_protocol::mail::ListPosition>,
    limit: u32,
) -> crate::query::Page {
    let accounts = ["a", "b"].map(|id| crate::query::ConfiguredAccount {
        id: id.into(),
        address: format!("{id}@x"),
        identities: own(&format!("{id}@x")),
        provider: uplink_protocol::mail::MailProvider::Gmail,
    });
    let query = uplink_query::parse(query, 0).unwrap();
    let tx = conn.transaction().unwrap();
    let unknown = crate::query::Unknown::Refuse;
    crate::query::conversations(&tx, &accounts, &query, unknown, after, limit).unwrap()
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

/// Fills an empty database with account `a`'s mailbox of `messages`
/// entries, none threaded, newer by id, each of the labels `labels(id)`
/// (of INBOX, UNREAD, IMPORTANT); account `b` has none.
fn fill_labelled(conn: &mut Connection, messages: i64, labels: fn(i64) -> Vec<&'static str>) {
    let tx = conn.transaction().unwrap();
    for account in ["a", "b"] {
        tx.execute(
            "INSERT INTO account (id, kind) VALUES (?1, 'gmail')",
            [account],
        )
        .unwrap();
        for label in ["INBOX", "UNREAD", "IMPORTANT", "TRASH", "SPAM"] {
            tx.execute(
                "INSERT INTO label (account_id, external_id, name, system) VALUES (?1, ?2, ?2, 1)",
                [account, label],
            )
            .unwrap();
        }
    }
    {
        let mut statements = [
            "INSERT INTO message (id, message_id, preferred_variant, subject, from_addr,
                                  to_addrs, cc_addrs, senders, snippet, has_attachments)
             VALUES (?1, ?1 || '@x', ?1, 's', 's@x', '[]', '[]', '[]', '', 0)",
            "INSERT INTO message_variant (sha256, message, source, size_bytes)
             VALUES (?1, ?1, 'a', 1)",
            "INSERT INTO mail_entry (account_id, external_id, message, variant, received_at)
             VALUES ('a', ?1, ?1, ?1, ?1 * 1000)",
        ]
        .map(|sql| tx.prepare(sql).unwrap());
        let mut labelled = tx
            .prepare(
                "INSERT INTO entry_label (entry, label, desired, observed)
                 SELECT ?1, id, 1, 1 FROM label WHERE account_id = 'a' AND external_id = ?2",
            )
            .unwrap();
        for id in 1..=messages {
            for statement in &mut statements {
                statement.execute([id]).unwrap();
            }
            for label in labels(id) {
                labelled.execute(params![id, label]).unwrap();
            }
        }
    }
    tx.commit().unwrap();
    conn.execute_batch("ANALYZE").unwrap();
}

/// A page of terms each of many entries but of few in common (home's
/// Urgent and Important: `in:inbox is:unread is:important`) is found by
/// the fewest's candidates (SPEC §8): its work is as much however many
/// more entries the mailbox has of the others, old ones in common that
/// walking newest first would look through all of it for.
#[test]
fn a_page_of_few_in_common_costs_as_the_fewest_however_large_the_mailbox() {
    use uplink_storage::work::Work;

    // The oldest 2500 important, in the Inbox, a hundred of them unread;
    // the others in the Inbox or unread, by halves.
    fn labels(id: i64) -> Vec<&'static str> {
        match id {
            ..=2500 if id % 25 == 0 => vec!["INBOX", "IMPORTANT", "UNREAD"],
            ..=2500 => vec!["INBOX", "IMPORTANT"],
            _ if id % 2 == 0 => vec!["INBOX"],
            _ => vec!["UNREAD"],
        }
    }
    let work_of = |messages: i64| {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = uplink_storage::open(dir.path()).unwrap();
        fill_labelled(&mut conn, messages, labels);
        let work = Work::default();
        work.count_on(&conn);
        let (steps, page) =
            work.of(|| list(&mut conn, "in:inbox is:unread is:important", None, 20));
        assert_eq!(page.conversations.len(), 20);
        steps
    };
    let (small, large) = (work_of(10_000), work_of(40_000));
    assert!(
        (large as f64) < 1.5 * small as f64,
        "of 10,000 entries {small} steps, of 40,000 {large}"
    );
}

/// Lists and searches with many candidates walk entries newest first
/// rather than sorting the candidates; both give the order plain queries
/// do, page by page.
#[test]
fn large_lists_and_searches_page_in_order() {
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
    for (query, sql) in [
        // 3000 entries: walked.
        (
            "account:a label:INBOX",
            format!(
                "SELECT e.id FROM mail_entry e JOIN entry_label el ON el.entry = e.id
                 WHERE el.label = {inbox} AND el.desired AND {unhidden} {order}"
            ),
        ),
        // 600: sorted.
        (
            "account:a is:unread",
            format!(
                "SELECT e.id FROM mail_entry e JOIN entry_label el ON el.entry = e.id
                 WHERE el.label = {unread} AND el.desired AND {unhidden} {order}"
            ),
        ),
        (
            "account:b",
            format!(
                "SELECT e.id FROM mail_entry e WHERE e.account_id = 'b' AND {unhidden} {order}"
            ),
        ),
        // Terms of other kinds are false of mail, their negations true.
        (
            "(is:room OR account:b) -is:mention -due:today",
            format!(
                "SELECT e.id FROM mail_entry e WHERE e.account_id = 'b' AND {unhidden} {order}"
            ),
        ),
        (
            "account:a is:todo",
            "SELECT e.id FROM mail_entry e WHERE 0".to_owned(),
        ),
    ] {
        let mut listed = vec![];
        let mut after = None;
        loop {
            let page = list(&mut conn, query, after, 250);
            listed.extend(page.conversations.iter().map(|c| c.latest.id.0));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(listed, plain(&conn, &sql), "{query}");
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
        let found: Vec<i64> = list(&mut conn, words, None, 100)
            .conversations
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
    use std::collections::{BTreeMap, BTreeSet};

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
    let hidden = |r: &Row| r.labels.contains("TRASH") || r.labels.contains("SPAM");
    // Each list: its scope, and what puts a conversation on it.
    type Test<'a> = (
        &'static str,
        Box<dyn Fn(&Row) -> bool + 'a>,
        Box<dyn Fn(&Row) -> bool + 'a>,
    );
    let tests: Vec<Test> = vec![
        // 3000 entries: walked.
        (
            "account:a label:INBOX",
            Box::new(|r: &Row| !hidden(r)),
            Box::new(|r: &Row| r.account == "a" && r.labels.contains("INBOX")),
        ),
        // 600: sorted.
        (
            "account:a is:unread",
            Box::new(|r: &Row| !hidden(r)),
            Box::new(|r: &Row| r.account == "a" && r.labels.contains("UNREAD")),
        ),
        // 60: sorted, of Trash alone.
        (
            "account:a in:trash",
            Box::new(|r: &Row| r.labels.contains("TRASH")),
            Box::new(|r: &Row| r.account == "a" && r.labels.contains("TRASH")),
        ),
        (
            "account:b",
            Box::new(|r: &Row| !hidden(r)),
            Box::new(|r: &Row| r.account == "b"),
        ),
        // Every account's: walked.
        (
            "in:inbox",
            Box::new(|r: &Row| !hidden(r)),
            Box::new(|r: &Row| r.labels.contains("INBOX")),
        ),
    ];
    for (query, scope, matches) in tests {
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
            let page = list(&mut conn, query, after, 250);
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
        assert_eq!(listed.len(), expected.len(), "{query}");
        assert_eq!(listed, expected, "{query}");
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

    const MESSAGES: i64 = 50_000;
    let dir = tempfile::tempdir().unwrap();
    let mut conn = uplink_storage::open(dir.path()).unwrap();
    let started = Instant::now();
    fill(&mut conn, MESSAGES, threads);
    eprintln!("set up {MESSAGES} messages in {:?}", started.elapsed());

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
    for query in [
        "w0",
        "w1 w2",
        "w4321",
        "w12 w3000",
        "w",
        "w1 OR w2",
        "w1 -w2",
        "account:a is:unread w3",
        "in:inbox -is:unread",
        "after:1970-01-01 w5",
    ] {
        time(&format!("search {query:?}"), &mut |conn| {
            list(conn, query, None, 100).conversations.len()
        });
    }
    let accounts = ["a", "b"].map(|id| query::ConfiguredAccount {
        id: id.into(),
        address: format!("{id}@x"),
        identities: own(&format!("{id}@x")),
        provider: uplink_protocol::mail::MailProvider::Gmail,
    });
    for negation in ["-has:", "-"] {
        time(&format!("home's buckets, {negation}(…)"), &mut |conn| {
            home(conn, &accounts, negation).iter().sum()
        });
    }
    time("Inbox, first page", &mut |conn| {
        list(conn, "account:a in:inbox", None, 100)
            .conversations
            .len()
    });
    time("every Inbox, first page", &mut |conn| {
        list(conn, "in:inbox", None, 100).conversations.len()
    });
    time("All Mail, first page", &mut |conn| {
        list(conn, "account:a", None, 100).conversations.len()
    });
    time("accounts and counts", &mut |conn| {
        query::accounts(
            conn,
            &[query::ConfiguredAccount {
                id: "a".into(),
                address: "a@x".into(),
                identities: own("a@x"),
                provider: uplink_protocol::mail::MailProvider::Gmail,
            }],
        )
        .unwrap()
        .1
        .len()
    });
    eprintln!("slowest: {slowest:?}");
}

/// Home's default buckets (uplink-config's), each less those before it:
/// of the items, `-has:(…)`, as home has them, or of each entry, as it had
/// them. How many of the first 20 conversations each lists.
fn home(
    conn: &mut Connection,
    accounts: &[crate::query::ConfiguredAccount],
    negation: &str,
) -> Vec<usize> {
    let home = [
        "(in:inbox is:unread is:important to:me) OR (is:mention is:new) \
         OR (is:participating is:new) OR (is:dm is:unread is:notifying) OR is:invite",
        "(in:inbox is:unread is:important) OR (is:unread is:notifying) \
         OR is:overdue OR due:today",
        "(in:inbox is:unread) OR (is:unread tag:favourite)",
        "is:starred OR is:todo",
        "in:inbox",
        "is:unread -tag:lowpriority",
    ];
    let tx = conn.transaction().unwrap();
    (0..home.len())
        .map(|at| {
            let text = std::iter::once(format!("({})", home[at]))
                .chain(home[..at].iter().map(|q| format!("{negation}({q})")))
                .collect::<Vec<_>>()
                .join(" ");
            let query = uplink_query::parse(&text, 0).unwrap();
            let unknown = crate::query::Unknown::False;
            crate::query::conversations(&tx, accounts, &query, unknown, None, 20)
                .unwrap()
                .conversations
                .len()
        })
        .collect()
}

/// How fast home's buckets are over a copy of a database, `UPLINK_DB`
/// (`uplink.sqlite`): on demand, as `speed_of_search_and_listing`.
#[test]
#[ignore]
fn speed_of_home_over_a_copy() {
    use std::time::Instant;

    let path = std::env::var("UPLINK_DB").expect("UPLINK_DB names a copy of a database");
    let mut conn = Connection::open(path).unwrap();
    let accounts: Vec<_> = conn
        .prepare("SELECT id FROM account WHERE kind <> 'matrix'")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|id| {
            let id = id.unwrap();
            crate::query::ConfiguredAccount {
                address: format!("{id}@x"),
                identities: own(&format!("{id}@x")),
                id,
                provider: uplink_protocol::mail::MailProvider::Gmail,
            }
        })
        .collect();
    for negation in ["-has:", "-", "-has:", "-"] {
        let started = Instant::now();
        let listed = home(&mut conn, &accounts, negation);
        eprintln!("{negation}(…): {listed:?} in {:?}", started.elapsed());
    }
}

/// Messages stored before `has_attachments` was recorded get it from their
/// .eml once, before the dispatcher takes the writer.
#[tokio::test]
async fn attachments_are_indexed_for_messages_stored_before() {
    use crate::{mailbox::index_attachments, parse::tests::WITH_PARTS};

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
        let mut entries: Vec<_> = list(conn, "account:a in:inbox", None, 10)
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

/// Reads back `entries` as uplink has them, as a provider whose labels
/// are uplink's would.
async fn read_as_desired(
    account: &crate::account::Account,
    entries: Vec<(EntryId, String)>,
) -> anyhow::Result<()> {
    let as_of = account.as_of().await?;
    let mut updated = vec![];
    for (entry, external_id) in entries {
        updated.push(EntryLabels {
            external_id,
            labels: account.desired_labels(entry).await?,
        });
    }
    account
        .observe(MailboxObserved {
            updated,
            as_of,
            ..observed(None, vec![], vec![], &[])
        })
        .await
}

/// An account with a message, "m1", which the user has archived.
async fn archived(db: &Db) {
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
    db.dispatcher
        .dispatch(SetLabels {
            changes: vec![LabelChange {
                entry: db.entry("m1"),
                label: db.label("a", "INBOX"),
                desired: false,
            }],
            shown: false,
        })
        .await
        .unwrap();
}

/// A provider whose sync takes `steps` steps, recording its calls.
struct Stepping {
    steps: std::sync::atomic::AtomicU32,
    calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl crate::account::Provider for Stepping {
    const KIND: &'static str = "gmail";
    const FILES_SENT: bool = true;

    async fn sync(&self, _: &crate::account::Account) -> anyhow::Result<bool> {
        self.calls.lock().unwrap().push("sync");
        Ok(self.steps.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) > 1)
    }

    async fn push(
        &self,
        _: &crate::account::Account,
        claimed: Vec<crate::account::Pending>,
    ) -> Vec<crate::account::PushEnded> {
        self.calls.lock().unwrap().push("push");
        vec![crate::account::PushEnded::of(
            &claimed,
            Ended::Applied,
            None,
        )]
    }

    async fn read(
        &self,
        account: &crate::account::Account,
        entries: Vec<(EntryId, String)>,
    ) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push("read");
        read_as_desired(account, entries).await
    }

    async fn send(
        &self,
        _: &crate::account::Account,
        _: crate::account::Outgoing<'_>,
    ) -> Result<crate::compose::SentCopy, crate::account::SendError> {
        unreachable!("nothing is queued")
    }
}

/// A long sync (a first listing, of days) does not hold up pushes: they
/// go between its steps, which follow each other without waiting for the
/// poll interval.
#[tokio::test]
async fn pushes_go_between_the_steps_of_a_long_sync() {
    let db = Db::new();
    archived(&db).await;
    let calls = Arc::new(std::sync::Mutex::new(vec![]));
    let task = crate::account::SyncTask::new(
        Stepping {
            steps: 3.into(),
            calls: calls.clone(),
        },
        "a".into(),
        own("a@x"),
        db.dispatcher.clone(),
        db.reader(),
        db.dir.path(),
        std::time::Duration::from_secs(3600),
    );
    let run = tokio::spawn(task.run());
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while calls.lock().unwrap().len() < 5 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:?}",
            calls.lock().unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // Done, it waits for the poll interval. What was pushed is read back
    // after the next sync, which tells nothing of it.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        *calls.lock().unwrap(),
        ["sync", "push", "sync", "read", "sync"]
    );
    assert!(db.accounts_outbox().is_empty(), "pushed, and seen so");
    run.abort();
}

/// A provider whose sync tells of the entries pushed, as Gmail's history
/// and IMAP's CHANGEDSINCE do, recording its calls.
struct Telling {
    calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl crate::account::Provider for Telling {
    const KIND: &'static str = "gmail";
    const FILES_SENT: bool = true;

    async fn sync(&self, account: &crate::account::Account) -> anyhow::Result<bool> {
        self.calls.lock().unwrap().push("sync");
        let pushed = account
            .read(|conn| crate::mailbox::reconciling(conn, "a"))
            .await?
            .into_iter()
            .filter(|f| f.field.attempt.is_some())
            .map(|f| (f.key.0, f.target.external_id))
            .collect();
        read_as_desired(account, pushed).await?;
        Ok(false)
    }

    async fn push(
        &self,
        _: &crate::account::Account,
        claimed: Vec<crate::account::Pending>,
    ) -> Vec<crate::account::PushEnded> {
        self.calls.lock().unwrap().push("push");
        vec![crate::account::PushEnded::of(
            &claimed,
            Ended::Applied,
            None,
        )]
    }

    async fn read(
        &self,
        _: &crate::account::Account,
        _: Vec<(EntryId, String)>,
    ) -> anyhow::Result<()> {
        unreachable!("the sync after the push tells of it")
    }

    async fn send(
        &self,
        _: &crate::account::Account,
        _: crate::account::Outgoing<'_>,
    ) -> Result<crate::compose::SentCopy, crate::account::SendError> {
        unreachable!("nothing is queued")
    }
}

/// A push applied is confirmed by the sync sent at once after it, where
/// that tells of the entry, without reading it back.
#[tokio::test]
async fn a_push_is_confirmed_by_the_next_sync() {
    let db = Db::new();
    archived(&db).await;
    let calls = Arc::new(std::sync::Mutex::new(vec![]));
    let task = crate::account::SyncTask::new(
        Telling {
            calls: calls.clone(),
        },
        "a".into(),
        own("a@x"),
        db.dispatcher.clone(),
        db.reader(),
        db.dir.path(),
        std::time::Duration::from_secs(3600),
    );
    let run = tokio::spawn(task.run());
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !db.accounts_outbox().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:?}",
            calls.lock().unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // Done, it waits for the poll interval.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(*calls.lock().unwrap(), ["sync", "push", "sync"]);
    run.abort();
}

/// A provider whose pushes fail `failures` times, then work.
struct Failing {
    failures: std::sync::atomic::AtomicU32,
}

impl crate::account::Provider for Failing {
    const KIND: &'static str = "gmail";
    const FILES_SENT: bool = true;

    async fn sync(&self, _: &crate::account::Account) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn push(
        &self,
        _: &crate::account::Account,
        claimed: Vec<crate::account::Pending>,
    ) -> Vec<crate::account::PushEnded> {
        let left = self.failures.load(std::sync::atomic::Ordering::SeqCst);
        if left > 0 {
            self.failures
                .store(left - 1, std::sync::atomic::Ordering::SeqCst);
            return vec![crate::account::PushEnded::not_sent(
                &claimed,
                anyhow::anyhow!("down {left}"),
            )];
        }
        vec![crate::account::PushEnded::of(
            &claimed,
            Ended::Applied,
            None,
        )]
    }

    async fn read(
        &self,
        account: &crate::account::Account,
        entries: Vec<(EntryId, String)>,
    ) -> anyhow::Result<()> {
        read_as_desired(account, entries).await
    }

    async fn send(
        &self,
        _: &crate::account::Account,
        _: crate::account::Outgoing<'_>,
    ) -> Result<crate::compose::SentCopy, crate::account::SendError> {
        unreachable!("nothing is queued")
    }
}

/// The task records when an account starts failing and when it recovers,
/// not each retry.
#[tokio::test(start_paused = true)]
async fn the_task_records_failing_as_it_starts_and_stops() {
    let db = Db::new();
    archived(&db).await;
    let task = crate::account::SyncTask::new(
        Failing { failures: 3.into() },
        "a".into(),
        own("a@x"),
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
            "Changes are not reaching the provider: pushing label changes: down 3",
            "Changes are reaching the provider again"
        ]
    );
    assert!(db.accounts_outbox().is_empty(), "pushed, and seen so");
}
