//! Following threads (MSC4306 thread subscriptions, SPEC §9.2) against a
//! homeserver that has them: Synapse (`UPLINK_SYNAPSE`, set by the dev
//! shell and `nix flake check`), its experimental `msc4306_enabled` on,
//! run for each test. Conduit (`tests/conduit.rs`) has none. Another
//! matrix-sdk client plays the other side.

use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        api::client::{
            account::register::v3::Request as Register,
            uiaa::{AuthData, Dummy},
        },
        EventId, OwnedEventId, OwnedRoomId, RoomId,
    },
    Client,
};
use serde_json::json;
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use uplink_config::MatrixAccountConfig;
use uplink_matrix::{
    client, room_view::RoomsView, service::ConfiguredAccount, service::Matrix, sync::Sync,
};
use uplink_protocol::{
    matrix::{MatrixContent, Membership, Room, TextFormat},
    ServerResponse,
};
use uplink_realtime::Hub;
use uplink_storage::seal::MasterKey;
use uplink_tx_log::{Dispatcher, Registry};

const PASSWORD: &str = "correct horse battery staple";

fn port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Homeserver {
    url: String,
    _synapse: tokio::process::Child,
}

impl Homeserver {
    async fn start(dir: &Path) -> Self {
        let synapse = std::env::var_os("UPLINK_SYNAPSE")
            .expect("UPLINK_SYNAPSE must name synapse_homeserver (the dev shell sets it)");
        let port = port();
        let data = dir.join("synapse");
        std::fs::create_dir(&data).unwrap();
        let config = dir.join("synapse.yaml");
        let d = data.display();
        // As few limits as a test needs: none of rate, nor of keys from
        // other servers, with no federation.
        let limit = "{per_second: 1000, burst_count: 1000}";
        std::fs::write(
            &config,
            format!(
                "server_name: localhost
pid_file: {d}/homeserver.pid
listeners:
  - port: {port}
    bind_addresses: ['127.0.0.1']
    type: http
    tls: false
    resources: [{{names: [client], compress: false}}]
database: {{name: sqlite3, args: {{database: {d}/homeserver.db}}}}
media_store_path: {d}/media
signing_key_path: {d}/signing.key
trusted_key_servers: []
suppress_key_server_warning: true
report_stats: false
enable_registration: true
enable_registration_without_verification: true
rc_message: {limit}
rc_registration: {limit}
rc_joins: {{local: {limit}, remote: {limit}}}
rc_invites: {{per_room: {limit}, per_user: {limit}, per_issuer: {limit}}}
rc_login: {{address: {limit}, account: {limit}, failed_attempts: {limit}}}
experimental_features:
  msc4306_enabled: true
"
            ),
        )
        .unwrap();
        let generated = tokio::process::Command::new(&synapse)
            .arg("-c")
            .arg(&config)
            .arg("--generate-keys")
            .output()
            .await
            .unwrap();
        assert!(generated.status.success(), "{generated:?}");
        let log = dir.join("synapse.log");
        let child = tokio::process::Command::new(&synapse)
            .arg("-c")
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(std::fs::File::create(dir.join("synapse.err")).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        for _ in 0..600 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return Self {
                    url: format!("http://127.0.0.1:{port}"),
                    _synapse: child,
                };
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "synapse did not start:\n{}{}",
            std::fs::read_to_string(&log).unwrap(),
            std::fs::read_to_string(dir.join("synapse.err")).unwrap()
        );
    }

    /// A client of a newly registered user, with its own store under `dir`.
    async fn register(&self, dir: &Path, user: &str) -> Client {
        let client = Client::builder()
            .homeserver_url(&self.url)
            .sqlite_store(dir.join(user), None)
            .build()
            .await
            .unwrap();
        let mut request = Register::new();
        request.username = Some(user.to_owned());
        request.password = Some(PASSWORD.to_owned());
        request.auth = Some(AuthData::Dummy(Dummy::new()));
        client.matrix_auth().register(request).await.unwrap();
        client
    }
}

struct Uplink {
    dir: tempfile::TempDir,
    account: MatrixAccountConfig,
    key: Arc<MasterKey>,
    dispatcher: Dispatcher,
    matrix: Matrix,
}

