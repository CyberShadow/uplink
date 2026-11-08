//! The log entries of rooms and the outbox, without a homeserver; the
//! sync task is tested against one in `tests/conduit.rs`.

use crate::{
    outbox::{SendFailed, SendStarted, SendSucceeded},
    rooms::{ObservedEvent, ObservedRoom, ReceiptSent, Synced},
    service::{ConfiguredAccount, Matrix},
};
use std::sync::Arc;
use uplink_protocol::{
    compose::OutboxState,
    matrix::{Encryption, MatrixOutboxItem, Membership, Room, RoomEventId, RoomId},
    ServerResponse, TxLogId,
};
use uplink_realtime::Hub;
use uplink_tx_log::{Dispatcher, Observation, Registry, UndoError};

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
        self.observe(Synced {
            next_batch: "next".into(),
            rooms: vec![ObservedRoom {
                room_id: ROOM.into(),
                name: "Room".into(),
                encrypted: false,
                membership: Membership::Joined,
                events: events
                    .iter()
                    .map(|(id, sender)| message(id, sender))
                    .collect(),
                own_receipts: own_receipts.iter().map(|r| r.to_string()).collect(),
            }],
        })
        .await
        .unwrap();
    }

    async fn room(&self) -> Room {
        match self.matrix.list_rooms().await.unwrap() {
            ServerResponse::Rooms { mut rooms, .. } => rooms.remove(0),
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn outbox(&self) -> Vec<MatrixOutboxItem> {
        match self.matrix.list_outbox().await.unwrap() {
            ServerResponse::MatrixOutbox { items, .. } => items,
            other => panic!("unexpected {other:?}"),
        }
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
            .send_message(room.id, body.into())
            .await
            .unwrap()
        {
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
async fn sending_is_undone_while_queued() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    let sent = db.send("hi").await;
    assert_eq!(db.outbox().await[0].state, OutboxState::Queued);
    db.dispatcher.undo(sent).await.unwrap();
    assert!(db.outbox().await.is_empty());
}

#[tokio::test]
async fn sending_cannot_be_undone_once_started() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    let sent = db.send("hi").await;
    let item = db.outbox().await[0].id;
    db.observe(SendStarted { item }).await.unwrap();
    let refused = db.dispatcher.undo(sent).await.unwrap_err();
    assert!(
        matches!(&refused, UndoError::Conflict { conflict, .. } if conflict.0 == "the message is being sent"),
        "{refused}"
    );
    // Claiming twice is refused too.
    assert!(db.observe(SendStarted { item }).await.is_err());

    // A passing failure queues it again; a refusal fails it.
    db.observe(SendFailed {
        item,
        refused: None,
    })
    .await
    .unwrap();
    assert_eq!(db.outbox().await[0].state, OutboxState::Queued);
    db.observe(SendStarted { item }).await.unwrap();
    db.observe(SendFailed {
        item,
        refused: Some("M_FORBIDDEN".into()),
    })
    .await
    .unwrap();
    assert_eq!(db.outbox().await[0].error.as_deref(), Some("M_FORBIDDEN"));
    // A failed message can be cancelled.
    db.dispatcher.undo(sent).await.unwrap();
    assert!(db.outbox().await.is_empty());
}

#[tokio::test]
async fn sent_messages_leave_the_outbox_once_synced() {
    let db = Db::new();
    db.sync(&[("$1", "@bob:x")], &[]).await;
    // Synced after the homeserver answered.
    db.send("first").await;
    let first = db.outbox().await[0].id;
    db.observe(SendStarted { item: first }).await.unwrap();
    db.observe(SendSucceeded {
        item: first,
        event_id: "$first".into(),
    })
    .await
    .unwrap();
    assert_eq!(db.outbox().await[0].state, OutboxState::Sent);
    db.sync(&[("$first", ME)], &[]).await;
    assert!(db.outbox().await.is_empty());

    // Synced before the homeserver answered.
    db.send("second").await;
    let second = db.outbox().await[0].id;
    db.observe(SendStarted { item: second }).await.unwrap();
    db.sync(&[("$second", ME)], &[]).await;
    db.observe(SendSucceeded {
        item: second,
        event_id: "$second".into(),
    })
    .await
    .unwrap();
    assert!(db.outbox().await.is_empty());
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
    // The receipt is sent; its echo does not move the marker again.
    db.observe(ReceiptSent {
        room: room.id,
        up_to: second,
    })
    .await
    .unwrap();
    db.dispatcher.undo(read).await.unwrap();
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
        .send_message(RoomId(1), "hi".into())
        .await
        .unwrap_err();
    assert_eq!(refused.code, uplink_protocol::ErrorCode::NotFound);
    db.sync(&[], &[]).await;
    let room = db.room().await;
    let refused = db
        .matrix
        .send_message(room.id, " ".into())
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
                    own_receipts: vec![],
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

    #[tokio::test]
    async fn refusals_are_kept_and_other_changes_conflict() {
        let db = Db::new();
        db.sync_membership(Membership::Invited).await;
        let join = db.change(MembershipChange::Join).await;
        let code = db
            .matrix
            .change_membership(db.room().await.id, MembershipChange::Leave)
            .await
            .unwrap_err()
            .code;
        assert_eq!(code, ErrorCode::Conflict, "a change is pending");
        db.reconcile(MembershipChange::Join, Some("M_FORBIDDEN"))
            .await;
        let room = db.room().await;
        assert_eq!((room.membership, room.pending), (Membership::Invited, None));
        assert_eq!(room.membership_error.as_deref(), Some("M_FORBIDDEN"));

        // Refused: there is nothing to undo.
        let refused = db.dispatcher.undo(join).await.unwrap_err();
        assert!(matches!(refused, UndoError::Conflict { .. }), "{refused}");
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
}
