//! Reading a thread marks its messages read (MSC3771 threaded receipts,
//! SPEC §12.15): its own read marker, desired state as the room's is,
//! pushed as a threaded receipt, and moved by the account's own threaded
//! receipts from other clients.

use super::*;
use crate::rooms::{FullyRead, MarkRead, MarkThreadRead};

/// A message of the thread of `root`, notifying so.
fn reply(id: &str, sender: &str, root: &str, notify: bool) -> ObservedEvent {
    let raw = json!({ "type": "m.room.message", "event_id": id, "sender": sender,
                      "origin_server_ts": 1_700_000_000_000_i64,
                      "content": { "msgtype": "m.text", "body": id,
                                   "m.relates_to": { "rel_type": "m.thread", "event_id": root,
                                                     "is_falling_back": true,
                                                     "m.in_reply_to": { "event_id": root } } } });
    ObservedEvent {
        push: crate::rooms::Push {
            notify,
            highlight: false,
        },
        ..ObservedEvent::parse(&raw.to_string(), None, Encryption::None).unwrap()
    }
}

impl Db {
    fn thread_conn(&self) -> uplink_storage::rusqlite::Connection {
        uplink_storage::open_reader(self._dir.path()).unwrap()
    }

    /// The messages `query` finds unseen, by event id.
    fn thread_found(&self, query: &str) -> Vec<String> {
        let mut conn = self.thread_conn();
        let tx = conn.transaction().unwrap();
        let rooms = crate::query::Rooms::of(&self.rooms, &tx).unwrap();
        let query = uplink_query::parse(query, 0).unwrap();
        let mut found: Vec<String> = rooms
            .unseen(&tx, &query, RoomEventId(0), &Default::default(), 50)
            .unwrap()
            .into_iter()
            .map(|(_, event)| event.event_id)
            .collect();
        found.sort();
        found
    }

    async fn followed_thread(&self, root: &str) {
        let room = self.room().await.id;
        self.matrix
            .set_followed(room, root.into(), true)
            .await
            .unwrap();
    }

