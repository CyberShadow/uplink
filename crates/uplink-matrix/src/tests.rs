//! The log entries of rooms and sends, without a homeserver; the
//! sync task is tested against one in `tests/conduit.rs`.

use crate::{
    rooms::{ObservedEvent, ObservedRoom, ReceiptSent, Synced},
    send::{
        SendFailed, SendStarted, SendSucceeded, TakeBackFailed, TakeBackStarted, TakeBackSucceeded,
    },
    service::{ConfiguredAccount, Matrix},
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use uplink_protocol::{
    compose::{AccountOutbox, Pending},
    matrix::{
        Encryption, MatrixContent, MatrixSend, MatrixSendId, MatrixSendState, Membership, Room,
        RoomEvent, RoomEventId, RoomId, TextFormat,
    },
    ServerEvent, ServerResponse, TxLogId,
};
use uplink_realtime::Hub;
use uplink_tx_log::{Dispatcher, Observation, Registry};

/// What a sync requested after every entry tells of: current.
const FRESH: uplink_protocol::TxLogId = uplink_protocol::TxLogId(i64::MAX);

const ME: &str = "@me:example.org";
const ROOM: &str = "!room:example.org";

struct Db {
    _dir: tempfile::TempDir,
    dispatcher: Dispatcher,
    rooms: Arc<crate::room_view::RoomsView>,
    matrix: Matrix,
}

impl Db {
    /// What is sent after the Unix epoch is news.
    fn new() -> Self {
        Self::news_since(0)
    }

    /// What is sent from `news_since` (ms since the Unix epoch) is news.
    fn news_since(news_since: i64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        crate::register(&mut registry);
        let db = uplink_storage::open(dir.path()).unwrap();
        db.execute(
            "INSERT INTO account (id, kind, news_since) VALUES ('mx', 'matrix', ?1)",
            [news_since],
        )
        .unwrap();
        let dispatcher = Dispatcher::new(db, registry, Arc::new(Hub::new(64)));
        let rooms = crate::room_view::RoomsView::kept(&dispatcher, vec![account()]).unwrap();
        let matrix = Matrix::new(
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
            rooms.clone(),
            uplink_storage::store::Store::blobs(dir.path()),
        );
        Self {
            _dir: dir,
            dispatcher,
            rooms,
            matrix,
        }
    }

    async fn observe(&self, observation: impl Observation) -> anyhow::Result<TxLogId> {
        self.dispatcher.observe("mx".into(), observation).await
    }

    /// Syncs `events` (by event id and sender) into the room, and the
    /// account's own receipts.
    async fn sync(&self, events: &[(&str, &str)], own_receipts: &[&str]) {
        self.sync_events(
            events
                .iter()
                .map(|(id, sender)| message(id, sender))
                .collect(),
            own_receipts,
        )
        .await;
    }

    /// Syncs `events` as JSON, each with a timestamp by its place.
    async fn sync_json(&self, events: &[serde_json::Value]) -> TxLogId {
        let events = events
            .iter()
            .enumerate()
            .map(|(i, event)| {
                let mut event = event.clone();
                event["origin_server_ts"] = json!(1_700_000_000_000_i64 + i as i64);
                ObservedEvent::parse(&event.to_string(), None, Encryption::None).unwrap()
            })
            .collect();
        self.sync_events(events, &[]).await
    }

    async fn sync_events(&self, events: Vec<ObservedEvent>, own_receipts: &[&str]) -> TxLogId {
        self.sync_room(events, own_receipts, &[], None).await
    }

    /// Syncs `events`, the room's `pinned` events those, and its
    /// `m.fully_read` at `fully_read`.
    async fn sync_room(
        &self,
        events: Vec<ObservedEvent>,
        own_receipts: &[&str],
        pinned: &[&str],
        fully_read: Option<&str>,
    ) -> TxLogId {
        self.observe(Synced {
            requested: FRESH,
            next_batch: "next".into(),
            packs: None,
            rooms: vec![ObservedRoom {
                own_receipts: own_receipts.iter().map(|r| r.to_string()).collect(),
                fully_read: fully_read.map(str::to_owned),
                pinned: pinned.iter().map(|p| p.to_string()).collect(),
                ..joined(ROOM, events)
            }],
        })
        .await
        .unwrap()
    }

    async fn timeline(&self) -> Vec<RoomEvent> {
        let room = self.room().await;
        match self
            .matrix
            .list_timeline(room.id, uplink_protocol::matrix::TimelinePage::Latest, 100)
            .await
            .unwrap()
        {
            ServerResponse::Timeline { events, .. } => events,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The events of the log entry `entry`.
    fn events_of(&self, entry: TxLogId) -> Vec<ServerEvent> {
        let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
        let events: String = conn
            .query_row(
                "SELECT events_json FROM tx_log WHERE id = ?1",
                [entry.0],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&events).unwrap()
    }

    async fn room(&self) -> Room {
        match self.matrix.list_rooms().await.unwrap() {
            ServerResponse::Rooms { mut rooms, .. } => rooms.remove(0),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The room's sends not yet reconciled.
    async fn sends(&self) -> Vec<MatrixSend> {
        let room = self.room().await;
        match self
            .matrix
            .list_timeline(room.id, uplink_protocol::matrix::TimelinePage::Latest, 1)
            .await
            .unwrap()
        {
            ServerResponse::Timeline { sends, .. } => sends.unwrap(),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// What the sends show: their state and whether they are wanted.
    async fn shown(&self) -> Vec<(MatrixSendState, bool)> {
        self.sends()
            .await
            .iter()
            .map(|s| (s.state, s.wanted))
            .collect()
    }

    /// What the sync task would take back, and how.
    fn take_backs(&self) -> Vec<(MatrixSendId, MatrixContent)> {
        let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
        crate::send::take_backs_due(&conn, "mx")
            .unwrap()
            .into_iter()
            .map(|d| (d.item, d.content))
            .collect()
    }

    /// Sends `content`, and has the homeserver accept it as `event_id`
    /// and sync it back.
    async fn sent(&self, content: MatrixContent, event_id: Option<&str>) -> TxLogId {
        let room = self.room().await;
        let ServerResponse::Dispatched { tx_log_id } = self
            .matrix
            .send_message(room.id, key(), content)
            .await
            .unwrap()
        else {
            panic!()
        };
        let item = start(self).await;
        self.observe(SendSucceeded {
            item,
            event_id: event_id.map(Into::into),
        })
        .await
        .unwrap();
        if let Some(id) = event_id {
            self.sync(&[(id, ME)], &[]).await;
        }
        assert!(self.sends().await.is_empty());
        tx_log_id
    }

    fn description(&self, id: TxLogId) -> String {
        let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
        conn.query_row(
            "SELECT description FROM tx_log WHERE id = ?1",
            [id.0],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// The outbox, as the server lists it of Matrix accounts.
    fn outbox(&self) -> Vec<AccountOutbox> {
        let mut conn = uplink_storage::open_reader(self._dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        crate::outbox::outbox(&tx).unwrap()
    }

    async fn event(&self, event_id: &str) -> RoomEventId {
        let room = self.room().await;
        match self
            .matrix
            .list_timeline(room.id, uplink_protocol::matrix::TimelinePage::Latest, 100)
            .await
            .unwrap()
        {
            ServerResponse::Timeline { events, .. } => {
                events.iter().find(|e| e.event_id == event_id).unwrap().id
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn send(&self, body: &str) -> TxLogId {
        let room = self.room().await;
        match self
            .matrix
            .send_message(room.id, key(), text(body))
            .await
            .unwrap()
        {
            ServerResponse::Dispatched { tx_log_id } => tx_log_id,
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// The account's configuration: of rooms of at most two members, the whole
/// history is fetched.
/// The room `room_id`, joined, named "Room", as a sync tells of it
/// with `events`.
fn joined(room_id: &str, events: Vec<ObservedEvent>) -> ObservedRoom {
    ObservedRoom {
        room_id: room_id.into(),
        name: "Room".into(),
        encrypted: false,
        membership: Membership::Joined,
        events,
        limited: false,
        own_receipts: vec![],
        fully_read: None,
        image_packs: vec![],
        space: false,
        direct: false,
        avatar: None,
        bridge: None,
        children: vec![],
        space_order: None,
        tags: vec![],
        topic: None,
        alias: None,
        pinned: vec![],
        may_pin: true,
        joined_members: 2,
        created: None,
        version: "11".into(),
    }
}

fn account() -> ConfiguredAccount {
    ConfiguredAccount {
        id: "mx".into(),
        mxid: ME.into(),
        history_members: 2,
    }
}

fn message(event_id: &str, sender: &str) -> ObservedEvent {
    let raw = serde_json::json!({
        "type": "m.room.message",
        "event_id": event_id,
        "sender": sender,
        "origin_server_ts": 1_700_000_000_000_i64,
        "content": { "msgtype": "m.text", "body": format!("message {event_id}") },
    });
    ObservedEvent::parse(&raw.to_string(), None, Encryption::None).unwrap()
}

#[test]
fn events_of_any_type_are_kept_as_they_came() {
    let raw = r#"{"type":"org.example.custom","event_id":"$e","sender":"@b:x",
                  "origin_server_ts":5,"content":{"body":"not a message"}}"#;
    let event = ObservedEvent::parse(raw, None, Encryption::Decrypted).unwrap();
    assert_eq!(event.kind, "org.example.custom");
    assert_eq!(event.body, None, "only messages have a body");
    assert_eq!(event.raw, raw);
    assert!(ObservedEvent::parse(r#"{"content":{}}"#, None, Encryption::None).is_err());
}

#[tokio::test]
async fn events_are_stored_once_and_counted_unread() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x"), ("$2", ME)], &[]).await;
    db.sync(&[("$2", ME), ("$3", "@bob:x")], &[]).await;
    let room = db.room().await;
    assert_eq!(
        room.unread, 1,
        "own messages are not unread, nor what is before them, which they answer"
    );
    let ServerResponse::Timeline { events, .. } = db
        .matrix
        .list_timeline(room.id, uplink_protocol::matrix::TimelinePage::Latest, 10)
        .await
        .unwrap()
    else {
        panic!()
    };
    let ids: Vec<_> = events.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(ids, ["$1", "$2", "$3"]);
}

#[tokio::test]
async fn unread_messages_that_notify_or_highlight_are_counted() {
    // Synced since `message`s are sent.
    let sent = 1_700_000_000_000;
    let db = Db::news_since(sent);
    let pushed = |id: &str, sender: &str, notify: bool, highlight: bool| ObservedEvent {
        push: crate::rooms::Push { notify, highlight },
        ..message(id, sender)
    };
    db.sync_events(
        vec![
            // Never one's own.
            pushed("$4", ME, true, true),
            message("$1", "@bob:x"),
            pushed("$2", "@bob:x", true, false),
            pushed("$3", "@bob:x", true, true),
            // Nor history, sent before uplink began to sync the account,
            // which the push rules evaluate as any, and is no more unread.
            ObservedEvent {
                ts: sent - 1,
                ..pushed("$5", "@bob:x", true, true)
            },
        ],
        &[],
    )
    .await;
    let room = db.room().await;
    assert_eq!((room.unread, room.notifying, room.highlighted), (3, 2, 1));
    let events = db.timeline().await;
    db.matrix
        .mark_read(room.id, events[3].id, false, false)
        .await
        .unwrap();
    let room = db.room().await;
    assert_eq!((room.notifying, room.highlighted), (0, 0));
}

#[tokio::test]
async fn pinned_events_are_listed_in_the_order_pinned() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x"), ("$2", "@bob:x")], &[]).await;
    let room = db.room().await;
    assert!(room.pinned.is_empty());
    // One not stored is pinned too, but not listed.
    let entry = db
        .sync_room(vec![], &[], &["$2", "$gone", "$1"], None)
        .await;
    assert!(db
        .events_of(entry)
        .iter()
        .any(|e| matches!(e, ServerEvent::RoomsChanged(_))));
    assert_eq!(db.room().await.pinned, ["$2", "$gone", "$1"]);
    assert!(db.room().await.may_pin);
    let ServerResponse::Pinned { events, .. } = db.matrix.list_pinned(room.id).await.unwrap()
    else {
        panic!()
    };
    let ids: Vec<_> = events.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(ids, ["$2", "$1"]);

    // Pinning one not stored, or not an event id, is refused.
    let pin = |event: &str| uplink_protocol::matrix::MatrixContent::Pin {
        event: event.into(),
        pinned: true,
    };
    assert!(db
        .matrix
        .send_message(room.id, key(), pin("$gone"))
        .await
        .is_err());
    assert!(db
        .matrix
        .send_message(room.id, key(), pin("nope"))
        .await
        .is_err());
    db.matrix
        .send_message(room.id, key(), pin("$1"))
        .await
        .unwrap();
}

#[test]
fn pins_keep_others_pinned_meanwhile() {
    use crate::sync::pins;
    let now = ["$a".to_owned(), "$b".to_owned()];
    assert_eq!(pins(&now, "$c", true).unwrap(), ["$a", "$b", "$c"]);
    assert_eq!(pins(&now, "$a", false).unwrap(), ["$b"]);
    assert_eq!(pins(&now, "$a", true), None, "pinned already");
    assert_eq!(pins(&now, "$c", false), None, "not pinned");
}

/// What came of a room is its newest message stored since, and sent
/// since, that the query matches as a message, and is not the account's.
#[tokio::test]
async fn what_came_matches_and_is_recent() {
    let db = Db::new();
    let pushed = |id: &str, sender: &str, notify: bool, ts: i64| ObservedEvent {
        push: crate::rooms::Push {
            notify,
            highlight: false,
        },
        ts,
        ..message(id, sender)
    };
    db.sync_events(vec![pushed("$1", "@bob:x", true, 3000)], &[])
        .await;
    let since = db.event("$1").await;
    db.sync_events(
        vec![
            pushed("$4", ME, true, 3000),
            pushed("$2", "@bob:x", true, 3000),
            pushed("$3", "@bob:x", false, 3000),
            // Sent long before: stored only now, as history is.
            pushed("$5", "@bob:x", true, 1000),
        ],
        &[],
    )
    .await;
    let room = db.room().await.id;
    let arrived = |query: &str| {
        let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        let rooms = crate::query::Rooms::of(&db.rooms, &tx).unwrap();
        let query = uplink_query::parse(query, 0).unwrap();
        let arrived = rooms.arrived(&tx, &query, &[room], since, 2000).unwrap();
        arrived
            .into_iter()
            .map(|(_, event)| event)
            .collect::<Vec<_>>()
    };
    let event = |id: &'static str| db.event(id);
    // Of the messages that notify, Bob's; of the unread, his last.
    assert_eq!(arrived("is:unread is:notifying"), [event("$2").await]);
    assert_eq!(arrived("is:unread"), [event("$3").await]);
    // Read: no longer unread, but still of a room its terms alone match.
    let read = event("$3").await;
    db.matrix.mark_read(room, read, false, false).await.unwrap();
    assert!(arrived("is:unread").is_empty());
    assert_eq!(arrived("is:room"), [event("$3").await]);
}

#[tokio::test]
async fn sending_is_undone_while_queued() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    let sent = db.send("hi").await;
    assert_eq!(db.shown().await, [(MatrixSendState::Queued, true)]);
    let [account] = &db.outbox()[..] else {
        panic!()
    };
    assert_eq!(account.account, "mx");
    assert!(matches!(&account.pending, Pending::Matrix { sends, .. } if sends.len() == 1));
    let cancel = db.dispatcher.undo(sent).await.unwrap();
    assert!(db.sends().await.is_empty());
    assert!(db.outbox().is_empty());
    // Redo sends it afresh.
    db.dispatcher.undo(cancel).await.unwrap();
    assert_eq!(db.shown().await, [(MatrixSendState::Queued, true)]);
}

async fn start(db: &Db) -> MatrixSendId {
    let item = db.sends().await.last().unwrap().id;
    db.observe(SendStarted { item }).await.unwrap();
    item
}

/// Once claimed, a send cancelled is taken back if it did not go out; a
/// refused one waits for the user, who takes it back at once.
#[tokio::test]
async fn sends_handed_over_are_settled_by_their_outcome() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    let sent = db.send("hi").await;
    let item = start(&db).await;
    // Claimed once only.
    assert!(db.observe(SendStarted { item }).await.is_err());
    // Cancelled while being sent: no longer wanted, until the outcome.
    db.dispatcher.undo(sent).await.unwrap();
    assert_eq!(db.shown().await, [(MatrixSendState::Sending, false)]);
    // Not sent after all (a passing failure): taken back.
    db.observe(SendFailed {
        item,
        refused: None,
    })
    .await
    .unwrap();
    assert!(db.sends().await.is_empty());

    // Wanted, a passing failure queues it again; a refusal waits.
    let sent = db.send("again").await;
    let item = start(&db).await;
    db.observe(SendFailed {
        item,
        refused: None,
    })
    .await
    .unwrap();
    assert_eq!(db.shown().await, [(MatrixSendState::Queued, true)]);
    db.observe(SendStarted { item }).await.unwrap();
    db.observe(SendFailed {
        item,
        refused: Some("M_FORBIDDEN".into()),
    })
    .await
    .unwrap();
    assert_eq!(db.sends().await[0].error.as_deref(), Some("M_FORBIDDEN"));
    assert_eq!(db.shown().await, [(MatrixSendState::Refused, true)]);
    // A refused message is cancelled at once.
    db.dispatcher.undo(sent).await.unwrap();
    assert!(db.sends().await.is_empty());
}

#[tokio::test]
async fn sent_messages_are_reconciled_once_synced() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    // Synced after the homeserver answered.
    let sent = db.send("first").await;
    let first = start(&db).await;
    db.observe(SendSucceeded {
        item: first,
        event_id: Some("$first".into()),
    })
    .await
    .unwrap();
    assert_eq!(db.shown().await, [(MatrixSendState::Sent, true)]);
    db.sync(&[("$first", ME)], &[]).await;
    assert!(db.sends().await.is_empty());

    // Undone before it is taken back, then redone: nothing to do.
    let cancel = db.dispatcher.undo(sent).await.unwrap();
    assert_eq!(db.shown().await, [(MatrixSendState::Sent, false)]);
    assert_eq!(db.outbox().len(), 1);
    db.dispatcher.undo(cancel).await.unwrap();
    assert!(db.sends().await.is_empty());
    assert!(db.take_backs().is_empty());

    // Synced before the homeserver answered.
    db.send("second").await;
    let second = start(&db).await;
    db.sync(&[("$second", ME)], &[]).await;
    db.observe(SendSucceeded {
        item: second,
        event_id: Some("$second".into()),
    })
    .await
    .unwrap();
    assert!(db.sends().await.is_empty());
    assert!(db.outbox().is_empty());
}

#[tokio::test]
async fn reading_moves_the_marker_and_undo_moves_it_back() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x"), ("$2", "@bob:x")], &[]).await;
    let room = db.room().await;
    let second = db.event("$2").await;
    let read = match db
        .matrix
        .mark_read(room.id, second, false, false)
        .await
        .unwrap()
    {
        ServerResponse::Dispatched { tx_log_id } => tx_log_id,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(db.room().await.unread, 0);
    // Its receipt is pending until sent.
    let receipts = |db: &Db| match db.outbox().into_iter().next().map(|a| a.pending) {
        Some(Pending::Matrix { receipts, .. }) => receipts,
        None => vec![],
        other => panic!("{other:?}"),
    };
    assert_eq!(receipts(&db), [room.id]);
    // The receipt is sent; its echo does not move the marker again.
    db.observe(ReceiptSent {
        room: room.id,
        up_to: second,
        fully_read: false,
    })
    .await
    .unwrap();
    assert!(receipts(&db).is_empty());
    db.dispatcher.undo(read).await.unwrap();
    // Receipts only move forward: moving the marker back sends none.
    assert!(receipts(&db).is_empty());
    db.sync(&[], &["$2"]).await;
    assert_eq!(db.room().await.unread, 2);

    // A receipt from another device, for a later event, does.
    db.sync(&[("$3", "@bob:x")], &[]).await;
    db.sync(&[], &["$3"]).await;
    assert_eq!(db.room().await.unread, 0);
}

#[tokio::test]
async fn catching_up_moves_the_fully_read_marker_and_mentions_wait_for_it() {
    let db = Db::new();
    let mut mention = message("$2", "@bob:x");
    mention.push.highlight = true;
    db.sync_events(
        vec![message("$1", "@bob:x"), mention, message("$3", "@bob:x")],
        &[],
    )
    .await;
    let room = db.room().await;
    let [first, third] = [db.event("$1").await, db.event("$3").await];
    assert_eq!((room.mentions, room.fully_read), (1, None));
    // Read as it shows, not caught up on: the mention stays.
    db.matrix
        .mark_read(room.id, third, true, false)
        .await
        .unwrap();
    let room = db.room().await;
    assert_eq!((room.unread, room.mentions), (0, 1));
    let caught_up = match db.matrix.mark_fully_read(room.id, first).await.unwrap() {
        ServerResponse::Dispatched { tx_log_id } => tx_log_id,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(db.room().await.fully_read.map(|m| m.event), Some(first));
    assert_eq!(db.room().await.mentions, 1);
    // Its marker is set on the homeserver, as receipts are sent.
    let unsent = |db: &Db| {
        crate::rooms::unsent_receipts(&uplink_storage::open_reader(db._dir.path()).unwrap(), "mx")
            .unwrap()
            .into_iter()
            .map(|u| (u.up_to, u.fully_read))
            .collect::<Vec<_>>()
    };
    assert_eq!(unsent(&db), [(third, false), (first, true)]);
    db.observe(ReceiptSent {
        room: room.id,
        up_to: first,
        fully_read: true,
    })
    .await
    .unwrap();
    assert_eq!(unsent(&db), [(third, false)]);
    db.dispatcher.undo(caught_up).await.unwrap();
    assert_eq!(db.room().await.fully_read, None);
    // Done with it, by the user's hand: caught up as well.
    db.matrix
        .mark_read(room.id, third, false, true)
        .await
        .unwrap();
    assert_eq!(db.room().await.mentions, 0);

    // Another device's `m.fully_read` moves it, past what was set.
    let db = Db::new();
    db.sync_events(vec![message("$1", "@bob:x"), message("$2", "@bob:x")], &[])
        .await;
    db.sync_room(vec![], &[], &[], Some("$2")).await;
    assert_eq!(
        db.room().await.fully_read.map(|m| m.event),
        Some(db.event("$2").await)
    );
}

/// A key no other send of the tests has.
fn key() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("key-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

#[tokio::test]
async fn a_send_asked_for_again_is_answered_by_the_entry_that_queued_it() {
    let db = Db::new();
    db.sync(&[], &[]).await;
    let room = db.room().await;
    let send = || db.matrix.send_message(room.id, "resent".into(), text("hi"));
    let first = send().await.unwrap();
    assert!(matches!(first, ServerResponse::Dispatched { .. }));
    assert_eq!(send().await.unwrap(), first);
    assert_eq!(db.sends().await.len(), 1);
}

#[tokio::test]
async fn messages_go_only_to_known_rooms() {
    let db = Db::new();
    let refused = db
        .matrix
        .send_message(RoomId(1), key(), text("hi"))
        .await
        .unwrap_err();
    assert_eq!(refused.code, uplink_protocol::ErrorCode::NotFound);
    db.sync(&[], &[]).await;
    let room = db.room().await;
    let refused = db
        .matrix
        .send_message(room.id, key(), text(" "))
        .await
        .unwrap_err();
    assert_eq!(refused.code, uplink_protocol::ErrorCode::Invalid);
}

mod membership {
    use super::*;
    use crate::membership::MembershipChanged;
    use uplink_protocol::{matrix::MembershipChange, ErrorCode};

    impl Db {
        /// Syncs the room with `membership`, and no events.
        async fn sync_membership(&self, membership: Membership) {
            self.observe(Synced {
                requested: FRESH,
                next_batch: "next".into(),
                packs: None,
                rooms: vec![ObservedRoom {
                    room_id: ROOM.into(),
                    name: "Room".into(),
                    encrypted: false,
                    membership,
                    events: vec![],
                    limited: false,
                    own_receipts: vec![],
                    fully_read: None,
                    image_packs: vec![],
                    space: false,
                    direct: false,
                    avatar: None,
                    bridge: None,
                    children: vec![],
                    space_order: None,
                    tags: vec![],
                    topic: None,
                    alias: None,
                    pinned: vec![],
                    may_pin: false,
                    joined_members: 2,
                    created: None,
                    version: "11".into(),
                }],
            })
            .await
            .unwrap();
        }

        async fn change(&self, change: MembershipChange) -> TxLogId {
            let room = self.room().await.id;
            match self.matrix.change_membership(room, change).await.unwrap() {
                ServerResponse::Dispatched { tx_log_id } => tx_log_id,
                other => panic!("unexpected {other:?}"),
            }
        }

        /// The homeserver did as asked, or refused.
        async fn reconcile(&self, change: MembershipChange, refused: Option<&str>) {
            let room = self.room().await.id;
            self.observe(MembershipChanged {
                room,
                change,
                refused: refused.map(Into::into),
            })
            .await
            .unwrap();
        }

        async fn state(&self) -> (Membership, Option<MembershipChange>) {
            let room = self.room().await;
            (room.membership, room.pending)
        }
    }

    #[tokio::test]
    async fn joining_an_invitation_is_undone_by_cancelling_or_leaving() {
        let db = Db::new();
        db.sync_membership(Membership::Invited).await;
        let join = db.change(MembershipChange::Join).await;
        assert_eq!(
            db.state().await,
            (Membership::Invited, Some(MembershipChange::Join))
        );
        // Not joined yet: undo cancels; redo asks again.
        let cancel = db.dispatcher.undo(join).await.unwrap();
        assert_eq!(db.state().await, (Membership::Invited, None));
        let redo = db.dispatcher.undo(cancel).await.unwrap();
        assert_eq!(
            db.state().await,
            (Membership::Invited, Some(MembershipChange::Join))
        );

        // Declining instead.
        db.dispatcher.undo(redo).await.unwrap();
        db.change(MembershipChange::Leave).await;
        assert_eq!(
            db.state().await,
            (Membership::Invited, Some(MembershipChange::Leave))
        );
    }

    #[tokio::test]
    async fn a_change_that_took_effect_is_reversed() {
        let db = Db::new();
        db.sync_membership(Membership::Invited).await;
        let join = db.change(MembershipChange::Join).await;
        db.reconcile(MembershipChange::Join, None).await;
        assert_eq!(db.state().await, (Membership::Joined, None));
        let leave = db.dispatcher.undo(join).await.unwrap();
        assert_eq!(
            db.state().await,
            (Membership::Joined, Some(MembershipChange::Leave))
        );
        // Left: the room is listed no more, unless joining it again.
        db.reconcile(MembershipChange::Leave, None).await;
        let ServerResponse::Rooms { rooms, .. } = db.matrix.list_rooms().await.unwrap() else {
            panic!()
        };
        assert!(rooms.is_empty());
        db.dispatcher.undo(leave).await.unwrap();
        assert_eq!(
            db.state().await,
            (Membership::Left, Some(MembershipChange::Join))
        );
        // The sync's view settles it as well as the sync task's report.
        db.sync_membership(Membership::Joined).await;
        assert_eq!(db.state().await, (Membership::Joined, None));
    }

    /// The rooms the account's outbox lists as being joined or left.
    fn memberships(db: &Db) -> Vec<uplink_protocol::matrix::PendingMembership> {
        match db.outbox().into_iter().next().map(|a| a.pending) {
            Some(Pending::Matrix { memberships, .. }) => memberships,
            None => vec![],
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn refusals_wait_for_a_decision_and_other_changes_conflict() {
        let db = Db::new();
        db.sync_membership(Membership::Invited).await;
        let join = db.change(MembershipChange::Join).await;
        let [pending] = &memberships(&db)[..] else {
            panic!()
        };
        assert_eq!(
            (
                pending.change,
                pending.op.id,
                &pending.op.description[..],
                &pending.refused
            ),
            (MembershipChange::Join, join, "Joined 'Room'", &None)
        );
        let code = db
            .matrix
            .change_membership(db.room().await.id, MembershipChange::Leave)
            .await
            .unwrap_err()
            .code;
        assert_eq!(code, ErrorCode::Conflict, "a change is pending");

        // Refused: still asked for, and not asked again, until undone.
        db.reconcile(MembershipChange::Join, Some("M_FORBIDDEN"))
            .await;
        let room = db.room().await;
        assert_eq!(
            (room.membership, room.pending),
            (Membership::Invited, Some(MembershipChange::Join))
        );
        assert_eq!(room.membership_error.as_deref(), Some("M_FORBIDDEN"));
        assert_eq!(memberships(&db)[0].refused.as_deref(), Some("M_FORBIDDEN"));
        let conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        assert!(crate::membership::wanted(&conn, "mx").unwrap().is_empty());
        db.dispatcher.undo(join).await.unwrap();
        let room = db.room().await;
        assert_eq!((room.pending, room.membership_error), (None, None));
        assert!(memberships(&db).is_empty());

        // Joined otherwise: nothing left to ask for.
        db.sync_membership(Membership::Joined).await;
        assert_eq!(
            db.matrix
                .change_membership(db.room().await.id, MembershipChange::Join)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
    }

    /// A membership asked for that sync sees had is no longer pending.
    #[tokio::test]
    async fn sync_settles_what_it_sees() {
        let db = Db::new();
        db.sync_membership(Membership::Invited).await;
        db.change(MembershipChange::Join).await;
        db.reconcile(MembershipChange::Join, Some("M_FORBIDDEN"))
            .await;
        db.sync_membership(Membership::Joined).await;
        let room = db.room().await;
        assert_eq!((room.pending, room.membership_error), (None, None));
        assert!(memberships(&db).is_empty());
    }
}

mod room_tags {
    use super::*;
    use crate::room_tags::RoomTagSet;
    use uplink_protocol::{
        matrix::{PendingRoomTags, RoomTagChange},
        ErrorCode, ServerError,
    };

    const FAV: &str = "m.favourite";
    const LOW: &str = "m.lowpriority";

    impl Db {
        /// Syncs the room, its tags as the homeserver has them.
        async fn sync_tags(&self, tags: &[&str]) -> TxLogId {
            self.sync_tags_requested(tags, FRESH).await
        }

        /// The latest entry.
        fn head(&self) -> TxLogId {
            let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            uplink_tx_log::as_of(&tx).unwrap()
        }

        /// Syncs the room as requested after entry `requested`, its tags as
        /// the homeserver then had them.
        async fn sync_tags_requested(&self, tags: &[&str], requested: TxLogId) -> TxLogId {
            self.observe(Synced {
                requested,
                next_batch: "next".into(),
                packs: None,
                rooms: vec![ObservedRoom {
                    room_id: ROOM.into(),
                    name: "Room".into(),
                    encrypted: false,
                    membership: Membership::Joined,
                    events: vec![],
                    limited: false,
                    own_receipts: vec![],
                    fully_read: None,
                    image_packs: vec![],
                    space: false,
                    direct: false,
                    avatar: None,
                    bridge: None,
                    children: vec![],
                    space_order: None,
                    tags: tags.iter().map(|t| t.to_string()).collect(),
                    topic: None,
                    alias: None,
                    pinned: vec![],
                    may_pin: false,
                    joined_members: 1,
                    created: None,
                    version: "11".into(),
                }],
            })
            .await
            .unwrap()
        }

        async fn tag(&self, add: &[&str], remove: &[&str]) -> Result<TxLogId, ServerError> {
            let room = self.room().await.id;
            let strings = |tags: &[&str]| tags.iter().map(|t| t.to_string()).collect();
            match self
                .matrix
                .modify_room_tags(vec![room], strings(add), strings(remove))
                .await?
            {
                ServerResponse::Dispatched { tx_log_id } => Ok(tx_log_id),
                other => panic!("unexpected {other:?}"),
            }
        }

        /// The homeserver tagged the room, or untagged it, or refused.
        async fn tag_set(&self, tag: &str, tagged: bool, refused: Option<&str>) {
            let room = self.room().await.id;
            self.observe(RoomTagSet {
                room,
                tag: tag.into(),
                tagged,
                refused: refused.map(Into::into),
            })
            .await
            .unwrap();
        }

        async fn tags(&self) -> Vec<String> {
            self.room().await.tags
        }

        /// The tags still to set, `+tag` or `-tag`.
        fn wanted_tags(&self) -> Vec<String> {
            let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
            crate::room_tags::wanted(&conn, "mx")
                .unwrap()
                .into_iter()
                .map(|w| format!("{}{}", if w.desired { '+' } else { '-' }, w.tag))
                .collect()
        }

        fn pending_tags(&self) -> Vec<PendingRoomTags> {
            match self.outbox().into_iter().next().map(|a| a.pending) {
                Some(Pending::Matrix { room_tags, .. }) => room_tags,
                None => vec![],
                other => panic!("{other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn tags_show_at_once_and_are_set_then_undone() {
        let db = Db::new();
        db.sync_tags(&["u.work"]).await;
        assert_eq!(db.tags().await, ["u.work"]);
        let tagged = db.tag(&[FAV], &[]).await.unwrap();
        assert_eq!(db.tags().await, [FAV, "u.work"]);
        assert_eq!(db.wanted_tags(), ["+m.favourite"]);
        let room = db.room().await.id;
        let [pending] = &db.pending_tags()[..] else {
            panic!()
        };
        assert_eq!(
            (pending.op.id, &pending.op.description[..], &pending.refused),
            (tagged, "Tagged 'Room' Favourite", &None)
        );
        assert_eq!(
            pending.changes,
            [RoomTagChange {
                room,
                tag: FAV.into(),
                desired: true
            }]
        );

        db.tag_set(FAV, true, None).await;
        assert!(db.wanted_tags().is_empty());
        assert!(db.pending_tags().is_empty());
        // The echo changes nothing.
        db.sync_tags(&[FAV, "u.work"]).await;
        assert_eq!(db.tags().await, [FAV, "u.work"]);

        db.dispatcher.undo(tagged).await.unwrap();
        assert_eq!(db.tags().await, ["u.work"]);
        assert_eq!(db.wanted_tags(), ["-m.favourite"]);
        // Had as asked for, however it came about.
        db.sync_tags(&["u.work"]).await;
        assert!(db.wanted_tags().is_empty());
        assert!(db.pending_tags().is_empty());
    }

    /// The homeserver sets tags one by one, and a sync requested before
    /// it set one may tell of them as they were before: they are not taken
    /// for its, but read again, as what is requested after tells of them.
    #[tokio::test]
    async fn tags_told_of_as_before_they_were_set_are_read_again() {
        use crate::room_tags::{set_since, stale, RoomTagsRead};
        let db = Db::new();
        db.sync_tags(&[LOW]).await;
        let room = db.room().await.id;
        db.tag(&[FAV], &[]).await.unwrap();
        let before = db.head();
        db.tag_set(LOW, false, None).await;
        db.tag_set(FAV, true, None).await;
        assert!(db.pending_tags().is_empty());
        // Of between the two: stale, so not the homeserver's.
        db.sync_tags_requested(&[], before).await;
        assert_eq!(db.tags().await, [FAV]);
        let conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        assert_eq!(
            set_since(&conn, "mx", before).unwrap(),
            [(room, ROOM.to_owned())]
        );
        // Agreeing, it is of after, whenever requested.
        db.sync_tags_requested(&[FAV], before).await;
        assert_eq!(db.tags().await, [FAV]);
        // Read since, they are the homeserver's: another client's.
        let requested = db.head();
        assert!(!stale(&conn, room, requested).unwrap());
        db.observe(RoomTagsRead {
            room,
            tags: vec!["u.other".into()],
            requested,
        })
        .await
        .unwrap();
        assert_eq!(db.tags().await, ["u.other"]);
        // As is what a sync requested after tells of.
        db.sync_tags_requested(&[LOW], db.head()).await;
        assert_eq!(db.tags().await, [LOW]);
    }

    #[tokio::test]
    async fn a_favourite_is_not_of_low_priority() {
        let db = Db::new();
        db.sync_tags(&[LOW]).await;
        let tagged = db.tag(&[FAV], &[]).await.unwrap();
        assert_eq!(db.tags().await, [FAV]);
        assert_eq!(db.wanted_tags(), ["+m.favourite", "-m.lowpriority"]);
        assert_eq!(
            db.pending_tags()[0].op.description,
            "Tagged 'Room' Favourite"
        );
        db.dispatcher.undo(tagged).await.unwrap();
        assert_eq!(db.tags().await, [LOW]);
        assert!(db.wanted_tags().is_empty());
        let code = db.tag(&[FAV, LOW], &[]).await.unwrap_err().code;
        assert_eq!(code, ErrorCode::Invalid);
    }

    #[tokio::test]
    async fn others_changes_show_unless_one_is_asked_for() {
        let db = Db::new();
        db.sync_tags(&[]).await;
        db.tag(&["u.a"], &[]).await.unwrap();
        // Another client's tags meanwhile: those asked for prevail.
        db.sync_tags(&["u.b"]).await;
        assert_eq!(db.tags().await, ["u.a", "u.b"]);
        assert_eq!(db.wanted_tags(), ["+u.a"]);
        db.sync_tags(&[]).await;
        assert_eq!(db.tags().await, ["u.a"]);
        // Removing a tag the room lacks changes nothing.
        let entry = db.tag(&[], &["u.b"]).await.unwrap();
        assert_eq!(db.events_of(entry), []);
    }

    #[tokio::test]
    async fn refusals_wait_for_a_decision() {
        let db = Db::new();
        db.sync_tags(&[]).await;
        let tagged = db.tag(&["u.x"], &[]).await.unwrap();
        db.tag_set("u.x", true, Some("M_FORBIDDEN")).await;
        assert!(db.wanted_tags().is_empty(), "not asked again");
        assert_eq!(db.pending_tags()[0].refused.as_deref(), Some("M_FORBIDDEN"));
        assert_eq!(db.tags().await, ["u.x"]);
        db.dispatcher.undo(tagged).await.unwrap();
        assert!(db.tags().await.is_empty());
        assert!(db.pending_tags().is_empty());
    }

    #[tokio::test]
    async fn only_valid_tags_are_added() {
        let db = Db::new();
        db.sync_tags(&["legacy"]).await;
        let long = format!("u.{}", "a".repeat(254));
        for tag in ["work", "m.other", "u.", "u. ", &long] {
            let code = db.tag(&[tag], &[]).await.unwrap_err().code;
            assert_eq!(code, ErrorCode::Invalid, "{tag}");
        }
        let code = db.tag(&["u.a"], &["u.a"]).await.unwrap_err().code;
        assert_eq!(code, ErrorCode::Invalid, "added and removed");
        let code = db.tag(&[], &[]).await.unwrap_err().code;
        assert_eq!(code, ErrorCode::Invalid, "nothing to change");
        // Another client's tag may be removed.
        db.tag(&[], &["legacy"]).await.unwrap();
        assert!(db.tags().await.is_empty());
    }

    #[test]
    fn tags_are_the_keys_of_the_content() {
        let parse = |content| crate::room_tags::parse(&content).unwrap();
        assert_eq!(
            parse(json!({ "tags": { "m.favourite": { "order": 0.5 }, "u.x": {} } })),
            ["m.favourite", "u.x"]
        );
        assert!(parse(json!({})).is_empty());
        assert!(crate::room_tags::parse(&json!({ "tags": [] })).is_err());
    }
}

mod space_order {
    use super::*;
    use crate::space_order::SpaceOrderSet;
    use uplink_protocol::{
        matrix::{PendingSpaceOrders, SpaceOrderChange},
        ErrorCode, ServerError,
    };

    impl Db {
        /// Syncs the room as a joined space, its order as the homeserver
        /// has it.
        async fn sync_space(&self, order: Option<&str>) {
            self.observe(Synced {
                requested: FRESH,
                next_batch: "next".into(),
                packs: None,
                rooms: vec![ObservedRoom {
                    room_id: ROOM.into(),
                    name: "Space".into(),
                    encrypted: false,
                    membership: Membership::Joined,
                    events: vec![],
                    limited: false,
                    own_receipts: vec![],
                    fully_read: None,
                    image_packs: vec![],
                    space: true,
                    direct: false,
                    avatar: None,
                    bridge: None,
                    children: vec![],
                    space_order: order.map(Into::into),
                    tags: vec![],
                    topic: None,
                    alias: None,
                    pinned: vec![],
                    may_pin: false,
                    joined_members: 1,
                    created: None,
                    version: "11".into(),
                }],
            })
            .await
            .unwrap();
        }

        async fn reorder(
            &self,
            from: Option<&str>,
            to: Option<&str>,
        ) -> Result<TxLogId, ServerError> {
            let space = self.room().await.id;
            let change = SpaceOrderChange {
                space,
                from: from.map(Into::into),
                to: to.map(Into::into),
            };
            match self.matrix.order_spaces(vec![change]).await? {
                ServerResponse::Dispatched { tx_log_id } => Ok(tx_log_id),
                other => panic!("unexpected {other:?}"),
            }
        }

        /// The homeserver set the order, or refused.
        async fn pushed(&self, order: Option<&str>, refused: Option<&str>) {
            let room = self.room().await.id;
            self.observe(SpaceOrderSet {
                room,
                order: order.map(Into::into),
                refused: refused.map(Into::into),
            })
            .await
            .unwrap();
        }

        /// The orders still to set.
        fn wanted(&self) -> Vec<Option<String>> {
            let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
            crate::space_order::wanted(&conn, "mx")
                .unwrap()
                .into_iter()
                .map(|w| w.order)
                .collect()
        }

        fn pending_orders(&self) -> Vec<PendingSpaceOrders> {
            match self.outbox().into_iter().next().map(|a| a.pending) {
                Some(Pending::Matrix { space_orders, .. }) => space_orders,
                None => vec![],
                other => panic!("{other:?}"),
            }
        }

        async fn order(&self) -> Option<String> {
            self.room().await.space_order
        }
    }

    #[tokio::test]
    async fn reordering_shows_at_once_and_is_set_then_undone() {
        let db = Db::new();
        db.sync_space(Some("b")).await;
        assert_eq!(db.order().await.as_deref(), Some("b"));
        let moved = db.reorder(Some("b"), Some("a")).await.unwrap();
        assert_eq!(db.order().await.as_deref(), Some("a"));
        assert_eq!(db.wanted(), [Some("a".to_owned())]);
        let [pending] = &db.pending_orders()[..] else {
            panic!()
        };
        assert_eq!(
            (
                pending.op.id,
                &pending.op.description[..],
                &pending.spaces[..],
                &pending.refused
            ),
            (moved, "Moved 'Space'", &[db.room().await.id][..], &None)
        );

        db.pushed(Some("a"), None).await;
        assert!(db.wanted().is_empty());
        assert!(db.pending_orders().is_empty());
        assert_eq!(db.order().await.as_deref(), Some("a"));
        // The echo changes nothing.
        db.sync_space(Some("a")).await;
        assert!(db.pending_orders().is_empty());

        // Undoing asks for the order replaced, which sync sees had.
        db.dispatcher.undo(moved).await.unwrap();
        assert_eq!(db.order().await.as_deref(), Some("b"));
        assert_eq!(db.wanted(), [Some("b".to_owned())]);
        db.sync_space(Some("b")).await;
        assert!(db.wanted().is_empty());
        assert!(db.pending_orders().is_empty());
    }

    #[tokio::test]
    async fn no_order_is_asked_for_as_none() {
        let db = Db::new();
        db.sync_space(None).await;
        let ordered = db.reorder(None, Some("m")).await.unwrap();
        db.pushed(Some("m"), None).await;
        db.dispatcher.undo(ordered).await.unwrap();
        assert_eq!((db.order().await, db.wanted()), (None, vec![None]));
        db.pushed(None, None).await;
        assert!(db.pending_orders().is_empty());
    }

    #[tokio::test]
    async fn changes_since_conflict_and_others_prevail_until_pushed() {
        let db = Db::new();
        db.sync_space(Some("b")).await;
        let code = db.reorder(Some("c"), Some("a")).await.unwrap_err().code;
        assert_eq!(code, ErrorCode::Conflict, "moved since it was shown");

        let moved = db.reorder(Some("b"), Some("a")).await.unwrap();
        // Another client's order meanwhile: uplink's is pushed regardless.
        db.sync_space(Some("z")).await;
        assert_eq!(db.order().await.as_deref(), Some("a"));
        assert_eq!(db.wanted(), [Some("a".to_owned())]);
        // Moved again, undoing the first move conflicts.
        db.reorder(Some("a"), Some("c")).await.unwrap();
        let undo = db.dispatcher.undo(moved).await;
        assert!(undo.is_err(), "{undo:?}");
        // A push of an order no longer asked for leaves the one asked.
        db.pushed(Some("a"), None).await;
        assert_eq!(db.wanted(), [Some("c".to_owned())]);
    }

    #[tokio::test]
    async fn refusals_wait_for_a_decision() {
        let db = Db::new();
        db.sync_space(Some("b")).await;
        let moved = db.reorder(Some("b"), Some("a")).await.unwrap();
        db.pushed(Some("a"), Some("M_FORBIDDEN")).await;
        assert!(db.wanted().is_empty(), "not asked again");
        assert_eq!(
            db.pending_orders()[0].refused.as_deref(),
            Some("M_FORBIDDEN")
        );
        assert_eq!(db.order().await.as_deref(), Some("a"));
        // Undoing decides: the order as the homeserver has it, which is
        // set again, harmlessly.
        db.dispatcher.undo(moved).await.unwrap();
        assert_eq!(db.wanted(), [Some("b".to_owned())]);
        assert_eq!(db.pending_orders()[0].refused, None);
    }

    #[tokio::test]
    async fn only_joined_spaces_move_to_valid_orders() {
        let db = Db::new();
        db.sync_space(None).await;
        for to in ["é", &"a".repeat(51)] {
            let code = db.reorder(None, Some(to)).await.unwrap_err().code;
            assert_eq!(code, ErrorCode::Invalid, "{to}");
        }
        let code = db.reorder(None, None).await.unwrap_err().code;
        assert_eq!(code, ErrorCode::Invalid, "not moved");
        let code = db.matrix.order_spaces(vec![]).await.unwrap_err().code;
        assert_eq!(code, ErrorCode::Invalid, "nothing moved");
        db.reorder(None, Some(&"~".repeat(50))).await.unwrap();

        db.sync(&[], &[]).await;
        let code = db.reorder(None, Some("a")).await.unwrap_err().code;
        assert_eq!(code, ErrorCode::Invalid, "not a space");
    }

    #[test]
    fn orders_are_read_if_valid() {
        use crate::space_order::parse;
        assert_eq!(parse(&json!({ "order": "a b~" })).as_deref(), Some("a b~"));
        for order in [json!("a".repeat(51)), json!("é"), json!("\n"), json!(1)] {
            assert_eq!(parse(&json!({ "order": order })), None);
        }
        assert_eq!(parse(&json!({})), None);
    }
}

#[test]
fn space_children_are_listed_by_via_with_valid_orders() {
    use crate::rooms::SpaceChild;
    let child = |content: serde_json::Value| {
        SpaceChild::parse(
            &json!({ "type": "m.space.child", "state_key": "!r:x", "content": content })
                .to_string(),
        )
        .unwrap()
    };
    assert_eq!(
        child(json!({ "via": ["x"], "order": "a" })),
        Some(SpaceChild {
            room_id: "!r:x".into(),
            order: Some("a".into())
        })
    );
    // Removed: no `via`.
    assert_eq!(child(json!({})), None);
    assert_eq!(child(json!({ "via": [] })), None);
    // Invalid orders are none.
    for order in [json!("a".repeat(51)), json!("é"), json!("\n"), json!(1)] {
        assert_eq!(
            child(json!({ "via": ["x"], "order": order }))
                .unwrap()
                .order,
            None
        );
    }
}

mod relations {
    use super::*;
    use uplink_protocol::matrix::{TimelineChanged, TimelinePage};

    fn text(id: &str, sender: &str, body: &str) -> serde_json::Value {
        json!({ "type": "m.room.message", "event_id": id, "sender": sender,
                "content": { "msgtype": "m.text", "body": body } })
    }

    fn edit(id: &str, sender: &str, of: &str, body: &str) -> serde_json::Value {
        json!({ "type": "m.room.message", "event_id": id, "sender": sender,
                "content": { "msgtype": "m.text", "body": format!("* {body}"),
                             "m.new_content": { "msgtype": "m.text", "body": body },
                             "m.relates_to": { "rel_type": "m.replace", "event_id": of } } })
    }

    fn reaction(id: &str, sender: &str, of: &str, key: &str) -> serde_json::Value {
        json!({ "type": "m.reaction", "event_id": id, "sender": sender,
                "content": { "m.relates_to": { "rel_type": "m.annotation", "event_id": of,
                                               "key": key } } })
    }

    fn redaction(id: &str, sender: &str, of: &str) -> serde_json::Value {
        json!({ "type": "m.room.redaction", "event_id": id, "sender": sender,
                "content": { "redacts": of, "reason": "oops" } })
    }

    fn ids(events: &[RoomEvent]) -> Vec<&str> {
        events.iter().map(|e| e.event_id.as_str()).collect()
    }

    fn edited(event: &RoomEvent) -> Option<String> {
        let content: serde_json::Value =
            serde_json::from_str(&event.edit.as_ref()?.content).unwrap();
        Some(content["body"].as_str().unwrap().to_owned())
    }

    #[tokio::test]
    async fn edits_and_reactions_are_folded_into_what_they_are_of() {
        let db = Db::new();
        db.sync_json(&[
            text("$1", "@bob:x", "helo"),
            edit("$2", "@bob:x", "$1", "hello"),
            edit("$3", "@bob:x", "$1", "hello!"),
            // Only its sender edits a message.
            edit("$4", "@eve:x", "$1", "pwned"),
            reaction("$5", "@eve:x", "$1", "👍"),
            reaction("$6", ME, "$1", "👍"),
            reaction("$7", "@eve:x", "$1", "👍"),
            reaction("$8", ME, "$1", "🎉"),
            // Of an event not stored: listed.
            reaction("$9", ME, "$gone", "👍"),
        ])
        .await;
        let timeline = db.timeline().await;
        assert_eq!(ids(&timeline), ["$1", "$9"]);
        let first = &timeline[0];
        assert_eq!(edited(first).as_deref(), Some("hello!"));
        let reactions: Vec<_> = first
            .reactions
            .iter()
            .map(|r| {
                let senders: Vec<_> = r.senders.iter().map(|s| s.sender.as_str()).collect();
                (r.key.as_str(), senders)
            })
            .collect();
        assert_eq!(
            reactions,
            [("👍", vec!["@eve:x", ME]), ("🎉", vec![ME])],
            "each sender once, in order"
        );
        // Edits and reactions are not unread messages.
        assert_eq!(db.room().await.unread, 1);
        assert!(first.reactions.iter().all(|r| r.shortcode.is_none()));

        let room = db.room().await.id;
        let ServerResponse::Edits { edits, .. } =
            db.matrix.list_edits(room, first.id).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(ids(&edits), ["$2", "$3"], "not another's");
    }

    #[tokio::test]
    async fn custom_reactions_are_named_by_the_first_shortcode_given() {
        let db = Db::new();
        let custom = |id: &str, sender: &str, shortcode: serde_json::Value| {
            let mut event = reaction(id, sender, "$1", "mxc://x/cat");
            event["content"]["com.beeper.reaction.shortcode"] = shortcode;
            event
        };
        db.sync_json(&[
            text("$1", "@bob:x", "hello"),
            reaction("$2", "@bob:x", "$1", "mxc://x/cat"),
            custom("$3", "@eve:x", json!(42)),
            custom("$4", ME, json!(":cat:")),
            custom("$5", "@carol:x", json!(":kitty:")),
        ])
        .await;
        let timeline = db.timeline().await;
        let [cat] = &timeline[0].reactions[..] else {
            panic!("{:?}", timeline[0].reactions)
        };
        assert_eq!(cat.key, "mxc://x/cat");
        assert_eq!(
            cat.shortcode.as_deref(),
            Some(":cat:"),
            "of those that are text"
        );
        assert_eq!(cat.senders.len(), 4);
    }

    #[tokio::test]
    async fn the_accounts_own_messages_are_edited_from_their_html_as_last_edited() {
        let db = Db::new();
        let html = |id: &str, sender: &str, formatted_body: &str| {
            json!({ "type": "m.room.message", "event_id": id, "sender": sender,
                    "content": { "msgtype": "m.text", "body": "~~struck ~~rest",
                                 "format": "org.matrix.custom.html",
                                 "formatted_body": formatted_body } })
        };
        db.sync_json(&[
            html("$1", ME, "<del>struck </del>rest"),
            html("$2", "@bob:x", "<b>Bob's</b>"),
            text("$3", ME, "first"),
            edit("$4", ME, "$3", "**as is**"),
        ])
        .await;
        let timeline = db.timeline().await;
        let room = db.room().await.id;
        let edit_text = |event: &str| {
            let event = timeline.iter().find(|e| e.event_id == event).unwrap().id;
            db.matrix.edit_text(room, event)
        };
        let ServerResponse::EditText { text, .. } = edit_text("$1").await.unwrap() else {
            panic!()
        };
        assert_eq!(text, "<del>struck </del>rest", "as HTML Markdown keeps");
        let ServerResponse::EditText { text, .. } = edit_text("$3").await.unwrap() else {
            panic!()
        };
        assert_eq!(text, "/plain **as is**", "as last edited, plain");
        let refused = edit_text("$2").await.unwrap_err();
        assert_eq!(
            refused.code,
            uplink_protocol::ErrorCode::Invalid,
            "not another's"
        );
    }

    #[tokio::test]
    async fn redactions_redact_and_change_what_shows_them() {
        let db = Db::new();
        db.sync_json(&[
            text("$1", "@bob:x", "secret"),
            reaction("$2", ME, "$1", "👍"),
            json!({ "type": "m.room.message", "event_id": "$3", "sender": "@eve:x",
                    "content": { "msgtype": "m.text", "body": "what?",
                                 "m.relates_to": { "m.in_reply_to": { "event_id": "$1" } } } }),
            edit("$4", "@bob:x", "$1", "more secret"),
        ])
        .await;
        let timeline = db.timeline().await;
        assert_eq!(ids(&timeline), ["$1", "$3"]);
        let replied = timeline[1].replied.as_ref().unwrap();
        assert_eq!(
            (replied.event_id.as_str(), replied.body.as_deref()),
            ("$1", Some("more secret")),
            "a reply shows what it answers as edited"
        );
        let [first, reply] = [timeline[0].id, timeline[1].id];
        let room = db.room().await.id;

        // Taking the reaction back.
        let entry = db.sync_json(&[redaction("$5", ME, "$2")]).await;
        assert!(db
            .events_of(entry)
            .contains(&ServerEvent::TimelineChanged(TimelineChanged {
                room,
                events: vec![first],
            })));
        let timeline = db.timeline().await;
        assert_eq!(ids(&timeline), ["$1", "$3"], "folded still");
        assert!(timeline[0].reactions.is_empty());

        // Deleting the message: it and the reply change.
        let entry = db.sync_json(&[redaction("$6", "@bob:x", "$1")]).await;
        assert!(db
            .events_of(entry)
            .contains(&ServerEvent::TimelineChanged(TimelineChanged {
                room,
                events: vec![first, reply],
            })));
        let timeline = db.timeline().await;
        let message = &timeline[0];
        assert!(message.redacted);
        assert_eq!(message.body, None);
        let raw: serde_json::Value = serde_json::from_str(&message.raw).unwrap();
        assert_eq!(raw["content"], json!({}));
        assert_eq!(raw["unsigned"]["redacted_because"]["event_id"], "$6");
        assert!(timeline[1].replied.as_ref().unwrap().redacted);
        assert_eq!(db.room().await.unread, 1, "only the reply");

        // Pages count only the events listed.
        let ServerResponse::Timeline {
            events,
            more_before,
            ..
        } = db
            .matrix
            .list_timeline(room, TimelinePage::Latest, 1)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!((ids(&events), more_before), (vec!["$3"], true));
        // Pages that reach the end have the sends, which follow it.
        let matrix = &db.matrix;
        let has_sends = |page| async move {
            let ServerResponse::Timeline { sends, .. } =
                matrix.list_timeline(room, page, 1).await.unwrap()
            else {
                panic!()
            };
            sends.is_some()
        };
        let last = events[0].id;
        assert!(has_sends(TimelinePage::Latest).await);
        assert!(!has_sends(TimelinePage::Before { event: last }).await);
        assert!(
            !has_sends(TimelinePage::After { event: first }).await,
            "full"
        );
        assert!(has_sends(TimelinePage::After { event: last }).await);
        let ServerResponse::RoomEvents { events, .. } = db
            .matrix
            .list_room_events(room, vec![reply, first, RoomEventId(9999)])
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(ids(&events), ["$1", "$3"]);
    }

    fn in_thread(
        id: &str,
        sender: &str,
        root: &str,
        reply_to: &str,
        falling_back: bool,
    ) -> serde_json::Value {
        json!({ "type": "m.room.message", "event_id": id, "sender": sender,
                "content": { "msgtype": "m.text", "body": id,
                             "m.relates_to": { "rel_type": "m.thread", "event_id": root,
                                               "is_falling_back": falling_back,
                                               "m.in_reply_to": { "event_id": reply_to } } } })
    }

    #[tokio::test]
    async fn threads_are_listed_apart_and_summed_up_by_their_roots() {
        let db = Db::new();
        // Of a thread whose root is not stored: in the main timeline.
        db.sync_json(&[
            in_thread("$2", "@bob:x", "$1", "$1", true),
            text("$3", "@bob:x", "main"),
        ])
        .await;
        assert_eq!(ids(&db.timeline().await), ["$2", "$3"]);
        let orphan = db.timeline().await[0].id;
        assert_eq!(db.timeline().await[0].thread_root, None);

        // Its root arrives: the message goes to the thread.
        let room = db.room().await.id;
        let entry = db.sync_json(&[text("$1", "@bob:x", "root")]).await;
        assert!(db
            .events_of(entry)
            .contains(&ServerEvent::TimelineChanged(TimelineChanged {
                room,
                events: vec![orphan],
            })));
        db.sync_json(&[
            in_thread("$4", "@eve:x", "$1", "$2", true),
            // Replying within the thread.
            in_thread("$5", ME, "$1", "$2", false),
        ])
        .await;
        let timeline = db.timeline().await;
        assert_eq!(ids(&timeline), ["$3", "$1"]);
        let root = &timeline[1];
        let summary = root.thread.as_ref().unwrap();
        assert_eq!(
            (summary.replies, summary.latest.event_id.as_str()),
            (3, "$5")
        );
        // Thread messages are not unread in the room.
        assert_eq!(db.room().await.unread, 2);

        let ServerResponse::Thread {
            root: listed,
            replies,
            ..
        } = db.matrix.list_thread(room, root.id).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(listed.event_id, "$1");
        assert_eq!(ids(&replies), ["$2", "$4", "$5"]);
        assert!(replies.iter().all(|r| r.thread_root == Some(root.id)));
        // A fallback is no reply; a reply within the thread is.
        assert_eq!(replies[1].replied, None);
        assert_eq!(replies[2].replied.as_ref().unwrap().event_id, "$2");
        // Nor is a message of a thread one's root.
        let reply = replies[0].id;
        assert!(db.matrix.list_thread(room, reply).await.is_err());

        // Sends to a thread: to its root, replying within it, or neither.
        let to_thread = |thread: &str, reply_to: Option<&str>, replaces: Option<&str>| {
            uplink_protocol::matrix::MatrixContent::Text {
                body: "hi".into(),
                format: TextFormat::Markdown,
                emote: false,
                reply_to: reply_to.map(Into::into),
                replaces: replaces.map(Into::into),
                thread: Some(thread.into()),
            }
        };
        for content in [
            to_thread("$1", None, None),
            to_thread("$1", Some("$4"), None),
        ] {
            db.matrix.send_message(room, key(), content).await.unwrap();
        }
        for content in [
            to_thread("$2", None, None),
            to_thread("$1", Some("$3"), None),
            to_thread("$1", None, Some("$5")),
        ] {
            let refused = db
                .matrix
                .send_message(room, key(), content)
                .await
                .unwrap_err();
            assert_eq!(refused.code, uplink_protocol::ErrorCode::Invalid);
        }

        // Editing the latest changes the root's summary.
        let entry = db.sync_json(&[edit("$6", ME, "$5", "edited")]).await;
        let changed = db.events_of(entry);
        assert!(
            changed
                .iter()
                .any(|e| matches!(e, ServerEvent::TimelineChanged(c)
                if c.events.contains(&root.id))),
            "{changed:?}"
        );
        let timeline = db.timeline().await;
        let latest = &timeline[1].thread.as_ref().unwrap().latest;
        assert_eq!(latest.body.as_deref(), Some("edited"));
    }

    #[tokio::test]
    async fn only_the_links_of_messages_are_previewed_and_not_in_encrypted_rooms() {
        let db = Db::new();
        db.sync_json(&[text("$1", "@bob:x", "see https://example.org/a")])
            .await;
        let room = db.room().await.id;
        let event = db.timeline().await[0].id;
        let refused = |url: &str| {
            let db = &db;
            let url = url.to_owned();
            async move { db.matrix.preview_url(room, event, url).await.unwrap_err() }
        };
        let unlinked = refused("https://example.org/b").await;
        assert_eq!(unlinked.code, uplink_protocol::ErrorCode::NotFound);
        assert!(unlinked.message.contains("links no"), "{unlinked:?}");
        // Linked: it goes as far as the homeserver, which tests have none of.
        let linked = refused("https://example.org/a").await;
        assert!(linked.message.contains("not connected"), "{linked:?}");

        uplink_storage::open(db._dir.path())
            .unwrap()
            .execute("UPDATE matrix_room SET encrypted = 1", [])
            .unwrap();
        let encrypted = refused("https://example.org/a").await;
        assert_eq!(encrypted.code, uplink_protocol::ErrorCode::Invalid);
    }

    #[tokio::test]
    async fn messages_are_searched_by_their_words_as_edited_newest_first() {
        let db = Db::new();
        db.sync_json(&[
            text("$1", "@bob:x", "Lunch at noon?"),
            text("$2", "@eve:x", "lunch AT one"),
            text("$3", "@bob:x", "dinner"),
            edit("$4", "@bob:x", "$3", "late lunch, at 3"),
            text("$5", "@bob:x", "100% lunch_at"),
            text("$6", "@bob:x", "lunch at the moon"),
            redaction("$7", "@bob:x", "$6"),
        ])
        .await;
        let room = db.room().await.id;
        let search = |query: &str, before: Option<RoomEventId>, limit: u32| {
            let db = &db;
            let query = query.to_owned();
            async move {
                match db
                    .matrix
                    .search_room(room, query, before, limit)
                    .await
                    .unwrap()
                {
                    ServerResponse::RoomSearch { events, more, .. } => (events, more),
                    other => panic!("unexpected {other:?}"),
                }
            }
        };
        let (found, more) = search("LUNCH at", None, 10).await;
        assert_eq!(
            ids(&found),
            ["$5", "$3", "$2", "$1"],
            "as edited; not deleted"
        );
        assert!(!more);
        // `%` and `_` are themselves.
        assert_eq!(ids(&search("0% h_a", None, 10).await.0), ["$5"]);
        assert!(search("lunch%at", None, 10).await.0.is_empty());
        // Page by page.
        let (page, more) = search("lunch", None, 2).await;
        assert_eq!((ids(&page), more), (vec!["$5", "$3"], true));
        let (page, more) = search("lunch", Some(page[1].id), 2).await;
        assert_eq!((ids(&page), more), (vec!["$2", "$1"], false));
        assert!(db
            .matrix
            .search_room(room, " ".into(), None, 10)
            .await
            .is_err());
    }

    #[test]
    fn redacting_follows_the_room_version_and_survives_non_canonical_json() {
        use matrix_sdk::ruma::RoomVersionId;
        let rules = RoomVersionId::V11.rules().unwrap().redaction;
        let because = redaction("$r", "@a:x", "$m").to_string();
        let member = json!({ "type": "m.room.member", "event_id": "$m", "sender": "@a:x",
                             "state_key": "@a:x", "origin_server_ts": 1,
                             "content": { "membership": "join", "displayname": "A" } });
        let redacted: serde_json::Value = serde_json::from_str(
            &crate::rooms::redacted_json(&member.to_string(), &because, &rules).unwrap(),
        )
        .unwrap();
        assert_eq!(redacted["content"], json!({ "membership": "join" }));
        let float = r#"{"type":"m.room.message","event_id":"$m","sender":"@a:x",
                        "origin_server_ts":1,"content":{"body":"x","n":1.5}}"#;
        let redacted: serde_json::Value =
            serde_json::from_str(&crate::rooms::redacted_json(float, &because, &rules).unwrap())
                .unwrap();
        assert_eq!(redacted["content"], json!({}));
        assert_eq!(redacted["unsigned"]["redacted_because"]["event_id"], "$r");
    }
}

/// A plain message.
fn text(body: &str) -> uplink_protocol::matrix::MatrixContent {
    uplink_protocol::matrix::MatrixContent::Text {
        body: body.into(),
        format: TextFormat::Markdown,
        emote: false,
        reply_to: None,
        replaces: None,
        thread: None,
    }
}

#[test]
fn messages_mention_the_users_they_link_and_whom_they_reply_to() {
    use crate::sync::text_content;
    let content = text_content(
        "hi [Bob](https://matrix.to/#/@bob:example.org), [a link](https://example.org) [x](https://matrix.to/#/nobody)",
        TextFormat::Markdown,
        false,
        Some(("$q", "@carol:example.org")),
        None,
        None,
    );
    assert_eq!(
        content["body"],
        "hi Bob, [a link](https://example.org) [x](https://matrix.to/#/nobody)"
    );
    assert_eq!(
        content["m.mentions"],
        json!({ "user_ids": ["@bob:example.org", "@carol:example.org"] })
    );
    assert!(content["formatted_body"]
        .as_str()
        .unwrap()
        .contains(r#"<a href="https://matrix.to/#/@bob:example.org">Bob</a>"#));
    assert_eq!(content["m.relates_to"]["m.in_reply_to"]["event_id"], "$q");

    // Plain: as it is, mentioning no one.
    let plain = text_content(
        "**[Bob](https://matrix.to/#/@bob:example.org)**",
        TextFormat::Plain,
        false,
        None,
        None,
        None,
    );
    assert_eq!(
        plain,
        json!({
            "msgtype": "m.text",
            "body": "**[Bob](https://matrix.to/#/@bob:example.org)**",
            "m.mentions": {},
        })
    );

    // An edit mentions none anew; its new content, all.
    let edit = text_content(
        "**[Bob](https://matrix.to/#/@bob:example.org)!**",
        TextFormat::Markdown,
        true,
        None,
        None,
        Some("$e"),
    );
    assert_eq!(edit["body"], "* **Bob!**");
    // Marked as Element marks it, not formatted as a list.
    assert_eq!(
        edit["formatted_body"],
        r#"* <strong><a href="https://matrix.to/#/@bob:example.org">Bob</a>!</strong>"#
    );
    assert_eq!(
        edit["m.new_content"]["formatted_body"],
        r#"<strong><a href="https://matrix.to/#/@bob:example.org">Bob</a>!</strong>"#
    );
    assert_eq!(edit["msgtype"], "m.emote");
    assert_eq!(edit["m.mentions"], json!({}));
    assert_eq!(
        edit["m.new_content"]["m.mentions"],
        json!({ "user_ids": ["@bob:example.org"] })
    );

    // HTML: as it is, its plain body Markdown, its links to users
    // mentioning them.
    let html = text_content(
        r#"<del>struck </del>for <a href="https://matrix.to/#/@bob:example.org">Bob</a>"#,
        TextFormat::Html,
        false,
        None,
        None,
        None,
    );
    assert_eq!(
        html,
        json!({
            "msgtype": "m.text",
            "body": "~~struck~~ for Bob",
            "format": "org.matrix.custom.html",
            "formatted_body": r#"<del>struck </del>for <a href="https://matrix.to/#/@bob:example.org">Bob</a>"#,
            "m.mentions": { "user_ids": ["@bob:example.org"] },
        })
    );

    // Of a thread: answering its latest by fallback, mentioning no one;
    // or replying within it.
    let fallback = text_content(
        "hi",
        TextFormat::Markdown,
        false,
        None,
        Some(("$root", "$latest")),
        None,
    );
    assert_eq!(
        fallback["m.relates_to"],
        json!({
            "rel_type": "m.thread",
            "event_id": "$root",
            "is_falling_back": true,
            "m.in_reply_to": { "event_id": "$latest" },
        })
    );
    assert_eq!(fallback["m.mentions"], json!({}));
    let reply = text_content(
        "hi",
        TextFormat::Markdown,
        false,
        Some(("$q", "@carol:example.org")),
        Some(("$root", "$latest")),
        None,
    );
    assert_eq!(
        reply["m.relates_to"],
        json!({
            "rel_type": "m.thread",
            "event_id": "$root",
            "is_falling_back": false,
            "m.in_reply_to": { "event_id": "$q" },
        })
    );
    assert_eq!(
        reply["m.mentions"],
        json!({ "user_ids": ["@carol:example.org"] })
    );

    // `@room`, a word of its own, Markdown or not, mentions the room.
    for (text, format, room) in [
        ("@room: lunch!", TextFormat::Markdown, true),
        ("lunch, @room", TextFormat::Plain, true),
        ("<b>lunch</b>, @room", TextFormat::Html, true),
        ("(@room)", TextFormat::Markdown, true),
        ("@roomy", TextFormat::Markdown, false),
        ("@room:example.org", TextFormat::Markdown, false),
        ("me@room", TextFormat::Markdown, false),
    ] {
        let content = text_content(text, format, false, None, None, None);
        let expected = if room {
            json!({ "room": true })
        } else {
            json!({})
        };
        assert_eq!(content["m.mentions"], expected, "{text}");
    }
}

mod query {
    use super::*;

    fn room(id: &str, name: &str) -> ObservedRoom {
        ObservedRoom {
            room_id: id.into(),
            name: name.into(),
            encrypted: false,
            membership: Membership::Joined,
            events: vec![],
            limited: false,
            own_receipts: vec![],
            fully_read: None,
            image_packs: vec![],
            space: false,
            direct: false,
            avatar: None,
            bridge: None,
            children: vec![],
            space_order: None,
            tags: vec![],
            topic: None,
            alias: None,
            pinned: vec![],
            may_pin: false,
            joined_members: 2,
            created: None,
            version: "11".into(),
        }
    }

    #[tokio::test]
    async fn rooms_say_how_much_history_is_stored_and_searches_which_they_miss() {
        use uplink_protocol::matrix::RoomHistory;
        let db = Db::new();
        // Synced with a gap before its event: of two, fetched; of three,
        // not; and one of nothing missing.
        let mut small = room("!small:x", "Small");
        small.events = vec![message("$1", "@bob:x")];
        small.limited = true;
        small.tags = vec!["u.Projects".into()];
        small.created = Some(1_600_000_000_000);
        let mut large = room("!large:x", "Large");
        large.events = vec![message("$2", "@bob:x")];
        large.limited = true;
        large.joined_members = 3;
        let mut whole = room("!whole:x", "Whole");
        whole.events = vec![message("$3", "@bob:x")];
        db.observe(Synced {
            requested: FRESH,
            next_batch: "next".into(),
            packs: None,
            rooms: vec![small, large, whole],
        })
        .await
        .unwrap();
        let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        let history = |name: &str| {
            crate::rooms::rooms(&tx, &account(), None)
                .unwrap()
                .into_iter()
                .find(|r| r.name == name)
                .unwrap()
                .history
        };
        let since = 1_700_000_000_000; // `message` events' time
        assert_eq!(
            history("Small"),
            RoomHistory::Fetching {
                since,
                created: Some(1_600_000_000_000)
            }
        );
        assert_eq!(history("Large"), RoomHistory::Partial { since });
        assert_eq!(history("Whole"), RoomHistory::Whole);

        let rooms = crate::query::Rooms::of(&db.rooms, &tx).unwrap();
        let partly = |query: &str| -> Vec<String> {
            let query = uplink_query::parse(query, 0).unwrap();
            let mut names: Vec<String> = rooms
                .partly_searched(&tx, &query)
                .unwrap()
                .into_iter()
                .map(|id| rooms.room(id).unwrap().name.clone())
                .collect();
            names.sort();
            names
        };
        assert_eq!(partly("hello"), ["Large", "Small"]);
        assert_eq!(partly("hello tag:projects"), ["Small"]);
        // Decided by the rooms' own terms: no message is looked at.
        assert!(partly("is:dm").is_empty());
    }

    #[tokio::test]
    async fn rooms_offer_the_accounts_packs_their_own_their_spaces_and_those_used_everywhere() {
        use crate::{image_packs, rooms::SpaceChild};
        let pack = |name: Option<&str>, images: &[&str]| {
            let images: serde_json::Map<_, _> = images
                .iter()
                .map(|i| (i.to_string(), json!({ "url": format!("mxc://x/{i}") })))
                .collect();
            image_packs::Pack::parse(&json!({ "pack": { "display_name": name }, "images": images }))
        };
        let child = |room_id: &str| SpaceChild {
            room_id: room_id.into(),
            order: None,
        };
        let db = Db::new();
        let mut lounge = room("!lounge:x", "Lounge");
        lounge.image_packs = vec![(String::new(), pack(None, &["cat"]))];
        let mut slack = room("!slack:x", "Slack");
        slack.space = true;
        slack.children = vec![child("!lounge:x"), child("!outer:x")];
        slack.image_packs = vec![
            ("work.000".into(), pack(Some("Work"), &["parrot"])),
            ("work.001".into(), pack(Some("Work"), &["shipit"])),
        ];
        // Spaces may list each other.
        let mut outer = room("!outer:x", "Outer");
        outer.space = true;
        outer.children = vec![child("!slack:x")];
        outer.image_packs = vec![("o".into(), pack(Some("Outer"), &["o"]))];
        let mut left = room("!left:x", "Left");
        left.space = true;
        left.membership = Membership::Left;
        left.children = vec![child("!lounge:x")];
        left.image_packs = vec![("l".into(), pack(None, &["l"]))];
        let mut global = room("!global:x", "Global");
        global.image_packs = vec![
            ("g".into(), pack(None, &["g"])),
            ("other".into(), pack(None, &["other"])),
        ];
        db.observe(Synced {
            requested: FRESH,
            next_batch: "next".into(),
            packs: Some(image_packs::AccountPacks {
                user: Some(pack(None, &["mine"])),
                everywhere: vec![
                    ("!global:x".into(), "g".into()),
                    ("!lounge:x".into(), String::new()),
                ],
            }),
            rooms: vec![lounge, slack, outer, left, global],
        })
        .await
        .unwrap();
        let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        let id = |room_id: &str| -> RoomId {
            tx.query_row(
                "SELECT id FROM matrix_room WHERE room_id = ?1",
                [room_id],
                |r| Ok(RoomId(r.get(0)?)),
            )
            .unwrap()
        };
        let offered = |room: RoomId| -> Vec<(Option<String>, Vec<String>)> {
            image_packs::offered(&tx, room)
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|p| (p.name, p.images.into_iter().map(|i| i.shortcode).collect()))
                .collect()
        };
        let named = |name: Option<&str>, images: &[&str]| {
            (
                name.map(str::to_owned),
                images.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            offered(id("!lounge:x")),
            [
                named(None, &["mine"]),
                named(Some("Lounge"), &["cat"]),
                named(Some("Outer"), &["o"]),
                named(Some("Work"), &["parrot"]),
                named(Some("Work"), &["shipit"]),
                named(Some("Global"), &["g"]),
            ],
            "once each, not of spaces left"
        );
        assert_eq!(
            offered(id("!global:x")),
            [
                named(None, &["mine"]),
                named(Some("Global"), &["g"]),
                named(Some("Global"), &["other"]),
                named(Some("Lounge"), &["cat"]),
            ]
        );
        assert!(image_packs::offered(&tx, RoomId(-1)).unwrap().is_none());
        let offering = |room: &str, image: &str| {
            image_packs::offering(&tx, id(room), &format!("mxc://x/{image}")).unwrap()
        };
        assert_eq!(offering("!lounge:x", "parrot"), Some(account().id));
        assert_eq!(offering("!lounge:x", "other"), None);
        assert_eq!(offering("!lounge:x", "l"), None);
        drop(tx);

        // Packs change as synced.
        let mut lounge = room("!lounge:x", "Lounge");
        lounge.image_packs = vec![(String::new(), pack(None, &["dog"]))];
        db.observe(Synced {
            requested: FRESH,
            next_batch: "next".into(),
            packs: None,
            rooms: vec![lounge],
        })
        .await
        .unwrap();
        let tx = conn.transaction().unwrap();
        let lounge: RoomId = tx
            .query_row(
                "SELECT id FROM matrix_room WHERE room_id = '!lounge:x'",
                [],
                |r| Ok(RoomId(r.get(0)?)),
            )
            .unwrap();
        let packs = image_packs::offered(&tx, lounge).unwrap().unwrap();
        assert_eq!(packs[1].images[0].url, "mxc://x/dog");
        assert_eq!(
            packs[0].images[0].shortcode, "mine",
            "the account's own kept"
        );
    }

    #[tokio::test]
    async fn queries_match_rooms_by_name_tags_spaces_and_what_is_unread() {
        let db = Db::new();
        let mut work = room("!work:x", "Work");
        work.space = true;
        work.children = vec![crate::rooms::SpaceChild {
            room_id: "!team:x".into(),
            order: None,
        }];
        let mut team = room("!team:x", "Team chat");
        team.tags = vec!["u.Projects".into()];
        team.events = vec![message("$1", "@bob:x")];
        let mut bob = room("!bob:x", "Bob");
        bob.direct = true;
        let mut news = room("!news:x", "News");
        news.tags = vec!["m.lowpriority".into()];
        news.events = vec![message("$2", "@feed:x")];
        let mut party = room("!party:x", "Party");
        party.membership = Membership::Invited;
        db.observe(Synced {
            requested: FRESH,
            next_batch: "next".into(),
            packs: None,
            rooms: vec![work, team, bob, news, party],
        })
        .await
        .unwrap();
        let matching = |query: &str| -> Vec<String> {
            let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
            let tx = conn.transaction().unwrap();
            let query = uplink_query::parse(query, 0).unwrap();
            let mut names: Vec<String> = crate::query::Rooms::of(&db.rooms, &tx)
                .unwrap()
                .matching(&tx, &query)
                .unwrap()
                .into_iter()
                .map(|g| g.room.name)
                .collect();
            names.sort();
            names
        };
        assert_eq!(matching("is:unread"), ["News", "Team chat"]);
        assert_eq!(matching("is:unread -tag:lowpriority"), ["Team chat"]);
        assert_eq!(matching(r#"tag:"Low priority""#), ["News"]);
        assert_eq!(matching("tag:projects"), ["Team chat"]);
        assert_eq!(matching("space:work"), ["Team chat"]);
        assert_eq!(matching("is:dm"), ["Bob"]);
        assert_eq!(matching("is:invite"), ["Party"]);
        assert_eq!(matching("tea"), ["Team chat"], "by name, not spaces");
        assert_eq!(matching("account:@me:example.org").len(), 4);
        // A room has the terms of its messages, of one of them.
        assert_eq!(matching("from:bob"), ["Team chat"]);
        assert_eq!(matching("is:read").len(), 0, "no message is read");
        // Terms of other kinds are false of rooms.
        assert!(matching("is:todo OR in:inbox").is_empty());
        assert_eq!(matching("-in:inbox").len(), 4);

        let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        let known = |name| crate::query::tag_known(&tx, name).unwrap();
        assert!(known("favourite") && known("low_priority") && known("Projects"));
        assert!(!known("inbox"));
        // Reading a room marks it read up to its latest event.
        let rooms = crate::rooms::rooms(&tx, &account(), None).unwrap();
        let latest = |name: &str| rooms.iter().find(|r| r.name == name).unwrap().latest;
        assert!(latest("Team chat").is_some());
        assert_eq!(latest("Bob"), None);
    }

    /// How fast home's buckets are of rooms with one as large and busy as
    /// a public community's (Matrix HQ): each bucket's window lists anew
    /// on each change, with rooms of its own (as `Items::read_page` has
    /// them), its query less those before it, by `-has:(…)` as home has
    /// them or by `-(…)` as it had them. Slow to set up, so run on demand:
    /// `cargo test --release -p uplink-matrix -- --ignored --nocapture speed`.
    #[tokio::test]
    #[ignore]
    async fn speed_of_home_with_a_large_room() {
        use std::time::Instant;
        const MESSAGES: i64 = 500_000;

        let db = Db::new();
        let mut hq = room("!hq:x", "HQ");
        hq.events = vec![said("$seed", "@a:x", "hello", 1)];
        let mut dm = room("!dm:x", "Dm");
        dm.direct = true;
        dm.events = vec![said("$d", "@b:x", "hi", 2)];
        let mut small = room("!small:x", "Small");
        small.events = vec![said("$s", "@c:x", "hey", 3)];
        db.observe(Synced {
            requested: FRESH,
            next_batch: "next".into(),
            packs: None,
            rooms: vec![hq, dm, small],
        })
        .await
        .unwrap();
        let path = db._dir.path().join("db").join("uplink.sqlite");
        let writer = uplink_storage::rusqlite::Connection::open(&path).unwrap();
        let started = Instant::now();
        // From many senders; one in a hundred notifies, one in ten
        // thousand mentions.
        writer
            .execute(
                "INSERT INTO matrix_event (room, event_id, sender, ts, type, msgtype, body,
                                           raw_json, encryption, pos, notify, highlight)
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
                 SELECT r.id, '$hq' || i, '@u' || (i % 500) || ':x', 1000 + i,
                        'm.room.message', 'm.text', 'message ' || i, '{}', 'none',
                        (SELECT max(pos) FROM matrix_event) + i * 16,
                        i % 100 = 0, i % 10000 = 0
                 FROM n, matrix_room r WHERE r.room_id = '!hq:x'",
                [MESSAGES],
            )
            .unwrap();
        writer.execute_batch("ANALYZE").unwrap();
        eprintln!("set up {MESSAGES} messages in {:?}", started.elapsed());

        let home = [
            "(in:inbox is:unread is:important to:me) OR (is:mention is:new) \
         OR (is:dm is:unread is:notifying) OR is:invite",
            "(in:inbox is:unread is:important) OR (is:unread is:notifying) \
         OR is:overdue OR due:today",
            "(in:inbox is:unread) OR (is:unread tag:favourite)",
            "is:starred OR is:todo",
            "in:inbox",
            "is:unread -tag:lowpriority",
        ];
        // Of HQ, how many are unread: none, some, many, and all (joined,
        // never read).
        for unread in [0, 1_000, 50_000, MESSAGES] {
            let marker = match unread {
                u if u == MESSAGES => "NULL".to_owned(),
                u => format!(
                    "(SELECT id FROM matrix_event WHERE event_id = '$hq{}')",
                    MESSAGES - u
                ),
            };
            writer
                .execute_batch(&format!(
                    "UPDATE matrix_room SET read_up_to = {marker}, fully_read = {marker}
                     WHERE room_id = '!hq:x'"
                ))
                .unwrap();
            // Written past the writer: listed anew.
            let view = crate::room_view::RoomsView::kept(&db.dispatcher, vec![account()]).unwrap();
            for negation in ["-has:", "-"] {
                let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
                let tx = conn.transaction().unwrap();
                let started = Instant::now();
                let mut listed = vec![];
                for at in 0..home.len() {
                    let text = std::iter::once(format!("({})", home[at]))
                        .chain(home[..at].iter().map(|q| format!("{negation}({q})")))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let query = uplink_query::parse(&text, 0).unwrap();
                    let rooms = crate::query::Rooms::of(&view, &tx).unwrap();
                    let names: Vec<_> = rooms
                        .matching(&tx, &query)
                        .unwrap()
                        .into_iter()
                        .map(|g| g.room.name)
                        .collect();
                    listed.push(names);
                }
                eprintln!(
                    "{unread} unread, {negation}(…): {:?} {listed:?}",
                    started.elapsed()
                );
            }
        }
    }

    fn said(id: &str, sender: &str, body: &str, ts: i64) -> ObservedEvent {
        let raw = serde_json::json!({
            "type": "m.room.message",
            "event_id": id,
            "sender": sender,
            "origin_server_ts": ts,
            "content": { "msgtype": "m.text", "body": body },
        });
        ObservedEvent::parse(&raw.to_string(), None, Encryption::None).unwrap()
    }

    #[tokio::test]
    async fn queries_match_messages_by_text_sender_and_room() {
        let db = Db::new();
        let mut work = room("!work:x", "Work");
        work.space = true;
        work.children = vec![crate::rooms::SpaceChild {
            room_id: "!team:x".into(),
            order: None,
        }];
        let mut team = room("!team:x", "Team chat");
        let mut mention = said("$3", "@bob:x", "Ping @me about the deploy", 3_000);
        mention.push.highlight = true;
        let edited = said("$5", "@carol:x", "Lunch at noon?", 5_000);
        let mut edit = said("$6", "@carol:x", "* Deploy at noon?", 6_000);
        edit.raw = serde_json::json!({
            "type": "m.room.message",
            "event_id": "$6",
            "sender": "@carol:x",
            "origin_server_ts": 6_000,
            "content": {
                "msgtype": "m.text",
                "body": "* Deploy at noon?",
                "m.new_content": { "msgtype": "m.text", "body": "Deploy at noon?" },
                "m.relates_to": { "rel_type": "m.replace", "event_id": "$5" },
            },
        })
        .to_string();
        let edit = ObservedEvent::parse(&edit.raw, None, Encryption::None).unwrap();
        team.events = vec![
            said("$1", ME, "I will deploy today", 1_000),
            said("$2", "@bob:x", "Deploy is green", 2_000),
            mention,
            edited,
            edit,
        ];
        team.tags = vec!["u.Projects".into()];
        let mut bob = room("!bob:x", "Bob");
        bob.direct = true;
        bob.events = vec![said("$4", "@bob:x", "100% deployed", 4_000)];
        // Read up to $8, caught up on to $7.
        let mut ops = room("!ops:x", "Ops");
        let mut ping = said("$8", "@dave:x", "Ship now, @me", 8_000);
        ping.push.highlight = true;
        ops.events = vec![
            said("$7", "@dave:x", "Ship it", 7_000),
            ping,
            said("$9", "@dave:x", "Shipped, ops", 9_000),
        ];
        ops.own_receipts = vec!["$8".into()];
        ops.fully_read = Some("$7".into());
        db.observe(Synced {
            requested: FRESH,
            next_batch: "next".into(),
            packs: None,
            rooms: vec![work, team, bob, ops],
        })
        .await
        .unwrap();
        let matching = |query: &str| -> Vec<String> {
            let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
            let tx = conn.transaction().unwrap();
            let query = uplink_query::parse(query, 0).unwrap();
            // The hits of the rooms found by their messages, as each
            // room's own search finds them.
            let rooms = crate::query::Rooms::of(&db.rooms, &tx).unwrap();
            let mut found: Vec<_> = rooms
                .matching(&tx, &query)
                .unwrap()
                .into_iter()
                .filter(|g| g.hits.is_some())
                .flat_map(|g| {
                    let (events, more) = rooms.search_room(&tx, &g.room, &query, None, 10).unwrap();
                    assert!(!more);
                    assert_eq!(g.hits.unwrap().count as usize, events.len());
                    events
                })
                .collect();
            found.sort_by_key(|e| std::cmp::Reverse(e.ts));
            found.into_iter().map(|e| e.event_id).collect()
        };
        // The latest first; an edit's text is its message's.
        assert_eq!(matching("deploy"), ["$5", "$4", "$3", "$2", "$1"]);
        assert_eq!(matching("deploy is:unread"), ["$5", "$4", "$3", "$2"]);
        assert_eq!(matching("deploy from:me"), ["$1"]);
        assert_eq!(matching("from:bob"), ["$4", "$3", "$2"]);
        assert_eq!(matching("deploy is:dm"), ["$4"]);
        assert_eq!(matching("deploy -space:work"), ["$4"]);
        assert_eq!(matching("tag:projects green"), ["$2"]);
        assert_eq!(matching("100%"), ["$4"], "% is no wildcard");
        assert_eq!(matching("deploy before:1970-01-01"), Vec::<String>::new());
        // Terms of other kinds are false of messages.
        assert!(matching("deploy is:todo").is_empty());
        assert!(matching("in:inbox OR subject:deploy").is_empty());
        assert_eq!(matching("deploy is:room").len(), 5);
        // Unread after the read marker, new after the fully-read one.
        assert_eq!(matching("ship is:unread"), ["$9"]);
        assert_eq!(matching("ship is:new"), ["$9", "$8"]);
        assert_eq!(matching("ship (is:new OR is:unread)"), ["$9", "$8"]);
        assert_eq!(matching("ship -is:unread"), ["$8", "$7"]);
        // Alternatives bounded apart, each by its own bound or by none.
        assert_eq!(
            matching("(ship is:new) OR (deploy is:dm)"),
            ["$9", "$8", "$4"]
        );
        assert_eq!(
            matching("(ship is:unread) OR from:bob"),
            ["$9", "$4", "$3", "$2"]
        );
        assert_eq!(
            matching("(ship is:unread) OR (ship is:new) OR deploy is:dm"),
            ["$9", "$8", "$4"]
        );
        // A room its name matches is found as a room, not by its messages.
        assert_eq!(matching("ops"), Vec::<String>::new());

        // Of rooms: by their own terms, or by their messages, the latest
        // of which they show; the most recently active first.
        let groups = |query: &str| -> Vec<(String, Option<(u32, String)>)> {
            let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
            let tx = conn.transaction().unwrap();
            let query = uplink_query::parse(query, 0).unwrap();
            crate::query::Rooms::of(&db.rooms, &tx)
                .unwrap()
                .matching(&tx, &query)
                .unwrap()
                .into_iter()
                .map(|g| {
                    let hits = g.hits.map(|h| (h.count, h.latest.event_id));
                    (g.room.name, hits)
                })
                .collect()
        };
        let hits =
            |name: &str, count, latest: &str| (name.to_owned(), Some((count, latest.to_owned())));
        assert_eq!(
            groups("deploy"),
            [hits("Team chat", 4, "$5"), hits("Bob", 1, "$4")]
        );
        // Of what state its messages are in, alone, the room is what is
        // found, as of its own terms: nothing in particular is sought.
        let room = |name: &str| (name.to_owned(), None);
        assert_eq!(groups("is:mention"), [room("Ops"), room("Team chat")]);
        assert_eq!(
            groups("is:mention is:new"),
            [room("Ops"), room("Team chat")]
        );
        assert_eq!(groups("is:mention is:new is:unread"), [room("Team chat")]);
        // Of a time, they are sought.
        assert_eq!(
            groups("is:mention is:new after:1970-01-01"),
            [hits("Ops", 1, "$8"), hits("Team chat", 1, "$3")]
        );
        assert_eq!(groups("ops"), [room("Ops")]);
        assert_eq!(
            groups("is:dm OR from:dave"),
            [hits("Ops", 3, "$9"), ("Bob".to_owned(), None)]
        );
        // A negation is of a message: Team chat has an unread message
        // that is not a mention ($2) as well as one that is ($3). Of the
        // room, `has:(…)`: which a room is in, and so a bucket of home
        // after one of `is:unread is:mention` is not (SPEC §12.13).
        assert_eq!(
            groups("is:unread -(is:unread is:mention)"),
            [room("Ops"), room("Team chat"), room("Bob")]
        );
        assert_eq!(
            groups("is:unread -has:(is:unread is:mention)"),
            [room("Ops"), room("Bob")]
        );
        // Ops' mention is read, but is one.
        assert_eq!(groups("is:unread -has:(is:mention)"), [room("Bob")]);
        // What it holds is of the room, not sought among its messages.
        assert_eq!(groups("has:(from:bob)"), [room("Team chat"), room("Bob")]);
        assert_eq!(
            groups("has:(from:bob) from:carol"),
            [hits("Team chat", 1, "$5")]
        );
        assert_eq!(
            groups("has:(is:dm) OR has:(ops)"),
            [room("Ops"), room("Bob")]
        );
        assert_eq!(groups("-has:(deploy) -has:(ship)").len(), 0);

        // Each room's preview is its latest message, as last edited (the
        // edit is not one of its own); a room with none has none.
        let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        let rooms = crate::rooms::rooms(&tx, &account(), None).unwrap();
        // Its counts are of what the terms of its messages find.
        let ops = rooms.iter().find(|r| r.name == "Ops").unwrap();
        assert_eq!((ops.unread, ops.mentions, ops.highlighted), (1, 1, 0));
        let preview = |name: &str| {
            let room = rooms.iter().find(|r| r.name == name).unwrap();
            room.preview
                .as_ref()
                .map(|p| (p.sender.clone(), p.body.clone()))
        };
        assert_eq!(
            preview("Team chat"),
            Some(("@carol:x".into(), Some("Deploy at noon?".into())))
        );
        assert_eq!(
            preview("Bob"),
            Some(("@bob:x".into(), Some("100% deployed".into())))
        );
        assert_eq!(preview("Work"), None);
    }
}

/// Undone once sent, a message is taken back: redacted; undone again, it
/// is sent anew.
#[tokio::test]
async fn sent_messages_undone_are_taken_back() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    let sent = db.sent(text("hi"), Some("$hi")).await;
    let cancel = db.dispatcher.undo(sent).await.unwrap();
    assert_eq!(db.description(cancel), "Took back 'hi'");
    let item = db.sends().await[0].id;
    assert!(db.sends().await[0].retractable);
    assert_eq!(
        db.take_backs(),
        [(
            item,
            MatrixContent::Redaction {
                event: "$hi".into()
            }
        )]
    );
    db.observe(TakeBackStarted { item }).await.unwrap();
    assert_eq!(db.shown().await, [(MatrixSendState::TakingBack, false)]);
    // Interrupted, or failing for a passing reason: tried again.
    db.observe(TakeBackFailed {
        item,
        refused: None,
    })
    .await
    .unwrap();
    assert_eq!(db.take_backs().len(), 1);
    db.observe(TakeBackStarted { item }).await.unwrap();
    db.observe(TakeBackSucceeded { item }).await.unwrap();
    assert!(db.sends().await.is_empty());
    assert!(db.outbox().is_empty());
    assert!(db.take_backs().is_empty());

    // Redone, it is sent anew, as a send of its own.
    let again = db.dispatcher.undo(cancel).await.unwrap();
    assert_eq!(db.description(again), "Sent 'hi' again");
    let [resent] = &db.sends().await[..] else {
        panic!()
    };
    assert_ne!(resent.id, item);
    assert_eq!(
        (resent.state, resent.wanted),
        (MatrixSendState::Queued, true)
    );
    // Whose undo cancels it, as any.
    db.dispatcher.undo(again).await.unwrap();
    assert!(db.sends().await.is_empty());
}

/// Redone while being taken back, it is sent anew, the take-back going on.
#[tokio::test]
async fn sends_redone_while_taken_back_are_sent_anew() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    let sent = db.sent(text("hi"), Some("$hi")).await;
    let cancel = db.dispatcher.undo(sent).await.unwrap();
    let item = db.sends().await[0].id;
    db.observe(TakeBackStarted { item }).await.unwrap();
    db.dispatcher.undo(cancel).await.unwrap();
    assert_eq!(
        db.shown().await,
        [
            (MatrixSendState::TakingBack, false),
            (MatrixSendState::Queued, true)
        ]
    );
    db.observe(TakeBackSucceeded { item }).await.unwrap();
    assert_eq!(db.shown().await, [(MatrixSendState::Queued, true)]);
}

/// A take-back the homeserver refuses needs a decision: keeping it sent.
#[tokio::test]
async fn take_backs_refused_need_a_decision() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    let sent = db.sent(text("hi"), Some("$hi")).await;
    db.dispatcher.undo(sent).await.unwrap();
    let item = db.sends().await[0].id;
    db.observe(TakeBackStarted { item }).await.unwrap();
    db.observe(TakeBackFailed {
        item,
        refused: Some("M_FORBIDDEN".into()),
    })
    .await
    .unwrap();
    let [refused] = &db.sends().await[..] else {
        panic!()
    };
    assert_eq!(
        (refused.state, refused.wanted, refused.error.as_deref()),
        (MatrixSendState::Sent, false, Some("M_FORBIDDEN"))
    );
    assert!(db.take_backs().is_empty());
    db.matrix.keep_sent(item).await.unwrap();
    assert!(db.sends().await.is_empty());
}

/// Cancelled while being sent, a message that went out is taken back.
#[tokio::test]
async fn sends_cancelled_while_sent_are_taken_back_once_out() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    let sent = db.send("hi").await;
    let item = start(&db).await;
    db.dispatcher.undo(sent).await.unwrap();
    db.observe(SendSucceeded {
        item,
        event_id: Some("$hi".into()),
    })
    .await
    .unwrap();
    assert_eq!(
        db.take_backs(),
        [(
            item,
            MatrixContent::Redaction {
                event: "$hi".into()
            }
        )]
    );
}

/// A pin is taken back by unpinning; one that changed nothing needs
/// nothing.
#[tokio::test]
async fn pins_are_taken_back_by_reversing_them() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    let pin = MatrixContent::Pin {
        event: "$1".into(),
        pinned: true,
    };
    let pinned = db.sent(pin.clone(), Some("$pins")).await;
    db.dispatcher.undo(pinned).await.unwrap();
    let item = db.sends().await[0].id;
    assert_eq!(
        db.take_backs(),
        [(
            item,
            MatrixContent::Pin {
                event: "$1".into(),
                pinned: false
            }
        )]
    );

    db.observe(TakeBackStarted { item }).await.unwrap();
    db.observe(TakeBackSucceeded { item }).await.unwrap();

    let unchanged = db.sent(pin, None).await;
    let cancel = db.dispatcher.undo(unchanged).await.unwrap();
    assert!(db.sends().await.is_empty());
    assert!(db.take_backs().is_empty());
    // Redone, it is sent anew.
    db.dispatcher.undo(cancel).await.unwrap();
    assert_eq!(db.shown().await, [(MatrixSendState::Queued, true)]);
}

/// What Matrix can't take back, or uplink does not yet, needs a decision
/// once sent.
#[tokio::test]
async fn sends_not_retractable_need_a_decision() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    let named = db
        .sent(
            MatrixContent::Name {
                name: "Lounge".into(),
            },
            Some("$name"),
        )
        .await;
    let cancel = db.dispatcher.undo(named).await.unwrap();
    assert_eq!(
        db.description(cancel),
        "Cancelled sending the name 'Lounge', which went out"
    );
    let [send] = &db.sends().await[..] else {
        panic!()
    };
    assert_eq!(
        (send.state, send.wanted, send.retractable),
        (MatrixSendState::Sent, false, false)
    );
    assert!(db.take_backs().is_empty());
    db.dispatcher.undo(cancel).await.unwrap();
    assert!(db.sends().await.is_empty());
}

/// Listing the rooms works through each room's messages by its indexes
/// (SPEC §9.5): as much however long its history, if read, and as
/// many times more as there are rooms, not more.
#[tokio::test]
async fn listing_rooms_costs_as_the_rooms_not_their_history() {
    use uplink_storage::work::Work;

    // `rooms` rooms of `messages` messages each from others, every
    // other room read to its latest, the others with two unread.
    async fn work_of(rooms: usize, messages: usize) -> u64 {
        let db = Db::new();
        let synced = (0..rooms)
            .map(|r| {
                let events: Vec<ObservedEvent> = (0..messages)
                    .map(|m| {
                        let raw = json!({
                            "type": "m.room.message",
                            "event_id": format!("$e{r}-{m}"),
                            "sender": "@bob:example.org",
                            "origin_server_ts": 1_700_000_000_000_i64 + (m * rooms + r) as i64,
                            "content": { "msgtype": "m.text", "body": "hi" },
                        });
                        ObservedEvent::parse(&raw.to_string(), None, Encryption::None).unwrap()
                    })
                    .collect();
                let read = messages - if r % 2 == 0 { 1 } else { 3 };
                ObservedRoom {
                    own_receipts: vec![format!("$e{r}-{read}")],
                    fully_read: Some(format!("$e{r}-{read}")),
                    ..joined(&format!("!r{r}:example.org"), events)
                }
            })
            .collect();
        db.observe(Synced {
            requested: FRESH,
            next_batch: "next".into(),
            packs: None,
            rooms: synced,
        })
        .await
        .unwrap();
        // With the statistics a database of them has (SPEC §4.2).
        uplink_storage::open(db._dir.path()).unwrap();
        let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        let work = Work::default();
        work.count_on(&conn);
        let tx = conn.transaction().unwrap();
        let (steps, listed) = work.of(|| crate::rooms::rooms(&tx, &account(), None).unwrap());
        assert_eq!(listed.len(), rooms);
        steps
    }
    let base = work_of(20, 20).await;
    let (longer, more) = (work_of(20, 80).await, work_of(80, 20).await);
    assert!(
        (longer as f64) < 1.5 * base as f64,
        "of 20 rooms of 20 messages {base} steps, of 80 messages {longer}"
    );
    assert!(
        (more as f64) < 5.0 * base as f64,
        "of 20 rooms of 20 messages {base} steps, of 80 rooms {more}"
    );
}
