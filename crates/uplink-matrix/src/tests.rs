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
use uplink_tx_log::{
    reconciler::{Claimed, Push, Pushed},
    Dispatcher, Observation, Registry,
};

const ME: &str = "@me:example.org";
const ROOM: &str = "!room:example.org";

struct Db {
    _dir: tempfile::TempDir,
    dispatcher: Dispatcher,
    rooms: Arc<crate::room_view::RoomsView>,
    matrix: Matrix,
    /// Whether the homeserver keeps thread subscriptions, as syncs say.
    follows: bool,
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
            follows: false,
        }
    }

    /// Of a homeserver that keeps thread subscriptions (MSC4306).
    fn following() -> Self {
        Self {
            follows: true,
            ..Self::new()
        }
    }

    async fn observe(&self, observation: impl Observation) -> anyhow::Result<TxLogId> {
        self.dispatcher.observe("mx".into(), observation).await
    }

    /// The position a request sent now is stamped with (SPEC §6.6.2).
    fn as_of(&self) -> TxLogId {
        let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
        uplink_tx_log::as_of(&conn).unwrap()
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
            next_batch: "next".into(),
            follows: self.follows,
            packs: None,
            as_of: self.as_of(),
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
        invitation: None,
        space: false,
        direct: false,
        avatar: None,
        bridge: None,
        children: vec![],
        space_order: None,
        tags: None,
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

    // Pinning one not stored, or not an event id, is refused; unpinning
    // any is not.
    let pin = |event: &str| db.matrix.set_pinned(room.id, event.into(), true);
    assert!(pin("$gone").await.is_err());
    assert!(pin("nope").await.is_err());
    pin("$1").await.unwrap();
    db.matrix
        .set_pinned(room.id, "$gone".into(), false)
        .await
        .unwrap();
    assert_eq!(db.room().await.pinned, ["$2", "$1"]);
}

mod pins {
    use super::*;
    use crate::{
        pins::Pins,
        reconciler::{Kind, Read, Rooms},
    };
    use uplink_protocol::matrix::PendingPins;
    use uplink_tx_log::reconcile::{Ended, Outcome};

    impl Db {
        async fn pin(&self, event: &str, pinned: bool) -> TxLogId {
            let room = self.room().await.id;
            match self
                .matrix
                .set_pinned(room, event.into(), pinned)
                .await
                .unwrap()
            {
                ServerResponse::Dispatched { tx_log_id } => tx_log_id,
                other => panic!("unexpected {other:?}"),
            }
        }

        fn conn(&self) -> uplink_storage::rusqlite::Connection {
            uplink_storage::open_reader(self._dir.path()).unwrap()
        }

        /// The reconciler claims the pins wanted, and the requests it
        /// would send: the room's list, and the events it pushes.
        async fn claim_pins(&self) -> Vec<(Vec<String>, Vec<String>)> {
            let wanted: Vec<_> = Pins::reconciling(&self.conn(), "mx")
                .unwrap()
                .into_iter()
                .filter(|p| p.field.wanted().is_some())
                .map(|p| (p.room, p.key))
                .collect();
            self.observe(Claimed::<Rooms<Pins>>::new(wanted))
                .await
                .unwrap();
            let conn = self.conn();
            let in_flight = Pins::reconciling(&conn, "mx")
                .unwrap()
                .into_iter()
                .filter(|p| {
                    p.field
                        .attempt
                        .as_ref()
                        .is_some_and(|a| a.outcome == Outcome::InFlight)
                })
                .collect();
            Pins::requests(&conn, in_flight)
                .unwrap()
                .into_iter()
                .map(|(list, fields)| (list, fields.into_iter().map(|f| f.key).collect()))
                .collect()
        }

        /// The pushes of `events`, claimed, ended so.
        async fn pushed(&self, events: &[String], ended: Ended) {
            let room = self.room().await.id;
            for event in events {
                self.observe(Pushed::<Rooms<Pins>> {
                    pushes: vec![Push {
                        fields: vec![(room, event.clone())],
                        ended: ended.clone(),
                        rekeyed: None,
                    }],
                })
                .await
                .unwrap();
            }
        }

        /// The room's pins, read back from the homeserver.
        async fn read_pins(&self, pinned: &[&str]) {
            let room = self.room().await.id;
            self.observe(Read::<Pins> {
                room,
                read: pinned.iter().map(|p| p.to_string()).collect(),
                as_of: self.as_of(),
            })
            .await
            .unwrap();
        }

        fn pending_pins(&self) -> Vec<PendingPins> {
            match self.outbox().into_iter().next().map(|a| a.pending) {
                Some(Pending::Matrix { pins, .. }) => pins,
                None => vec![],
                other => panic!("{other:?}"),
            }
        }
    }

    fn strings(events: &[&str]) -> Vec<String> {
        events.iter().map(|e| e.to_string()).collect()
    }

    /// Each write of the room's list is of every pin as uplink last
    /// pushed it, else as seen: a pin pushed, not yet seen, is not lost to
    /// the next, as it was when the list was built from the SDK's store.
    #[tokio::test]
    async fn pins_are_written_whole_from_uplinks_own_state() {
        let db = Db::new();
        db.sync(&[("$1", "@bob:x"), ("$2", "@bob:x"), ("$3", "@bob:x")], &[])
            .await;
        db.sync_room(vec![], &[], &["$1"], None).await;

        let pinned_2 = db.pin("$2", true).await;
        assert_eq!(db.room().await.pinned, ["$1", "$2"]);
        let requests = db.claim_pins().await;
        assert_eq!(requests, [(strings(&["$1", "$2"]), strings(&["$2"]))]);
        db.pushed(&strings(&["$2"]), Ended::Applied).await;

        // Pinned before $2 is seen pinned: the list keeps it.
        db.pin("$3", true).await;
        assert_eq!(db.room().await.pinned, ["$1", "$2", "$3"]);
        let requests = db.claim_pins().await;
        assert_eq!(requests, [(strings(&["$1", "$2", "$3"]), strings(&["$3"]))]);
        db.pushed(&strings(&["$3"]), Ended::Applied).await;
        assert_eq!(db.pending_pins().len(), 2);

        // Read back, they are done.
        db.read_pins(&["$1", "$2", "$3"]).await;
        assert!(db.pending_pins().is_empty());
        assert!(Pins::reconciling(&db.conn(), "mx").unwrap().is_empty());

        // Undone, $2 is unpinned; the others stay, in their order.
        db.dispatcher.undo(pinned_2).await.unwrap();
        assert_eq!(db.room().await.pinned, ["$1", "$3"]);
        let requests = db.claim_pins().await;
        assert_eq!(requests, [(strings(&["$1", "$3"]), strings(&["$2"]))]);
    }