impl Uplink {
    fn new(homeserver: &Homeserver) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let key = Arc::new(MasterKey::open(&dir.path().join("master.key")).unwrap());
        let mut registry = Registry::new();
        uplink_matrix::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(1024)),
        );
        let account = MatrixAccountConfig {
            id: "mx".into(),
            homeserver: homeserver.url.clone(),
            mxid: "@alice:localhost".into(),
            history_members: 0,
            state_dir: dir.path().join("matrix/mx"),
        };
        let rooms = RoomsView::kept(
            &dispatcher,
            vec![ConfiguredAccount {
                id: account.id.clone(),
                mxid: account.mxid.clone(),
                history_members: account.history_members,
            }],
        )
        .unwrap();
        let matrix = Matrix::new(
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
            rooms,
            uplink_storage::store::Store::blobs(dir.path()),
        );
        Self {
            dir,
            account,
            key,
            dispatcher,
            matrix,
        }
    }

    async fn pair_and_sync(&self) -> tokio::task::JoinHandle<()> {
        client::pair(&self.account, &self.key, PASSWORD, "uplink")
            .await
            .unwrap();
        tokio::spawn(
            Sync::new(
                &self.account,
                self.matrix.clients(),
                self.key.clone(),
                self.dispatcher.clone(),
                self.dir.path(),
            )
            .run(),
        )
    }

    async fn room(&self, room_id: &RoomId) -> Option<Room> {
        match self.matrix.list_rooms().await.unwrap() {
            ServerResponse::Rooms { rooms, .. } => {
                rooms.into_iter().find(|r| r.room_id == room_id.as_str())
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn follows(&self) -> bool {
        match self.matrix.list_rooms().await.unwrap() {
            ServerResponse::Rooms { accounts, .. } => accounts[0].follows,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Of the stored message with this body, whether it notifies and
    /// highlights, and whether its thread, of a root, is followed.
    fn stored(&self, body: &str) -> Option<(bool, bool, bool)> {
        let conn = uplink_storage::open_reader(self.dir.path()).unwrap();
        conn.query_row(
            "SELECT e.notify, e.highlight,
                    EXISTS (SELECT 1 FROM matrix_thread_follow f
                            WHERE f.room = e.room AND f.root = e.event_id AND f.desired)
             FROM matrix_event e WHERE e.body = ?1",
            [body],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    }

    /// Whether nothing of the account's is left to reach the homeserver.
    fn reconciled(&self) -> bool {
        let mut conn = uplink_storage::open_reader(self.dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        uplink_matrix::outbox::outbox(&tx).unwrap().is_empty()
    }

    async fn send(&self, room: &Room, content: MatrixContent) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let key = format!(
            "key-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        self.matrix
            .send_message(room.id, key, content)
            .await
            .unwrap();
    }
}

async fn eventually<T, F: std::future::Future<Output = Option<T>>>(
    what: &str,
    mut f: impl FnMut() -> F,
) -> T {
    for _ in 0..600 {
        if let Some(t) = f().await {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn text(body: &str, thread: Option<&str>) -> MatrixContent {
    MatrixContent::Text {
        body: body.into(),
        format: TextFormat::Markdown,
        emote: false,
        reply_to: None,
        replaces: None,
        thread: thread.map(str::to_owned),
    }
}

/// Bob's message of the thread of `root`, mentioning alice if `mention`.
async fn reply(bob: &Client, room: &RoomId, root: &EventId, body: &str, mention: bool) {
    let mut content = json!({
        "msgtype": "m.text",
        "body": body,
        "m.relates_to": { "rel_type": "m.thread", "event_id": root,
                          "is_falling_back": true, "m.in_reply_to": { "event_id": root } },
    });
    if mention {
        content["m.mentions"] = json!({ "user_ids": ["@alice:localhost"] });
    }
    bob.get_room(room)
        .unwrap()
        .send_raw("m.room.message", content)
        .await
        .unwrap();
}

/// The Matrix id of the stored event with this body.
fn event_id(uplink: &Uplink, body: &str) -> OwnedEventId {
    let conn = uplink_storage::open_reader(uplink.dir.path()).unwrap();
    let id: String = conn
        .query_row(
            "SELECT event_id FROM matrix_event WHERE body = ?1",
            [body],
            |r| r.get(0),
        )
        .unwrap();
    id.try_into().unwrap()
}

/// As Slack's: a thread one starts by writing what is replied to, writes
/// in, or is mentioned in is followed, and notifies of its replies; one
/// not followed does not; unfollowing quiets it, the homeserver's push
/// rules agreeing.
#[tokio::test(flavor = "multi_thread")]
async fn threads_followed_notify_of_their_replies() {
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let uplink = Uplink::new(&homeserver);
    let _sync = uplink.pair_and_sync().await;

    let room = bob.create_room(Default::default()).await.unwrap();
    room.invite_user_by_id(alice_phone.user_id().unwrap())
        .await
        .unwrap();
    alice_phone
        .sync_once(SyncSettings::default())
        .await
        .unwrap();
    alice_phone.join_room_by_id(room.room_id()).await.unwrap();
    let room_id: OwnedRoomId = room.room_id().to_owned();
    let stored = eventually("uplink to see the room", || async {
        uplink
            .room(&room_id)
            .await
            .filter(|r| r.membership == Membership::Joined)
    })
    .await;
    assert!(uplink.follows().await, "Synapse keeps thread subscriptions");

    // Bob's thread, which alice does not follow: its replies do not
    // notify, though every message of a room of two does.
    let root = room
        .send_raw(
            "m.room.message",
            json!({ "msgtype": "m.text", "body": "bob's" }),
        )
        .await
        .unwrap()
        .response
        .event_id;
    reply(&bob, &room_id, &root, "not followed", false).await;
    let got = eventually("the reply", || async { uplink.stored("not followed") }).await;
    assert_eq!(got, (false, false, false));

    // A mention in it notifies, and follows it: what comes after does.
    reply(&bob, &room_id, &root, "alice, look", true).await;
    let got = eventually("the mention", || async { uplink.stored("alice, look") }).await;
    assert_eq!(got, (true, true, false));
    eventually("the thread followed", || async {
        uplink.stored("bob's").filter(|s| s.2)
    })
    .await;
    reply(&bob, &room_id, &root, "after the mention", false).await;
    let got = eventually("the next reply", || async {
        uplink.stored("after the mention")
    })
    .await;
    assert_eq!(got, (true, false, false));

    // What alice sends is the root of a thread she follows, so Bob's
    // reply to it notifies her, by the homeserver's rules too.
    uplink.send(&stored, text("alice's", None)).await;
    let mine = eventually("alice's message, followed", || async {
        uplink.stored("alice's").filter(|s| s.2)
    })
    .await;
    assert_eq!(mine, (false, false, true));
    eventually("the follow pushed and read back", || async {
        uplink.reconciled().then_some(())
    })
    .await;
    let alices = event_id(&uplink, "alice's");
    let phone_room = alice_phone.get_room(&room_id).unwrap();
    assert!(
        phone_room
            .fetch_thread_subscription(alices.clone())
            .await
            .unwrap()
            .is_some(),
        "subscribed on the homeserver"
    );
    reply(&bob, &room_id, &alices, "to her", false).await;
    let got = eventually("bob's reply to her", || async { uplink.stored("to her") }).await;
    assert_eq!(got, (true, false, false));

    // Unfollowed, it quiets: what was stored, and what comes.
    let ServerResponse::Dispatched { .. } = uplink
        .matrix
        .set_followed(stored.id, alices.to_string(), false)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(uplink.stored("to her"), Some((false, false, false)));
    eventually("the unfollow pushed and read back", || async {
        uplink.reconciled().then_some(())
    })
    .await;
    assert!(phone_room
        .fetch_thread_subscription(alices.clone())
        .await
        .unwrap()
        .is_none());
    reply(&bob, &room_id, &alices, "unheard", false).await;
    let got = eventually("the reply unfollowed", || async {
        uplink.stored("unheard")
    })
    .await;
    assert_eq!(got, (false, false, false));

    // Writing in a thread follows it.
    let other = room
        .send_raw(
            "m.room.message",
            json!({ "msgtype": "m.text", "body": "another" }),
        )
        .await
        .unwrap()
        .response
        .event_id;
    eventually("the other root", || async { uplink.stored("another") }).await;
    uplink
        .send(&stored, text("me too", Some(other.as_str())))
        .await;
    eventually("the thread written in, followed", || async {
        uplink.stored("another").filter(|s| s.2)
    })
    .await;
    eventually("the follow pushed and read back", || async {
        uplink.reconciled().then_some(())
    })
    .await;
    reply(&bob, &room_id, &other, "in reply", false).await;
    let got = eventually("the reply in it", || async { uplink.stored("in reply") }).await;
    assert_eq!(got, (true, false, false));
}

/// The stored id of the event with this body.
fn stored_id(uplink: &Uplink, body: &str) -> uplink_protocol::matrix::RoomEventId {
    let conn = uplink_storage::open_reader(uplink.dir.path()).unwrap();
    uplink_protocol::matrix::RoomEventId(
        conn.query_row("SELECT id FROM matrix_event WHERE body = ?1", [body], |r| {
            r.get(0)
        })
        .unwrap(),
    )
}

/// Reading a thread marks its replies read, by a threaded receipt
/// (MSC3771) the account's other clients see; and theirs read it here.
#[tokio::test(flavor = "multi_thread")]
async fn threads_are_read_by_threaded_receipts_both_ways() {
    use matrix_sdk::ruma::{
        api::client::receipt::create_receipt::v3::ReceiptType as SendReceipt,
        events::receipt::{ReceiptThread, ReceiptType},
    };
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let uplink = Uplink::new(&homeserver);
    let _sync = uplink.pair_and_sync().await;

    let room = bob.create_room(Default::default()).await.unwrap();
    room.invite_user_by_id(alice_phone.user_id().unwrap())
        .await
        .unwrap();
    alice_phone
        .sync_once(SyncSettings::default())
        .await
        .unwrap();
    alice_phone.join_room_by_id(room.room_id()).await.unwrap();
    let room_id: OwnedRoomId = room.room_id().to_owned();
    let stored = eventually("uplink to see the room", || async {
        uplink
            .room(&room_id)
            .await
            .filter(|r| r.membership == Membership::Joined)
    })
    .await;
    let root = room
        .send_raw(
            "m.room.message",
            json!({ "msgtype": "m.text", "body": "the root" }),
        )
        .await
        .unwrap()
        .response
        .event_id;
    eventually("the root", || async { uplink.stored("the root") }).await;
    let ServerResponse::Dispatched { .. } = uplink
        .matrix
        .set_followed(stored.id, root.to_string(), true)
        .await
        .unwrap()
    else {
        panic!()
    };
    // Read up to the root, from another client: the replies after it
    // are the thread's to read.
    alice_phone
        .sync_once(SyncSettings::default().timeout(Duration::ZERO))
        .await
        .unwrap();
    let phone_room = alice_phone.get_room(&room_id).unwrap();
    phone_room
        .send_single_receipt(SendReceipt::Read, ReceiptThread::Unthreaded, root.clone())
        .await
        .unwrap();
    reply(&bob, &room_id, &root, "first", false).await;
    reply(&bob, &room_id, &root, "second", false).await;
    eventually("the replies unread", || async {
        uplink
            .room(&room_id)
            .await
            .filter(|r| r.unread == 2 && uplink.stored("second").is_some())
    })
    .await;

    // Read here: a threaded receipt the phone sees.
    let (root_id, second) = (stored_id(&uplink, "the root"), stored_id(&uplink, "second"));
    uplink
        .matrix
        .mark_thread_read(stored.id, root_id, second, true)
        .await
        .unwrap();
    assert_eq!(uplink.room(&room_id).await.unwrap().unread, 0);
    let second_id = event_id(&uplink, "second");
    eventually("the phone to see the threaded receipt", || async {
        alice_phone
            .sync_once(SyncSettings::default().timeout(Duration::ZERO))
            .await
            .unwrap();
        phone_room
            .load_user_receipt(
                ReceiptType::Read,
                &ReceiptThread::Thread(root.clone()),
                alice_phone.user_id().unwrap(),
            )
            .await
            .unwrap()
            .filter(|(event, _)| *event == second_id)
    })
    .await;

    // Read on the phone: read here too.
    reply(&bob, &room_id, &root, "third", false).await;
    eventually("the third unread", || async {
        uplink.room(&room_id).await.filter(|r| r.unread == 1)
    })
    .await;
    let third = event_id(&uplink, "third");
    phone_room
        .send_single_receipt(
            SendReceipt::Read,
            ReceiptThread::Thread(root.clone()),
            third,
        )
        .await
        .unwrap();
    eventually("the phone's threaded receipt to read it", || async {
        uplink.room(&room_id).await.filter(|r| r.unread == 0)
    })
    .await;
    let ServerResponse::Thread { read_up_to, .. } =
        uplink.matrix.list_thread(stored.id, root_id).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(
        read_up_to.map(|r| r.event),
        Some(stored_id(&uplink, "third"))
    );
    assert_eq!(
        uplink
            .room(&room_id)
            .await
            .unwrap()
            .read_up_to
            .map(|r| r.event),
        Some(root_id),
        "the room's own marker is where it was"
    );
}
