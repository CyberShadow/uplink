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
        self.observe(Synced {
            next_batch: "next".into(),
            rooms: vec![ObservedRoom {
                room_id: ROOM.into(),
                name: "Room".into(),
                encrypted: false,
                membership: Membership::Joined,
                events,
                own_receipts: own_receipts.iter().map(|r| r.to_string()).collect(),
                space: false,
                avatar: None,
                children: vec![],
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
        event_id: "$first".into(),
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
        event_id: "$second".into(),
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
                    space: false,
                    avatar: None,
                    children: vec![],
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