    /// The homeserver's order is kept: another client's reordering shows,
    /// and what uplink pins goes after.
    #[tokio::test]
    async fn pins_keep_the_homeservers_order() {
        let db = Db::new();
        db.sync(&[("$1", "@bob:x"), ("$2", "@bob:x"), ("$3", "@bob:x")], &[])
            .await;
        db.sync_room(vec![], &[], &["$1", "$2"], None).await;
        db.sync_room(vec![], &[], &["$2", "$1"], None).await;
        assert_eq!(db.room().await.pinned, ["$2", "$1"]);
        db.pin("$3", true).await;
        assert_eq!(db.room().await.pinned, ["$2", "$1", "$3"]);
        assert_eq!(
            db.claim_pins().await,
            [(strings(&["$2", "$1", "$3"]), strings(&["$3"]))]
        );
        // Unpinned elsewhere meanwhile: gone from the list too.
        db.sync_room(vec![], &[], &["$2"], None).await;
        db.pushed(&strings(&["$3"]), Ended::Applied).await;
        db.read_pins(&["$2", "$3"]).await;
        assert_eq!(db.room().await.pinned, ["$2", "$3"]);
    }

    /// A pin the homeserver refuses waits for a decision, in the outbox,
    /// which undoing it makes.
    #[tokio::test]
    async fn a_refused_pin_waits_for_a_decision() {
        let db = Db::new();
        db.sync(&[("$1", "@bob:x")], &[]).await;
        let pinned = db.pin("$1", true).await;
        db.claim_pins().await;
        db.pushed(
            &strings(&["$1"]),
            Ended::NotApplied {
                refused: Some("not a moderator".into()),
            },
        )
        .await;
        let [pending] = &db.pending_pins()[..] else {
            panic!("{:?}", db.pending_pins())
        };
        assert_eq!(pending.op.id, pinned);
        assert_eq!(pending.refused.as_deref(), Some("not a moderator"));
        assert!(db.claim_pins().await.is_empty(), "not pushed again");
        db.dispatcher.undo(pinned).await.unwrap();
        assert!(db.pending_pins().is_empty());
        assert!(db.room().await.pinned.is_empty());
    }
}

