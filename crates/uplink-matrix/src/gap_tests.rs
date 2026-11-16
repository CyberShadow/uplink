//! Filling a timeline's gap with a page the homeserver sent.

use crate::rooms::{Filled, ObservedEvent, ObservedRoom, Synced};
use std::sync::Arc;
use uplink_protocol::matrix::{Encryption, GapId, Membership, RoomId};
use uplink_tx_log::{Dispatcher, Registry};

fn message(event_id: &str) -> ObservedEvent {
    let raw = serde_json::json!({
        "type": "m.room.message",
        "event_id": event_id,
        "sender": "@other:example.org",
        "origin_server_ts": 1_700_000_000_000_i64,
        "content": { "msgtype": "m.text", "body": format!("message {event_id}") },
    });
    ObservedEvent::parse(&raw.to_string(), None, Encryption::None).unwrap()
}

/// tuwunel gave two rooms one id (a room of version 12's id is the hash
/// of its creation, and the bridge created two in one millisecond), and
/// the room's history listed its creation twice: it is stored once, and
/// the fill does not fail (it panicked, holding the database, and
/// poisoned it for every request after).
#[tokio::test]
async fn an_event_a_page_lists_twice_is_stored_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    crate::register(&mut registry);
    let dispatcher = Dispatcher::new(
        uplink_storage::open(dir.path()).unwrap(),
        registry,
        Arc::new(uplink_realtime::Hub::new(64)),
    );
    // A limited sync: a gap before its event.
    dispatcher
        .observe(
            "mx".into(),
            Synced {
                next_batch: "next".into(),
                rooms: vec![ObservedRoom {
                    room_id: "!room:example.org".into(),
                    name: "Room".into(),
                    encrypted: false,
                    membership: Membership::Joined,
                    events: vec![message("$latest")],
                    limited: true,
                    own_receipts: vec![],
                    fully_read: None,
                    space: false,
                    direct: false,
                    avatar: None,
                    children: vec![],
                    space_order: None,
                    tags: vec![],
                    topic: None,
                    alias: None,
                    pinned: vec![],
                    may_pin: true,
                    joined_members: 2,
                    created: None,
                    version: "12".into(),
                }],
            },
        )
        .await
        .unwrap();
    let reader = uplink_storage::open_reader(dir.path()).unwrap();
    let (gap, room): (i64, i64) = reader
        .query_row("SELECT id, room FROM matrix_gap", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();

    // Newest first; the oldest listed twice.
    dispatcher
        .observe(
            "mx".into(),
            Filled {
                room: RoomId(room),
                gap: GapId(gap),
                version: "12".into(),
                events: vec![message("$b"), message("$a"), message("$a")],
                start: true,
                own_receipts: vec![],
                fully_read: None,
            },
        )
        .await
        .unwrap();

    let stored: Vec<String> = reader
        .prepare("SELECT event_id FROM matrix_event WHERE room = ?1 ORDER BY pos")
        .unwrap()
        .query_map([room], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(stored, ["$a", "$b", "$latest"]);
    let gaps: i64 = reader
        .query_row("SELECT count(*) FROM matrix_gap", [], |r| r.get(0))
        .unwrap();
    assert_eq!(gaps, 0);
}