    /// The thread of `root`: its messages, and its read marker.
    async fn thread_of(&self, root: &str) -> (Vec<RoomEvent>, Option<RoomEventId>) {
        let room = self.room().await.id;
        let root = self.stored_id(root);
        match self.matrix.list_thread(room, root).await.unwrap() {
            ServerResponse::Thread {
                replies,
                read_up_to,
                ..
            } => (replies, read_up_to.map(|r| r.event)),
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn read_thread(&self, root: &str, up_to: &str) -> TxLogId {
        let room = self.room().await.id;
        let (root, up_to) = (self.stored_id(root), self.stored_id(up_to));
        match self
            .matrix
            .mark_thread_read(room, root, up_to, true)
            .await
            .unwrap()
        {
            ServerResponse::Dispatched { tx_log_id } => tx_log_id,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A stored event's id, of the main timeline or a thread.
    fn stored_id(&self, event_id: &str) -> RoomEventId {
        RoomEventId(
            self.thread_conn()
                .query_row(
                    "SELECT id FROM matrix_event WHERE event_id = ?1",
                    [event_id],
                    |r| r.get(0),
                )
                .unwrap(),
        )
    }

    /// The threaded receipts to send: of each, its event and thread.
    fn receipts_to_send(&self) -> Vec<(String, Option<String>)> {
        crate::rooms::unsent_receipts(&self.thread_conn(), "mx")
            .unwrap()
            .into_iter()
            .filter(|r| r.thread.is_some())
            .map(|r| (r.event_id, r.thread))
            .collect()
    }

    /// Sets the room's read and fully-read markers at `event`.
    async fn read_room_to(&self, event: &str) {
        let up_to = Some(self.stored_id(event));
        self.dispatcher
            .dispatch(MarkRead {
                room: self.room().await.id,
                up_to,
                shown: false,
                fully_read: Some(FullyRead { up_to }),
            })
            .await
            .unwrap();
    }
}

/// The replies of a thread followed are unread, and notify, until the
/// thread is read, which marks them read, and sends a threaded receipt;
/// the room's own markers stay where they were.
#[tokio::test]
async fn reading_a_thread_marks_its_replies_read() {
    let db = Db::following();
    db.sync_events(
        vec![
            message("$root", "@bob:x"),
            reply("$1", "@bob:x", "$root", true),
            reply("$2", "@carol:x", "$root", true),
        ],
        &[],
    )
    .await;
    db.read_room_to("$root").await;
    db.followed_thread("$root").await;
    let before = db.room().await;
    assert_eq!(
        db.thread_found("is:followed is:notifying is:new"),
        ["$1", "$2"]
    );
    assert_eq!(db.thread_found("is:unread"), ["$1", "$2"]);
    assert_eq!((before.unread, before.notifying), (2, 2));

    db.read_thread("$root", "$1").await;
    assert_eq!(db.thread_found("is:unread"), ["$2"]);
    let read = db.read_thread("$root", "$2").await;
    assert!(db
        .thread_found("is:followed is:notifying is:new")
        .is_empty());
    assert!(db.thread_found("is:unread").is_empty());
    let after = db.room().await;
    assert_eq!((after.unread, after.notifying), (0, 0));
    assert_eq!(after.read_up_to, before.read_up_to, "the room's own");
    assert_eq!(after.fully_read, before.fully_read);
    assert_eq!(db.thread_of("$root").await.1, Some(db.stored_id("$2")));

    // A threaded receipt for it is to send, until it is sent.
    assert_eq!(
        db.receipts_to_send(),
        [("$2".to_owned(), Some("$root".to_owned()))]
    );
    assert!(
        matches!(&db.outbox()[0].pending, Pending::Matrix { receipts, .. }
                 if receipts == &[after.id]),
        "{:?}",
        db.outbox()
    );
    db.observe(ReceiptSent {
        room: after.id,
        up_to: db.stored_id("$2"),
        fully_read: false,
        thread: Some("$root".into()),
    })
    .await
    .unwrap();
    assert!(db.receipts_to_send().is_empty());

    // Undone, unread again, in uplink only: receipts only go forward.
    db.dispatcher.undo(read).await.unwrap();
    assert_eq!(db.thread_found("is:unread"), ["$2"]);
    assert_eq!(db.thread_of("$root").await.1, Some(db.stored_id("$1")));
    assert!(db.receipts_to_send().is_empty());
}

/// A threaded receipt of the account's from another client, synced, reads
/// the thread as reading it here does; it is not the room's.
#[tokio::test]
async fn own_threaded_receipts_read_the_thread() {
    let db = Db::following();
    db.sync_events(
        vec![
            message("$root", "@bob:x"),
            reply("$1", "@bob:x", "$root", true),
            reply("$2", "@bob:x", "$root", true),
        ],
        &[],
    )
    .await;
    db.followed_thread("$root").await;
    assert_eq!(db.thread_found("is:unread"), ["$1", "$2", "$root"]);
    db.observe(Synced {
        next_batch: "next".into(),
        follows: true,
        packs: None,
        as_of: db.as_of(),
        rooms: vec![ObservedRoom {
            own_thread_receipts: vec![("$root".into(), "$2".into())],
            ..joined(ROOM, vec![])
        }],
    })
    .await
    .unwrap();
    assert_eq!(db.thread_found("is:unread"), ["$root"]);
    assert_eq!(db.thread_found("is:new"), ["$root"]);
    assert_eq!(db.thread_of("$root").await.1, Some(db.stored_id("$2")));
    // Seen sent: none to send of it.
    assert!(db.receipts_to_send().is_empty());
    assert_eq!(db.room().await.read_up_to, None);
}

/// What was read before threads had markers, at or before the room's
/// markers, stays read; replies of threads not followed are not unread
/// (no activity, MSC4306), but new until read.
#[tokio::test]
async fn replies_before_the_rooms_markers_stay_read() {
    let db = Db::following();
    db.sync_events(
        vec![
            message("$root", "@bob:x"),
            reply("$1", "@bob:x", "$root", true),
            message("$main", "@bob:x"),
            reply("$2", "@bob:x", "$root", true),
        ],
        &[],
    )
    .await;
    db.read_room_to("$main").await;
    assert!(db.thread_found("is:unread").is_empty(), "not followed");
    assert_eq!(db.thread_found("is:new"), ["$2"]);
    db.followed_thread("$root").await;
    assert_eq!(db.thread_found("is:unread"), ["$2"]);
    assert_eq!(db.room().await.unread, 1);
}

/// Only a message of the thread marks it read.
#[tokio::test]
async fn a_thread_is_read_up_to_its_own_messages() {
    let db = Db::following();
    db.sync(&[("$root", "@bob:x"), ("$main", "@bob:x")], &[])
        .await;
    let room = db.room().await.id;
    let refused = db
        .matrix
        .mark_thread_read(room, db.stored_id("$root"), db.stored_id("$main"), false)
        .await;
    assert_eq!(
        refused.unwrap_err().code,
        uplink_protocol::ErrorCode::Invalid
    );
}

/// A thread's marker is found by its root, a seek, as is the account's
/// own latest message of it; what is after the room's markers is found
/// by their positions.
#[tokio::test]
async fn thread_markers_are_looked_up_not_scanned() {
    let db = Db::following();
    let mut events = vec![message("$root", "@bob:x")];
    events.extend((0..200).map(|i| reply(&format!("$h{i}"), "@bob:x", "$root", true)));
    events.extend((0..200).map(|i| message(&format!("$m{i}"), "@bob:x")));
    db.sync_events(events, &[]).await;
    db.read_room_to("$m190").await;
    uplink_storage::open(db._dir.path()).unwrap();
    let conn = db.thread_conn();
    let me = "'@me:example.org'";
    let sql = format!(
        "EXPLAIN QUERY PLAN SELECT e.id FROM {marked} r CROSS JOIN matrix_event e
         WHERE e.room = r.id AND {after}",
        marked = crate::rooms::marked(me, "r.account_id = 'mx'"),
        after = crate::rooms::after(crate::rooms::Marker::Read, me),
    );
    let plan: Vec<String> = conn
        .prepare(&sql)
        .unwrap()
        .query_map([], |r| r.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let plan = plan.join("\n");
    assert!(plan.contains("(room=? AND pos>?)"), "{plan}");
    assert!(
        plan.contains(
            "SEARCH t USING INDEX sqlite_autoindex_matrix_thread_read_1 (room=? AND root=?)"
        ),
        "{plan}"
    );
    assert!(
        plan.contains("matrix_event_thread_by_sender (room=? AND relates_to=? AND sender=?)"),
        "{plan}"
    );
    assert!(
        !plan.contains("SCAN t") && !plan.contains("SCAN o"),
        "{plan}"
    );
}

/// The home buckets' queries cost what is unread, not a thread's history
/// read: its replies before the room's marker are not visited.
#[tokio::test]
async fn home_queries_cost_what_is_unread_not_threads_history() {
    use uplink_storage::work::Work;
    async fn work_of(history: usize) -> u64 {
        let db = Db::following();
        let mut events = vec![message("$root", "@bob:x")];
        events.extend((0..history).map(|i| reply(&format!("$h{i}"), "@bob:x", "$root", true)));
        events.push(message("$main", "@bob:x"));
        events.extend((0..5).map(|i| reply(&format!("$n{i}"), "@bob:x", "$root", true)));
        db.sync_events(events, &[]).await;
        db.read_room_to("$main").await;
        db.followed_thread("$root").await;
        db.dispatcher
            .dispatch(MarkThreadRead {
                room: db.room().await.id,
                root: "$root".into(),
                up_to: Some(db.stored_id("$n2")),
                shown: false,
            })
            .await
            .unwrap();
        uplink_storage::open(db._dir.path()).unwrap();
        let mut conn = db.thread_conn();
        let work = Work::default();
        work.count_on(&conn);
        let tx = conn.transaction().unwrap();
        let rooms = crate::query::Rooms::of(&db.rooms, &tx).unwrap();
        let query = uplink_query::parse(
            "(is:followed is:notifying is:new) OR (is:unread is:notifying)",
            0,
        )
        .unwrap();
        // As home lists its buckets' rooms.
        let (steps, found) = work.of(|| rooms.matching(&tx, &query).unwrap().len());
        assert_eq!(found, 1);
        steps
    }
    let (few, many) = (work_of(10).await, work_of(400).await);
    assert!(
        (many as f64) < 1.5 * few as f64,
        "of 10 replies read {few} steps, of 400 {many}"
    );
}