/// What is unseen of a room is each message the query matches as a
/// message, sent once uplink began to sync the account, and stored after
/// the room was last seen: by Home, or by itself.
#[tokio::test]
async fn unseen_messages_match_as_messages_and_are_news() {
    let db = Db::news_since(2000);
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
    db.sync_events(
        vec![
            pushed("$4", ME, true, 3000),
            pushed("$2", "@bob:x", true, 3000),
            pushed("$3", "@bob:x", false, 3000),
            // Sent before uplink began to sync: history, stored only now.
            pushed("$5", "@bob:x", true, 1000),
        ],
        &[],
    )
    .await;
    let room = db.room().await.id;
    let unseen = |query: &str, home: RoomEventId, own: Option<RoomEventId>| {
        let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        let rooms = crate::query::Rooms::of(&db.rooms, &tx).unwrap();
        let query = uplink_query::parse(query, 0).unwrap();
        let own: std::collections::HashMap<_, _> = own.map(|own| (room, own)).into_iter().collect();
        rooms
            .unseen(&tx, &query, home, &own, 10)
            .unwrap()
            .into_iter()
            .map(|(_, event)| event.event_id)
            .collect::<Vec<_>>()
    };
    let none = RoomEventId(0);
    // Of the messages that notify, Bob's since the first sync (which
    // notifies of none, §9.5), of the unread, those after it, the newest
    // first; neither of the account's own. Of a room its own terms match,
    // each, its own too: but of history.
    assert_eq!(unseen("is:unread is:notifying", none, None), ["$2"]);
    assert_eq!(unseen("is:unread", none, None), ["$3", "$2"]);
    assert_eq!(unseen("is:room", none, None), ["$3", "$2", "$4", "$1"]);
    // Seen in Home as of $1, or in the room as of $2.
    let (first, second) = (db.event("$1").await, db.event("$2").await);
    assert_eq!(unseen("is:room", first, None), ["$3", "$2", "$4"]);
    assert_eq!(unseen("is:room", first, Some(second)), ["$3"]);
    // Read: no longer unread.
    let read = db.event("$3").await;
    db.matrix.mark_read(room, read, false, false).await.unwrap();
    assert!(unseen("is:unread", none, None).is_empty());
    assert_eq!(unseen("is:room", first, None), ["$3", "$2", "$4"]);
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
    use crate::{
        membership::Memberships,
        reconciler::{Kind, Read, Rooms},
    };
    use uplink_protocol::{matrix::MembershipChange, ErrorCode};
    use uplink_tx_log::reconcile::Ended;

    impl Db {
        /// Syncs the room with `membership`, and no events.
        async fn sync_membership(&self, membership: Membership) {
            self.observe(Synced {
                next_batch: "next".into(),
                follows: false,
                packs: None,
                as_of: self.as_of(),
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
                    invitation: (membership == Membership::Invited).then(|| {
                        crate::rooms::ObservedInvitation {
                            inviter: "@bob:x".into(),
                            inviter_name: None,
                        }
                    }),
                    space: false,
                    direct: false,
                    avatar: None,
                    bridge: None,
                    children: vec![],
                    space_order: None,
                    tags: None,
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

        /// The sync task pushes the membership asked for, which ends so.
        async fn push_membership(&self, ended: Ended) {
            let room = self.room().await.id;
            self.observe(Claimed::<Rooms<Memberships>>::new(vec![(
                room,
                String::new(),
            )]))
            .await
            .unwrap();
            self.observe(Pushed::<Rooms<Memberships>> {
                pushes: vec![Push {
                    fields: vec![(room, String::new())],
                    ended,
                    rekeyed: None,
                }],
            })
            .await
            .unwrap();
        }

        /// The sync task reads back the room's membership.
        async fn read_membership(&self, read: Membership) {
            let room = self.room().await.id;
            let as_of = self.as_of();
            self.observe(Read::<Memberships> { room, read, as_of })
                .await
                .unwrap();
        }

        fn reconciling(&self) -> Vec<crate::reconciler::Stored<Membership>> {
            let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
            Memberships::reconciling(&conn, "mx").unwrap()
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
        db.push_membership(Ended::Applied).await;
        assert_eq!(
            db.state().await,
            (Membership::Invited, Some(MembershipChange::Join)),
            "until confirmed"
        );
        db.read_membership(Membership::Joined).await;
        assert_eq!(db.state().await, (Membership::Joined, None));
        let leave = db.dispatcher.undo(join).await.unwrap();
        assert_eq!(
            db.state().await,
            (Membership::Joined, Some(MembershipChange::Leave))
        );
        // Left: the room is listed no more, unless joining it again.
        db.push_membership(Ended::Applied).await;
        db.read_membership(Membership::Left).await;
        let ServerResponse::Rooms { rooms, .. } = db.matrix.list_rooms().await.unwrap() else {
            panic!()
        };
        assert!(rooms.is_empty());
        db.dispatcher.undo(leave).await.unwrap();
        assert_eq!(
            db.state().await,
            (Membership::Left, Some(MembershipChange::Join))
        );
        // A sync settles it as well as a read back.
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

    /// Undoing a join while it is pushed: it may land, so the room is
    /// left after it, rather than kept invited.
    #[tokio::test]
    async fn undoing_a_join_in_flight_leaves_after_it() {
        let db = Db::new();
        db.sync_membership(Membership::Invited).await;
        let join = db.change(MembershipChange::Join).await;
        let room = db.room().await.id;
        db.observe(Claimed::<Rooms<Memberships>>::new(vec![(
            room,
            String::new(),
        )]))
        .await
        .unwrap();
        db.dispatcher.undo(join).await.unwrap();
        assert_eq!(
            db.state().await,
            (Membership::Invited, Some(MembershipChange::Leave))
        );
        db.observe(Pushed::<Rooms<Memberships>> {
            pushes: vec![Push {
                fields: vec![(room, String::new())],
                ended: Ended::Applied,
                rekeyed: None,
            }],
        })
        .await
        .unwrap();
        let wanted: Vec<_> = db
            .reconciling()
            .iter()
            .filter_map(|r| r.field.wanted().copied())
            .collect();
        assert_eq!(wanted, [Membership::Left]);
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
        db.push_membership(Ended::NotApplied {
            refused: Some("M_FORBIDDEN".into()),
        })
        .await;
        let room = db.room().await;
        assert_eq!(
            (room.membership, room.pending),
            (Membership::Invited, Some(MembershipChange::Join))
        );
        assert_eq!(room.membership_error.as_deref(), Some("M_FORBIDDEN"));
        assert_eq!(memberships(&db)[0].refused.as_deref(), Some("M_FORBIDDEN"));
        assert!(db.reconciling().iter().all(|r| r.field.wanted().is_none()));
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
        db.push_membership(Ended::NotApplied {
            refused: Some("M_FORBIDDEN".into()),
        })
        .await;
        db.sync_membership(Membership::Joined).await;
        let room = db.room().await;
        assert_eq!((room.pending, room.membership_error), (None, None));
        assert!(memberships(&db).is_empty());
    }
}

mod room_tags {
    use super::*;
    use crate::{
        reconciler::{Kind, Read, Rooms, Stored},
        room_tags::Tags,
    };
    use uplink_protocol::{
        matrix::{PendingRoomTags, RoomTagChange},
        ErrorCode, ServerError,
    };
    use uplink_tx_log::reconcile::Ended;

    const FAV: &str = "m.favourite";
    const LOW: &str = "m.lowpriority";

    impl Db {
        /// Syncs the room, its tags as the homeserver has them.
        async fn sync_tags(&self, tags: &[&str]) -> TxLogId {
            self.sync_tags_as_of(tags, self.as_of()).await
        }

        /// Syncs the room's tags, by a request sent after `as_of`.
        async fn sync_tags_as_of(&self, tags: &[&str], as_of: TxLogId) -> TxLogId {
            self.observe(Synced {
                next_batch: "next".into(),
                follows: false,
                packs: None,
                as_of,
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
                    invitation: None,
                    space: false,
                    direct: false,
                    avatar: None,
                    bridge: None,
                    children: vec![],
                    space_order: None,
                    tags: Some(tags.iter().map(|t| t.to_string()).collect()),
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

        /// The reconciler pushes the room's `tag`, which ends so.
        async fn push(&self, tag: &str, ended: Ended) {
            let room = self.room().await.id;
            self.observe(Claimed::<Rooms<Tags>>::new(vec![(room, tag.into())]))
                .await
                .unwrap();
            self.observe(Pushed::<Rooms<Tags>> {
                pushes: vec![Push {
                    fields: vec![(room, tag.into())],
                    ended,
                    rekeyed: None,
                }],
            })
            .await
            .unwrap();
        }

        fn stored(&self, tag: &str) -> Stored<bool> {
            let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
            let all = Tags::reconciling(&conn, "mx").unwrap();
            all.into_iter().find(|t| t.key == tag).unwrap()
        }

        async fn tags(&self) -> Vec<String> {
            self.room().await.tags
        }

        /// The tags still to set, `+tag` or `-tag`.
        fn wanted_tags(&self) -> Vec<String> {
            let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
            Tags::reconciling(&conn, "mx")
                .unwrap()
                .into_iter()
                .filter_map(|t| {
                    let sign = if *t.field.wanted()? { '+' } else { '-' };
                    Some(format!("{sign}{}", t.key))
                })
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

        db.push(FAV, Ended::Applied).await;
        assert!(db.wanted_tags().is_empty());
        assert!(db.stored(FAV).field.read_back(), "until confirmed");
        // The echo, or a read back, confirms it.
        db.sync_tags(&[FAV, "u.work"]).await;
        assert!(db.pending_tags().is_empty());
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
    /// it set one may tell of them as they were before: it does not undo
    /// them, but they are read back; read so, or synced after, they are
    /// the homeserver's, a tag asked for and pushed but not had waiting for
    /// the user to decide (SPEC §6.6.2, §6.6.3).
    #[tokio::test]
    async fn tags_told_of_as_before_they_were_set_are_read_back() {
        let db = Db::new();
        db.sync_tags(&[LOW]).await;
        let room = db.room().await.id;
        let tagged = db.tag(&[FAV], &[]).await.unwrap();
        let before = db.as_of();
        db.push(LOW, Ended::Applied).await;
        db.push(FAV, Ended::Applied).await;
        assert!(db.wanted_tags().is_empty());
        // Of between the two: not the homeserver's.
        db.sync_tags_as_of(&[], before).await;
        assert_eq!(db.tags().await, [FAV]);
        assert!(db.stored(FAV).field.read_back());
        // Read back, they are the homeserver's: another client's, and the
        // favourite not, though pushed: not kept.
        let as_of = db.as_of();
        db.observe(Read::<Tags> {
            room,
            read: vec!["u.other".into()],
            as_of,
        })
        .await
        .unwrap();
        assert_eq!(db.tags().await, [FAV, "u.other"]);
        assert!(db.wanted_tags().is_empty(), "not pushed again");
        let [pending] = &db.pending_tags()[..] else {
            panic!()
        };
        assert_eq!(
            (pending.op.id, pending.refused.as_deref()),
            (tagged, Some("the provider kept another value"))
        );
        // Undone, it is decided: not a favourite, and of low priority
        // again, as before it.
        db.dispatcher.undo(tagged).await.unwrap();
        assert_eq!(db.tags().await, [LOW, "u.other"]);
        assert_eq!(db.wanted_tags(), ["+m.lowpriority"]);
        db.push(LOW, Ended::Applied).await;
        db.sync_tags(&[LOW, "u.other"]).await;
        assert!(db.pending_tags().is_empty());
        // As is what a sync requested after tells of.
        db.sync_tags(&[LOW]).await;
        assert_eq!(db.tags().await, [LOW]);
    }

    /// The CI failure: a long poll answered before a push, recorded after
    /// its outcome, does not undo it; the push is read back.
    #[tokio::test]
    async fn a_sync_older_than_a_push_does_not_undo_it() {
        let db = Db::new();
        db.sync_tags(&[]).await;
        let stale = db.as_of();
        db.tag(&[FAV], &[]).await.unwrap();
        db.push(FAV, Ended::Applied).await;
        db.sync_tags_as_of(&[], stale).await;
        assert_eq!(db.tags().await, [FAV]);
        assert!(db.wanted_tags().is_empty(), "applied");
        assert!(db.stored(FAV).field.read_back());
        let room = db.room().await.id;
        let as_of = db.as_of();
        db.observe(Read::<Tags> {
            room,
            read: vec![FAV.into()],
            as_of,
        })
        .await
        .unwrap();
        assert!(db.pending_tags().is_empty());
        assert_eq!(db.tags().await, [FAV]);
    }

    /// Undoing while the push is in flight: the undo is pushed after it,
    /// though it asks for what was observed.
    #[tokio::test]
    async fn an_undo_during_a_push_is_pushed_after_it() {
        let db = Db::new();
        db.sync_tags(&[]).await;
        let tagged = db.tag(&["u.a"], &[]).await.unwrap();
        let room = db.room().await.id;
        db.observe(Claimed::<Rooms<Tags>>::new(vec![(room, "u.a".into())]))
            .await
            .unwrap();
        db.dispatcher.undo(tagged).await.unwrap();
        assert!(db.tags().await.is_empty());
        assert!(!db.pending_tags().is_empty(), "the push may land");
        assert!(db.wanted_tags().is_empty(), "one push at a time");
        db.observe(Pushed::<Rooms<Tags>> {
            pushes: vec![Push {
                fields: vec![(room, "u.a".into())],
                ended: Ended::Applied,
                rekeyed: None,
            }],
        })
        .await
        .unwrap();
        assert_eq!(db.wanted_tags(), ["-u.a"]);
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
        let refused = Some("M_FORBIDDEN".into());
        db.push("u.x", Ended::NotApplied { refused }).await;
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
    use crate::{
        reconciler::{Kind, Rooms},
        space_order::Orders,
    };
    use uplink_protocol::{
        matrix::{PendingSpaceOrders, SpaceOrderChange},
        ErrorCode, ServerError,
    };
    use uplink_tx_log::reconcile::Ended;

    impl Db {
        /// Syncs the room as a joined space, its order as the homeserver
        /// has it.
        async fn sync_space(&self, order: Option<&str>) {
            self.observe(Synced {
                next_batch: "next".into(),
                follows: false,
                packs: None,
                as_of: self.as_of(),
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
                    invitation: None,
                    space: true,
                    direct: false,
                    avatar: None,
                    bridge: None,
                    children: vec![],
                    space_order: Some(order.map(Into::into)),
                    tags: None,
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

        /// The reconciler claims the order wanted.
        async fn claim(&self) {
            let room = self.room().await.id;
            self.observe(Claimed::<Rooms<Orders>>::new(vec![(room, String::new())]))
                .await
                .unwrap();
        }

        /// The push claimed ended so.
        async fn ended(&self, ended: Ended) {
            let room = self.room().await.id;
            self.observe(Pushed::<Rooms<Orders>> {
                pushes: vec![Push {
                    fields: vec![(room, String::new())],
                    ended,
                    rekeyed: None,
                }],
            })
            .await
            .unwrap();
        }

        async fn push_order(&self, ended: Ended) {
            self.claim().await;
            self.ended(ended).await;
        }

        /// The orders still to set.
        fn wanted(&self) -> Vec<Option<String>> {
            let conn = uplink_storage::open_reader(self._dir.path()).unwrap();
            Orders::reconciling(&conn, "mx")
                .unwrap()
                .into_iter()
                .filter_map(|o| o.field.wanted().cloned())
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

        db.push_order(Ended::Applied).await;
        assert!(db.wanted().is_empty());
        assert_eq!(db.order().await.as_deref(), Some("a"));
        // The echo, or a read back, confirms it.
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
        db.push_order(Ended::Applied).await;
        db.dispatcher.undo(ordered).await.unwrap();
        assert_eq!((db.order().await, db.wanted()), (None, vec![None]));
        db.push_order(Ended::Applied).await;
        db.sync_space(None).await;
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
        db.claim().await;
        db.reorder(Some("c"), Some("d")).await.unwrap();
        db.ended(Ended::Applied).await;
        assert_eq!(db.wanted(), [Some("d".to_owned())]);
    }

    #[tokio::test]
    async fn refusals_wait_for_a_decision() {
        let db = Db::new();
        db.sync_space(Some("b")).await;
        let moved = db.reorder(Some("b"), Some("a")).await.unwrap();
        db.push_order(Ended::NotApplied {
            refused: Some("M_FORBIDDEN".into()),
        })
        .await;
        assert!(db.wanted().is_empty(), "not asked again");
        assert_eq!(
            db.pending_orders()[0].refused.as_deref(),
            Some("M_FORBIDDEN")
        );
        assert_eq!(db.order().await.as_deref(), Some("a"));
        // Undoing decides: the order as the homeserver has it, so nothing
        // is left to set.
        db.dispatcher.undo(moved).await.unwrap();
        assert!(db.wanted().is_empty());
        assert!(db.pending_orders().is_empty());
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
            invitation: None,
            space: false,
            direct: false,
            avatar: None,
            bridge: None,
            children: vec![],
            space_order: None,
            tags: None,
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
        small.tags = Some(vec!["u.Projects".into()]);
        small.created = Some(1_600_000_000_000);
        let mut large = room("!large:x", "Large");
        large.events = vec![message("$2", "@bob:x")];
        large.limited = true;
        large.joined_members = 3;
        let mut whole = room("!whole:x", "Whole");
        whole.events = vec![message("$3", "@bob:x")];
        db.observe(Synced {
            next_batch: "next".into(),
            follows: false,
            packs: None,
            as_of: db.as_of(),
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
            as_of: db.as_of(),
            next_batch: "next".into(),
            follows: false,
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
            as_of: db.as_of(),
            next_batch: "next".into(),
            follows: false,
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
        team.tags = Some(vec!["u.Projects".into()]);
        team.events = vec![message("$1", "@bob:x")];
        let mut bob = room("!bob:x", "Bob");
        bob.direct = true;
        let mut news = room("!news:x", "News");
        news.tags = Some(vec!["m.lowpriority".into()]);
        news.events = vec![message("$2", "@feed:x")];
        let mut party = room("!party:x", "Party");
        party.membership = Membership::Invited;
        party.invitation = Some(crate::rooms::ObservedInvitation {
            inviter: "@bob:x".into(),
            inviter_name: None,
        });
        db.observe(Synced {
            next_batch: "next".into(),
            follows: false,
            packs: None,
            as_of: db.as_of(),
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
            as_of: db.as_of(),
            next_batch: "next".into(),
            follows: false,
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
         OR (is:followed is:notifying is:new) OR (is:dm is:unread is:notifying) \
         OR is:invite",
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
        team.tags = Some(vec!["u.Projects".into()]);
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
            next_batch: "next".into(),
            follows: false,
            packs: None,
            as_of: db.as_of(),
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
            next_batch: "next".into(),
            follows: false,
            packs: None,
            as_of: db.as_of(),
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

mod drafts {
    use super::*;
    use uplink_protocol::{
        compose::{BlobId, Splice},
        matrix::{RoomBox, RoomDraft},
        ErrorCode,
    };

    fn splice(offset: u32, delete: &str, insert: &str) -> Splice {
        Splice {
            offset,
            delete: delete.into(),
            insert: insert.into(),
        }
    }

    impl Db {
        async fn drafts(&self) -> Vec<RoomDraft> {
            match self.matrix.list_room_drafts().await.unwrap() {
                ServerResponse::RoomDrafts { drafts, .. } => drafts,
                other => panic!("unexpected {other:?}"),
            }
        }

        /// The room's box.
        async fn the_box(&self) -> RoomBox {
            RoomBox {
                room: self.room().await.id,
                thread: None,
            }
        }

        async fn edit(&self, at: &RoomBox, revision: i64, splice: Splice) -> TxLogId {
            dispatched(self.matrix.edit_room_draft(*at, revision, splice).await)
        }

        /// What the room's box holds: its revision and text.
        async fn text(&self) -> (i64, String) {
            let drafts = self.drafts().await;
            let [draft] = drafts.as_slice() else {
                panic!("not one draft: {drafts:?}")
            };
            (draft.revision, draft.text.clone())
        }
    }

    fn dispatched(response: uplink_tx_log::answer::Response) -> TxLogId {
        match response.unwrap() {
            ServerResponse::Dispatched { tx_log_id } => tx_log_id,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_box_is_edited_at_its_revision_from_empty() {
        let db = Db::new();
        db.sync(&[("$1", "@bob:x")], &[]).await;
        let at = db.the_box().await;
        assert_eq!(db.drafts().await, [], "never written to");

        let typed = db.edit(&at, 0, splice(0, "", "helo")).await;
        assert_eq!(
            db.events_of(typed),
            [ServerEvent::RoomDraftEdited(
                uplink_protocol::matrix::RoomDraftEdited {
                    at,
                    revision: 1,
                    splice: splice(0, "", "helo"),
                }
            )]
        );
        let fixed = db.edit(&at, 1, splice(3, "", "l")).await;
        assert_eq!(db.text().await, (2, "hello".into()));

        // Based on a revision it has moved on from: refused, to rebase.
        let stale = db
            .matrix
            .edit_room_draft(at, 1, splice(0, "", "x"))
            .await
            .unwrap_err();
        assert_eq!(stale.code, ErrorCode::Conflict);

        // Undone in any order that keeps their text in place; emptied,
        // the box keeps its revision rising.
        db.dispatcher.undo(typed).await.unwrap_err();
        db.dispatcher.undo(fixed).await.unwrap();
        db.dispatcher.undo(typed).await.unwrap();
        assert_eq!(db.text().await, (4, "".into()));
        db.edit(&at, 4, splice(0, "", "again")).await;
        assert_eq!(db.text().await, (5, "again".into()));
    }

    #[tokio::test]
    async fn a_thread_has_a_box_of_its_own() {
        let db = Db::new();
        db.sync(&[("$1", "@bob:x")], &[]).await;
        let at = db.the_box().await;
        let root = db.timeline().await[0].id;
        let thread = RoomBox {
            thread: Some(root),
            ..at
        };
        db.edit(&at, 0, splice(0, "", "to the room")).await;
        db.edit(&thread, 0, splice(0, "", "to the thread")).await;
        let texts: Vec<_> = db
            .drafts()
            .await
            .into_iter()
            .map(|d| (d.at.thread, d.text))
            .collect();
        assert_eq!(
            texts,
            [
                (None, "to the room".into()),
                (Some(root), "to the thread".into())
            ]
        );

        let none = |at: RoomBox| {
            let refused = db.matrix.edit_room_draft(at, 0, splice(0, "", "x"));
            async { refused.await.unwrap_err().code }
        };
        let no_thread = RoomBox {
            thread: Some(RoomEventId(root.0 + 1)),
            ..at
        };
        assert_eq!(none(no_thread).await, ErrorCode::NotFound);
        let no_room = RoomBox {
            room: RoomId(at.room.0 + 1),
            thread: None,
        };
        assert_eq!(none(no_room).await, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn a_box_replies_to_a_message_of_its_room() {
        let db = Db::new();
        db.sync(&[("$1", "@bob:x"), ("$2", "@eve:x")], &[]).await;
        let at = db.the_box().await;
        let [first, second] = db.timeline().await.try_into().unwrap();
        let reply = |to: Option<RoomEventId>| db.matrix.set_room_draft_reply(at, to);
        let replied = || async { db.drafts().await[0].reply_to.clone().map(|e| e.event_id) };

        let began = dispatched(reply(Some(first.id)).await);
        assert_eq!(
            db.events_of(began),
            [ServerEvent::RoomDraftsChanged(
                uplink_protocol::matrix::RoomDraftsChanged {
                    rooms: vec![at.room]
                }
            )]
        );
        assert_eq!(replied().await.as_deref(), Some("$1"));
        // Not an edit of the text: its revision stays.
        assert_eq!(db.text().await, (0, "".into()));

        let other = dispatched(reply(Some(second.id)).await);
        assert_eq!(replied().await.as_deref(), Some("$2"));
        db.dispatcher.undo(other).await.unwrap();
        assert_eq!(replied().await.as_deref(), Some("$1"));
        dispatched(reply(None).await);
        assert_eq!(replied().await, None);

        let unknown = reply(Some(RoomEventId(second.id.0 + 1))).await.unwrap_err();
        assert_eq!(unknown.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn files_are_attached_to_a_rooms_box() {
        let db = Db::new();
        db.sync(&[("$1", "@bob:x")], &[]).await;
        let room = db.room().await.id;
        let blobs = uplink_storage::store::Store::blobs(db._dir.path());
        let blob = BlobId(blobs.put(b"a file").unwrap().0);
        let attach = |name: &str| {
            db.matrix
                .attach_to_room_draft(room, blob.clone(), name.into(), "text/plain".into())
        };
        let attached = || async {
            let drafts = db.drafts().await;
            let files = drafts[0].attachments.iter();
            files.map(|a| (a.name.clone(), a.size)).collect::<Vec<_>>()
        };

        let first = dispatched(attach("a.txt").await);
        dispatched(attach("b.txt").await);
        assert_eq!(attached().await, [("a.txt".into(), 6), ("b.txt".into(), 6)]);
        assert_eq!(db.text().await, (0, "".into()), "not an edit of the text");

        // Detached, and attached again as it was, by undoing.
        let id = db.drafts().await[0].attachments[0].id;
        let detached = dispatched(db.matrix.detach_from_room_draft(room, id).await);
        assert_eq!(attached().await, [("b.txt".into(), 6)]);
        let again = db
            .matrix
            .detach_from_room_draft(room, id)
            .await
            .unwrap_err();
        assert_eq!(again.code, ErrorCode::Conflict);
        db.dispatcher.undo(detached).await.unwrap();
        assert_eq!(db.drafts().await[0].attachments[0].id, id);
        db.dispatcher.undo(first).await.unwrap();
        assert_eq!(attached().await, [("b.txt".into(), 6)]);

        assert_eq!(attach("").await.unwrap_err().code, ErrorCode::Invalid);
        let none = BlobId("0".repeat(64));
        let refused = db
            .matrix
            .attach_to_room_draft(room, none, "c.txt".into(), "text/plain".into())
            .await;
        assert_eq!(refused.unwrap_err().code, ErrorCode::NotFound);
    }
}

/// Following threads (MSC4306 thread subscriptions, SPEC §9.2): fields
/// reconciled as pins are, that sending follows, and that what notifies
/// of a thread's messages goes by.
mod follows {
    use super::*;
    use crate::{
        follows::{Follows, FollowsSeen, Seen},
        reconciler::{Kind, Read, Rooms},
    };
    use uplink_protocol::matrix::{FollowChange, PendingFollows};
    use uplink_tx_log::reconcile::Ended;

    /// A message of the thread of `root`, notifying (and highlighting) so.
    fn in_thread(
        id: &str,
        sender: &str,
        root: &str,
        notify: bool,
        highlight: bool,
    ) -> ObservedEvent {
        let raw = json!({ "type": "m.room.message", "event_id": id, "sender": sender,
                          "origin_server_ts": 1_700_000_000_000_i64,
                          "content": { "msgtype": "m.text", "body": id,
                                       "m.relates_to": { "rel_type": "m.thread", "event_id": root,
                                                         "is_falling_back": true,
                                                         "m.in_reply_to": { "event_id": root } } } });
        ObservedEvent {
            push: crate::rooms::Push { notify, highlight },
            ..ObservedEvent::parse(&raw.to_string(), None, Encryption::None).unwrap()
        }
    }

    impl Db {
        fn reader(&self) -> uplink_storage::rusqlite::Connection {
            uplink_storage::open_reader(self._dir.path()).unwrap()
        }

        async fn follow(&self, root: &str, followed: bool) -> TxLogId {
            let room = self.room().await.id;
            match self
                .matrix
                .set_followed(room, root.into(), followed)
                .await
                .unwrap()
            {
                ServerResponse::Dispatched { tx_log_id } => tx_log_id,
                other => panic!("unexpected {other:?}"),
            }
        }

        /// The roots of the threads the reconciler would follow, or
        /// unfollow, claiming them.
        async fn claim_follows(&self) -> Vec<(String, bool)> {
            let wanted: Vec<_> = Follows::reconciling(&self.reader(), "mx")
                .unwrap()
                .into_iter()
                .filter(|f| f.field.wanted().is_some())
                .map(|f| (f.room, f.key))
                .collect();
            self.observe(Claimed::<Rooms<Follows>>::new(wanted))
                .await
                .unwrap();
            let conn = self.reader();
            let in_flight = Follows::reconciling(&conn, "mx")
                .unwrap()
                .into_iter()
                .filter(|f| f.field.attempt.is_some())
                .collect();
            Follows::requests(&conn, in_flight)
                .unwrap()
                .into_iter()
                .map(|(request, _)| request)
                .collect()
        }

        async fn follow_pushed(&self, root: &str, ended: Ended) {
            let room = self.room().await.id;
            self.observe(Pushed::<Rooms<Follows>> {
                pushes: vec![Push {
                    fields: vec![(room, root.to_owned())],
                    ended,
                    rekeyed: None,
                }],
            })
            .await
            .unwrap();
        }

        async fn read_follow(&self, root: &str, followed: bool) {
            let room = self.room().await.id;
            self.observe(Read::<Follows> {
                room,
                read: (root.to_owned(), followed),
                as_of: self.as_of(),
            })
            .await
            .unwrap();
        }

        fn pending_follows(&self) -> Vec<PendingFollows> {
            match self.outbox().into_iter().next().map(|a| a.pending) {
                Some(Pending::Matrix { follows, .. }) => follows,
                None => vec![],
                other => panic!("{other:?}"),
            }
        }

        /// Whether the event's thread is shown followed.
        async fn followed(&self, event_id: &str) -> bool {
            let room = self.room().await;
            let ServerResponse::Timeline { events, .. } = self
                .matrix
                .list_timeline(room.id, uplink_protocol::matrix::TimelinePage::Latest, 100)
                .await
                .unwrap()
            else {
                panic!()
            };
            events
                .iter()
                .find(|e| e.event_id == event_id)
                .unwrap()
                .followed
        }

        /// The stored messages that notify, by event id.
        fn notifying(&self) -> Vec<String> {
            self.reader()
                .prepare("SELECT event_id FROM matrix_event WHERE notify ORDER BY event_id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        }

        /// The messages `query` finds unseen, by event id.
        fn found(&self, query: &str) -> Vec<String> {
            let mut conn = self.reader();
            let tx = conn.transaction().unwrap();
            let rooms = crate::query::Rooms::of(&self.rooms, &tx).unwrap();
            let query = uplink_query::parse(query, 0).unwrap();
            let mut found: Vec<String> = rooms
                .unseen(&tx, &query, RoomEventId(0), &Default::default(), 20)
                .unwrap()
                .into_iter()
                .map(|(_, event)| event.event_id)
                .collect();
            found.sort();
            found
        }
    }

    /// Following is pushed, read back, and undone as pins are; its root
    /// shows it, a thread not yet started too.
    #[tokio::test]
    async fn a_thread_is_followed_and_unfollowed_on_the_homeserver() {
        let db = Db::following();
        db.sync(&[("$root", "@bob:x")], &[]).await;
        assert!(!db.followed("$root").await);

        let followed = db.follow("$root", true).await;
        assert_eq!(db.description(followed), "Followed a thread");
        assert!(db.followed("$root").await, "shown as asked for");
        let [pending] = &db.pending_follows()[..] else {
            panic!("{:?}", db.pending_follows())
        };
        assert_eq!(pending.op.id, followed);
        let room = db.room().await.id;
        assert_eq!(
            pending.changes,
            [FollowChange {
                room,
                root: "$root".into(),
                followed: true
            }]
        );
        assert_eq!(db.claim_follows().await, [("$root".to_owned(), true)]);
        db.follow_pushed("$root", Ended::Applied).await;
        db.read_follow("$root", true).await;
        assert!(db.pending_follows().is_empty());
        assert!(Follows::reconciling(&db.reader(), "mx").unwrap().is_empty());

        db.dispatcher.undo(followed).await.unwrap();
        assert!(!db.followed("$root").await);
        assert_eq!(db.claim_follows().await, [("$root".to_owned(), false)]);
    }

    /// Unfollowing is Slack's "Don't notify me about replies": what of the
    /// thread is stored stops notifying (but mentions), so what stood of
    /// it goes; other threads' are as they were.
    #[tokio::test]
    async fn unfollowing_quiets_the_threads_messages_but_mentions() {
        let db = Db::following();
        db.sync_events(
            vec![
                message("$root", ME),
                in_thread("$1", "@bob:x", "$root", true, false),
                in_thread("$2", "@bob:x", "$root", true, true),
                message("$other", "@bob:x"),
                in_thread("$3", "@bob:x", "$other", true, false),
            ],
            &[],
        )
        .await;
        // Followed by another client: as the homeserver says.
        db.observe(FollowsSeen {
            as_of: db.as_of(),
            seen: vec![Seen {
                room_id: ROOM.into(),
                root: "$root".into(),
                followed: true,
                mentioned: false,
            }],
        })
        .await
        .unwrap();
        assert!(db.followed("$root").await);
        assert!(db.pending_follows().is_empty(), "no opinion of uplink's");
        assert_eq!(db.found("is:followed"), ["$1", "$2"]);

        db.follow("$root", false).await;
        assert_eq!(db.notifying(), ["$2", "$3"]);
        assert!(db.found("is:followed").is_empty());
    }

    /// The homeserver's state of a thread, read as its messages come, is
    /// what uplink shows of it, as another client changes it: but not over
    /// uplink's own change, pending.
    #[tokio::test]
    async fn follows_seen_are_observed_unless_uplink_asked_otherwise() {
        let db = Db::following();
        db.sync(&[("$root", "@bob:x")], &[]).await;
        let seen = |followed| FollowsSeen {
            as_of: TxLogId(0),
            seen: vec![Seen {
                room_id: ROOM.into(),
                root: "$root".into(),
                followed,
                mentioned: false,
            }],
        };
        db.follow("$root", true).await;
        db.claim_follows().await;
        // Seen before the push ended: ambiguous, and not adopted.
        db.observe(seen(false)).await.unwrap();
        assert!(db.followed("$root").await);
    }

    /// What is sent follows its thread, as MSC4306 prescribes, by the op
    /// that sent it, once the homeserver accepted it: a reply in a thread
    /// the thread, a message to the room the thread it would be the root
    /// of. Not an edit.
    #[tokio::test]
    async fn sending_follows_the_thread_of_what_is_sent() {
        let db = Db::following();
        db.sync(&[("$root", "@bob:x")], &[]).await;
        let reply = db
            .sent(
                MatrixContent::Text {
                    body: "in the thread".into(),
                    format: TextFormat::Markdown,
                    emote: false,
                    reply_to: None,
                    replaces: None,
                    thread: Some("$root".into()),
                },
                Some("$reply"),
            )
            .await;
        assert!(db.followed("$root").await);
        let [pending] = &db.pending_follows()[..] else {
            panic!("{:?}", db.pending_follows())
        };
        assert_eq!(pending.op.id, reply);
        assert_eq!(pending.changes[0].root, "$root");

        db.sent(text("a message of mine"), Some("$mine")).await;
        assert!(db.followed("$mine").await, "the root of a thread to come");

        db.sent(
            MatrixContent::Text {
                body: "edited".into(),
                format: TextFormat::Markdown,
                emote: false,
                reply_to: None,
                replaces: Some("$mine".into()),
                thread: None,
            },
            Some("$edit"),
        )
        .await;
        let roots: Vec<String> = db
            .claim_follows()
            .await
            .into_iter()
            .map(|(root, _)| root)
            .collect();
        assert_eq!(roots, ["$mine", "$root"]);
    }

    /// Where the homeserver keeps no thread subscriptions, nothing follows,
    /// nor can be followed.
    #[tokio::test]
    async fn without_thread_subscriptions_nothing_follows() {
        let db = Db::new();
        db.sync(&[("$root", "@bob:x")], &[]).await;
        db.sent(text("hi"), Some("$mine")).await;
        assert!(db.claim_follows().await.is_empty());
        let room = db.room().await.id;
        let refused = db.matrix.set_followed(room, "$root".into(), true).await;
        assert_eq!(
            refused.unwrap_err().code,
            uplink_protocol::ErrorCode::Invalid
        );
        // Unfollowing any is never refused.
        db.follow("$root", false).await;
    }

    /// Of the default Urgent bucket: the messages of threads followed that
    /// notify and are not caught up on, as mentions are.
    #[tokio::test]
    async fn messages_of_threads_followed_are_urgent() {
        let db = Db::following();
        db.sync_events(
            vec![
                message("$root", "@bob:x"),
                in_thread("$1", "@bob:x", "$root", true, false),
                in_thread("$2", "@carol:x", "$root", false, false),
                in_thread("$mine", ME, "$root", true, false),
                message("$other", "@bob:x"),
                in_thread("$3", "@bob:x", "$other", true, false),
            ],
            &[],
        )
        .await;
        db.follow("$root", true).await;
        assert_eq!(db.found("is:followed is:notifying is:new"), ["$1"]);
    }
}
