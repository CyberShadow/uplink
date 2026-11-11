//! The log entries of rooms and sends, without a homeserver; the
//! sync task is tested against one in `tests/conduit.rs`.

use crate::{
    rooms::{ObservedEvent, ObservedRoom, ReceiptSent, Synced},
    send::{SendFailed, SendStarted, SendSucceeded},
    service::{ConfiguredAccount, Matrix},
};
use serde_json::json;
use std::sync::Arc;
use uplink_protocol::{
    compose::{AccountOutbox, Pending},
    matrix::{
        Encryption, MatrixSend, MatrixSendId, MatrixSendState, Membership, Room, RoomEvent,
        RoomEventId, RoomId,
    },
    ServerEvent, ServerResponse, TxLogId,
};
use uplink_realtime::Hub;
use uplink_tx_log::{Dispatcher, Observation, Registry};

const ME: &str = "@me:example.org";
const ROOM: &str = "!room:example.org";

struct Db {
    _dir: tempfile::TempDir,
    dispatcher: Dispatcher,
    matrix: Matrix,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        crate::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(64)),
        );
        let matrix = Matrix::new(
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
            vec![ConfiguredAccount {
                id: "mx".into(),
                mxid: ME.into(),
            }],
            uplink_storage::store::Store::blobs(dir.path()),
        );
        Self {
            _dir: dir,
            dispatcher,
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
        self.sync_room(events, own_receipts, &[]).await
    }

    /// Syncs `events`, the room's `pinned` events those.
    async fn sync_room(
        &self,
        events: Vec<ObservedEvent>,
        own_receipts: &[&str],
        pinned: &[&str],
    ) -> TxLogId {
        self.observe(Synced {
            next_batch: "next".into(),
            rooms: vec![ObservedRoom {
                room_id: ROOM.into(),
                name: "Room".into(),
                encrypted: false,
                membership: Membership::Joined,
                events,
                limited: false,
                own_receipts: own_receipts.iter().map(|r| r.to_string()).collect(),
                space: false,
                direct: false,
                avatar: None,
                children: vec![],
                space_order: None,
                tags: vec![],
                topic: None,
                pinned: pinned.iter().map(|p| p.to_string()).collect(),
                may_pin: true,
                joined_members: 2,
                version: "11".into(),
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
        match self.matrix.list_sends(room.id).await.unwrap() {
            ServerResponse::MatrixSends { sends, .. } => sends,
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
        match self.matrix.send_message(room.id, text(body)).await.unwrap() {
            ServerResponse::Dispatched { tx_log_id } => tx_log_id,
            other => panic!("unexpected {other:?}"),
        }
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
    assert_eq!(room.unread, 2, "own messages are not unread");
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
    let db = Db::new();
    let pushed = |id: &str, sender: &str, notify: bool, highlight: bool| ObservedEvent {
        push: crate::rooms::Push { notify, highlight },
        ..message(id, sender)
    };
    db.sync_events(
        vec![
            message("$1", "@bob:x"),
            pushed("$2", "@bob:x", true, false),
            pushed("$3", "@bob:x", true, true),
            // Never one's own.
            pushed("$4", ME, true, true),
        ],
        &[],
    )
    .await;
    let room = db.room().await;
    assert_eq!((room.unread, room.notifying, room.highlighted), (3, 2, 1));
    let events = db.timeline().await;
    db.matrix.mark_read(room.id, events[2].id).await.unwrap();
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
    let entry = db.sync_room(vec![], &[], &["$2", "$gone", "$1"]).await;
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
    assert!(db.matrix.send_message(room.id, pin("$gone")).await.is_err());
    assert!(db.matrix.send_message(room.id, pin("nope")).await.is_err());
    db.matrix.send_message(room.id, pin("$1")).await.unwrap();
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

    // Undoing it now can't take it back: it needs a decision, which
    // keeping it sent (undoing the undo) makes.
    let cancel = db.dispatcher.undo(sent).await.unwrap();
    assert_eq!(db.shown().await, [(MatrixSendState::Sent, false)]);
    assert_eq!(db.outbox().len(), 1);
    db.dispatcher.undo(cancel).await.unwrap();
    assert!(db.sends().await.is_empty());

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
    let read = match db.matrix.mark_read(room.id, second).await.unwrap() {
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
async fn messages_go_only_to_known_rooms() {
    let db = Db::new();
    let refused = db
        .matrix
        .send_message(RoomId(1), text("hi"))
        .await
        .unwrap_err();
    assert_eq!(refused.code, uplink_protocol::ErrorCode::NotFound);
    db.sync(&[], &[]).await;
    let room = db.room().await;
    let refused = db
        .matrix
        .send_message(room.id, text(" "))
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
                next_batch: "next".into(),
                rooms: vec![ObservedRoom {
                    room_id: ROOM.into(),
                    name: "Room".into(),
                    encrypted: false,
                    membership,
                    events: vec![],
                    limited: false,
                    own_receipts: vec![],
                    space: false,
                    direct: false,
                    avatar: None,
                    children: vec![],
                    space_order: None,
                    tags: vec![],
                    topic: None,
                    pinned: vec![],
                    may_pin: false,
                    joined_members: 2,
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
            self.observe(Synced {
                next_batch: "next".into(),
                rooms: vec![ObservedRoom {
                    room_id: ROOM.into(),
                    name: "Room".into(),
                    encrypted: false,
                    membership: Membership::Joined,
                    events: vec![],
                    limited: false,
                    own_receipts: vec![],
                    space: false,
                    direct: false,
                    avatar: None,
                    children: vec![],
                    space_order: None,
                    tags: tags.iter().map(|t| t.to_string()).collect(),
                    topic: None,
                    pinned: vec![],
                    may_pin: false,
                    joined_members: 1,
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
                next_batch: "next".into(),
                rooms: vec![ObservedRoom {
                    room_id: ROOM.into(),
                    name: "Space".into(),
                    encrypted: false,
                    membership: Membership::Joined,
                    events: vec![],
                    limited: false,
                    own_receipts: vec![],
                    space: true,
                    direct: false,
                    avatar: None,
                    children: vec![],
                    space_order: order.map(Into::into),
                    tags: vec![],
                    topic: None,
                    pinned: vec![],
                    may_pin: false,
                    joined_members: 1,
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

        let room = db.room().await.id;
        let ServerResponse::Edits { edits, .. } =
            db.matrix.list_edits(room, first.id).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(ids(&edits), ["$2", "$3"], "not another's");
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
                markdown: true,
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
            db.matrix.send_message(room, content).await.unwrap();
        }
        for content in [
            to_thread("$2", None, None),
            to_thread("$1", Some("$3"), None),
            to_thread("$1", None, Some("$5")),
        ] {
            let refused = db.matrix.send_message(room, content).await.unwrap_err();
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
        markdown: true,
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
        true,
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
        false,
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
        "[Bob](https://matrix.to/#/@bob:example.org)!",
        true,
        true,
        None,
        None,
        Some("$e"),
    );
    assert_eq!(edit["body"], "* Bob!");
    assert_eq!(edit["msgtype"], "m.emote");
    assert_eq!(edit["m.mentions"], json!({}));
    assert_eq!(
        edit["m.new_content"]["m.mentions"],
        json!({ "user_ids": ["@bob:example.org"] })
    );

    // Of a thread: answering its latest by fallback, mentioning no one;
    // or replying within it.
    let fallback = text_content("hi", true, false, None, Some(("$root", "$latest")), None);
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
        true,
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
}

mod query {
    use super::*;
    use crate::service::ConfiguredAccount;

    fn room(id: &str, name: &str) -> ObservedRoom {
        ObservedRoom {
            room_id: id.into(),
            name: name.into(),
            encrypted: false,
            membership: Membership::Joined,
            events: vec![],
            limited: false,
            own_receipts: vec![],
            space: false,
            direct: false,
            avatar: None,
            children: vec![],
            space_order: None,
            tags: vec![],
            topic: None,
            pinned: vec![],
            may_pin: false,
            joined_members: 2,
            version: "11".into(),
        }
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
            next_batch: "next".into(),
            rooms: vec![work, team, bob, news, party],
        })
        .await
        .unwrap();
        let matching = |query: &str| -> Vec<String> {
            let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
            let tx = conn.transaction().unwrap();
            let accounts = [ConfiguredAccount {
                id: "mx".into(),
                mxid: ME.into(),
            }];
            let query = uplink_query::parse(query, 0).unwrap();
            let mut names: Vec<String> = crate::query::rooms(&tx, &accounts, &query)
                .unwrap()
                .into_iter()
                .map(|r| r.name)
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
        assert_eq!(matching("account:@me:example.org is:read").len(), 2);
        // Terms of other kinds are false of rooms.
        assert!(matching("from:bob OR is:todo OR in:inbox").is_empty());
        assert_eq!(matching("-in:inbox").len(), 4);

        let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        let known = |name| crate::query::tag_known(&tx, name).unwrap();
        assert!(known("favourite") && known("low_priority") && known("Projects"));
        assert!(!known("inbox"));
        // Reading a room marks it read up to its latest event.
        let rooms = crate::rooms::rooms(&tx, "mx", ME).unwrap();
        let latest = |name: &str| rooms.iter().find(|r| r.name == name).unwrap().latest;
        assert!(latest("Team chat").is_some());
        assert_eq!(latest("Bob"), None);
    }
}
