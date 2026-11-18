//! Storing a timeline's events: filling its gaps with pages the
//! homeserver sent, and naming senders who had no name when they spoke.

use crate::rooms::{Filled, ObservedEvent, ObservedRoom, Synced};
use std::sync::Arc;
use uplink_protocol::matrix::{Encryption, GapId, Membership, RoomId};
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::{Dispatcher, Registry};

/// What a sync requested after every entry tells of: current.
const FRESH: uplink_protocol::TxLogId = uplink_protocol::TxLogId(i64::MAX);

const ROOM: &str = "!room:example.org";

fn event(raw: serde_json::Value) -> ObservedEvent {
    ObservedEvent::parse(&raw.to_string(), None, Encryption::None).unwrap()
}

fn message(event_id: &str) -> ObservedEvent {
    message_from(event_id, "@other:example.org")
}

/// A message, whose sender's name at the time is unknown (`None`).
fn message_from(event_id: &str, sender: &str) -> ObservedEvent {
    event(serde_json::json!({
        "type": "m.room.message",
        "event_id": event_id,
        "sender": sender,
        "origin_server_ts": 1_700_000_000_000_i64,
        "content": { "msgtype": "m.text", "body": format!("message {event_id}") },
    }))
}

fn named(event_id: &str, user: &str, name: &str) -> ObservedEvent {
    event(serde_json::json!({
        "type": "m.room.member",
        "event_id": event_id,
        "sender": user,
        "state_key": user,
        "origin_server_ts": 1_700_000_000_000_i64,
        "content": { "membership": "join", "displayname": name },
    }))
}

struct Db {
    _dir: tempfile::TempDir,
    dispatcher: Dispatcher,
    reader: Connection,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        crate::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(uplink_realtime::Hub::new(64)),
        );
        let reader = uplink_storage::open_reader(dir.path()).unwrap();
        Self {
            _dir: dir,
            dispatcher,
            reader,
        }
    }

    async fn sync(&self, events: Vec<ObservedEvent>, limited: bool) {
        self.dispatcher
            .observe(
                "mx".into(),
                Synced {
                    requested: FRESH,
                    next_batch: "next".into(),
                    packs: None,
                    rooms: vec![ObservedRoom {
                        room_id: ROOM.into(),
                        name: "Room".into(),
                        encrypted: false,
                        membership: Membership::Joined,
                        events,
                        limited,
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
                    }],
                },
            )
            .await
            .unwrap();
    }

    /// The room's gap, and the room.
    fn gap(&self) -> (GapId, RoomId) {
        let (gap, room) = self
            .reader
            .query_row("SELECT id, room FROM matrix_gap", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        (GapId(gap), RoomId(room))
    }

    async fn fill(&self, events: Vec<ObservedEvent>) {
        let (gap, room) = self.gap();
        self.dispatcher
            .observe(
                "mx".into(),
                Filled {
                    room,
                    gap,
                    version: "11".into(),
                    events,
                    start: true,
                    own_receipts: vec![],
                    fully_read: None,
                },
            )
            .await
            .unwrap();
    }

    /// The timeline's events, oldest first, with their senders' names.
    fn stored(&self) -> Vec<(String, Option<String>)> {
        self.reader
            .prepare("SELECT event_id, sender_name FROM matrix_event ORDER BY pos")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
}

/// tuwunel gave two rooms one id (a room of version 12's id is the hash
/// of its creation, and the bridge created two in one millisecond), and
/// the room's history listed its creation twice: it is stored once, and
/// the fill does not fail (it panicked, holding the database, and
/// poisoned it for every request after).
#[tokio::test]
async fn an_event_a_page_lists_twice_is_stored_once() {
    let db = Db::new();
    db.sync(vec![message("$latest")], true).await;
    // Newest first; the oldest listed twice.
    db.fill(vec![message("$b"), message("$a"), message("$a")])
        .await;
    let ids: Vec<String> = db.stored().into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, ["$a", "$b", "$latest"]);
    let gaps: i64 = db
        .reader
        .query_row("SELECT count(*) FROM matrix_gap", [], |r| r.get(0))
        .unwrap();
    assert_eq!(gaps, 0);
}

/// A bridge joins a room's members before it knows their names, and
/// names them after: their messages before are named by the name given.
#[tokio::test]
async fn a_member_named_later_names_their_messages_from_before() {
    let db = Db::new();
    db.sync(
        vec![
            message_from("$1", "@bea:example.org"),
            message_from("$2", "@cid:example.org"),
        ],
        false,
    )
    .await;
    db.sync(
        vec![
            named("$bea", "@bea:example.org", "Bea"),
            message_from("$3", "@bea:example.org"),
        ],
        false,
    )
    .await;
    let names: Vec<Option<String>> = db.stored().into_iter().map(|(_, name)| name).collect();
    // Bea's message before, named; Cid, never named, not; the member event
    // and the message after, as they came.
    assert_eq!(names, [Some("Bea".into()), None, None, None]);
}

/// Earlier history fetched after the member's naming was stored is named
/// by it too.
#[tokio::test]
async fn earlier_messages_fetched_are_named_by_the_name_given_after() {
    let db = Db::new();
    db.sync(vec![named("$bea", "@bea:example.org", "Bea")], true)
        .await;
    db.fill(vec![message_from("$1", "@bea:example.org")]).await;
    assert_eq!(
        db.stored(),
        [("$1".into(), Some("Bea".into())), ("$bea".into(), None)]
    );
}

/// Of a member named twice, their events before each name are named by
/// it: those before the first by the first, those between (the first
/// member event too, which had no name of its sender) by the second.
#[tokio::test]
async fn messages_are_named_by_the_next_name_given() {
    let db = Db::new();
    db.sync(vec![message_from("$1", "@bea:example.org")], false)
        .await;
    db.sync(vec![named("$b1", "@bea:example.org", "Bea")], false)
        .await;
    db.sync(vec![message_from("$2", "@bea:example.org")], false)
        .await;
    db.sync(vec![named("$b2", "@bea:example.org", "Beatrice")], false)
        .await;
    let names: Vec<Option<String>> = db.stored().into_iter().map(|(_, name)| name).collect();
    assert_eq!(
        names,
        [
            Some("Bea".into()),
            Some("Beatrice".into()),
            Some("Beatrice".into()),
            None
        ]
    );
}
