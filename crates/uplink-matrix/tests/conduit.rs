//! Matrix sync and sending against a real homeserver: Conduit
//! (`UPLINK_CONDUIT`, set by the dev shell and `nix flake check`), run
//! for each test. Another matrix-sdk client plays the other side.

use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        api::client::{
            account::register::v3::Request as Register,
            uiaa::{AuthData, Dummy},
        },
        events::room::message::OriginalSyncRoomMessageEvent,
        OwnedRoomId, RoomId,
    },
    Client,
};
use serde_json::json;
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use uplink_config::MatrixAccountConfig;
use uplink_matrix::{client, service::ConfiguredAccount, service::Matrix, sync::Sync};
use uplink_protocol::{
    matrix::{Encryption, Membership, Room, RoomEvent, TimelinePage},
    ServerResponse,
};
use uplink_realtime::Hub;
use uplink_storage::seal::MasterKey;
use uplink_tx_log::{Dispatcher, Registry};

const PASSWORD: &str = "correct horse battery staple";

struct Homeserver {
    url: String,
    _conduit: tokio::process::Child,
}

impl Homeserver {
    async fn start(dir: &Path) -> Self {
        let conduit = std::env::var_os("UPLINK_CONDUIT")
            .expect("UPLINK_CONDUIT must name the conduit binary (the dev shell sets it)");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let db = dir.join("conduit");
        std::fs::create_dir(&db).unwrap();
        let config = dir.join("conduit.toml");
        std::fs::write(
            &config,
            format!(
                "[global]\nserver_name = \"localhost\"\ndatabase_backend = \"sqlite\"\n\
                 database_path = \"{}\"\naddress = \"127.0.0.1\"\nport = {port}\n\
                 allow_registration = true\nallow_federation = false\n\
                 allow_check_for_updates = false\ntrusted_servers = []\n",
                db.display()
            ),
        )
        .unwrap();
        let log = dir.join("conduit.log");
        let child = tokio::process::Command::new(conduit)
            .env("CONDUIT_CONFIG", &config)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(std::fs::File::create(dir.join("conduit.err")).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        for _ in 0..300 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                let homeserver = Self {
                    url: format!("http://127.0.0.1:{port}"),
                    _conduit: child,
                };
                // Conduit makes its first user admin, with an admin room.
                homeserver.register(dir, "admin").await;
                return homeserver;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "conduit did not start:\n{}{}",
            std::fs::read_to_string(&log).unwrap(),
            std::fs::read_to_string(dir.join("conduit.err")).unwrap()
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
        let key_path = dir.path().join("master.key");
        std::fs::write(&key_path, MasterKey::generate()).unwrap();
        let key = Arc::new(MasterKey::load(&key_path).unwrap());
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
            state_dir: dir.path().join("matrix/mx"),
        };
        let matrix = Matrix::new(
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
            vec![ConfiguredAccount {
                id: account.id.clone(),
                mxid: account.mxid.clone(),
            }],
        );
        Self {
            dir,
            account,
            key,
            dispatcher,
            matrix,
        }
    }

    /// Pairs the account, as `uplink matrix pair` does, and starts syncing.
    async fn pair_and_sync(&self) -> tokio::task::JoinHandle<()> {
        client::login(&self.account, &self.key, PASSWORD, "uplink")
            .await
            .unwrap();
        self.start_sync()
    }

    fn start_sync(&self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(
            Sync::new(
                &self.account,
                self.key.clone(),
                self.dispatcher.clone(),
                self.dir.path(),
            )
            .run(),
        )
    }

    async fn rooms(&self) -> Vec<Room> {
        match self.matrix.list_rooms().await.unwrap() {
            ServerResponse::Rooms { rooms, .. } => rooms,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn room(&self, room_id: &RoomId) -> Option<Room> {
        self.rooms()
            .await
            .into_iter()
            .find(|r| r.room_id == room_id.as_str())
    }

    async fn timeline(&self, room: &Room) -> Vec<RoomEvent> {
        match self
            .matrix
            .list_timeline(room.id, TimelinePage::Latest, 100)
            .await
            .unwrap()
        {
            ServerResponse::Timeline { events, .. } => events,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn outbox_len(&self) -> usize {
        match self.matrix.list_outbox().await.unwrap() {
            ServerResponse::MatrixOutbox { items, .. } => items.len(),
            other => panic!("unexpected {other:?}"),
        }
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

/// Syncs `client` in the background, passing on the bodies of the
/// messages it receives.
fn sync_messages(client: &Client) -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    client.add_event_handler(move |event: OriginalSyncRoomMessageEvent| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(event.content.body().to_owned());
        }
    });
    let client = client.clone();
    tokio::spawn(async move { client.sync(SyncSettings::default()).await });
    rx
}

/// Bob's encrypted room, with alice joined from another device.
async fn shared_room(bob: &Client, alice_phone: &Client) -> OwnedRoomId {
    let room = bob.create_room(Default::default()).await.unwrap();
    room.enable_encryption().await.unwrap();
    room.invite_user_by_id(alice_phone.user_id().unwrap())
        .await
        .unwrap();
    alice_phone
        .sync_once(SyncSettings::default())
        .await
        .unwrap();
    alice_phone.join_room_by_id(room.room_id()).await.unwrap();
    // Bob sees alice join, and so encrypts to her devices.
    eventually("bob to see alice join", || async {
        (room.joined_members_count() == 2).then_some(())
    })
    .await;
    room.room_id().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_both_ways_in_an_encrypted_room() {
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let mut bob_received = sync_messages(&bob);

    let uplink = Uplink::new(&homeserver);
    let _sync = uplink.pair_and_sync().await;
    let room_id = shared_room(&bob, &alice_phone).await;

    let room = eventually("uplink to see the room", || async {
        uplink
            .room(&room_id)
            .await
            .filter(|r| r.membership == Membership::Joined && r.encrypted)
    })
    .await;

    // From the other client to uplink, decrypted.
    let bob_room = bob.get_room(&room_id).unwrap();
    bob_room
        .send(
            matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(
                "hello from bob",
            ),
        )
        .await
        .unwrap();
    let received = eventually("uplink to receive bob's message", || async {
        uplink
            .timeline(&room)
            .await
            .into_iter()
            .find(|e| e.body.as_deref() == Some("hello from bob"))
    })
    .await;
    assert_eq!(received.sender, "@bob:localhost");
    assert_eq!(received.msgtype.as_deref(), Some("m.text"));
    assert_eq!(received.encryption, Encryption::Decrypted);
    let room = uplink.room(&room_id).await.unwrap();
    assert_eq!(room.unread, 1);

    // From uplink to the other client.
    uplink
        .matrix
        .send_message(room.id, "hello from uplink".into())
        .await
        .unwrap();
    let body = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let body = bob_received.recv().await.unwrap();
            if body != "hello from bob" {
                return body;
            }
        }
    })
    .await
    .expect("bob receives uplink's message");
    assert_eq!(body, "hello from uplink");
    // Once synced back, the sent message is in the timeline, not the outbox.
    eventually("the sent message to sync back", || async {
        (uplink.outbox_len().await == 0).then_some(())
    })
    .await;
    let own = uplink
        .timeline(&room)
        .await
        .into_iter()
        .find(|e| e.body.as_deref() == Some("hello from uplink"))
        .expect("the sent message is in the timeline");
    assert_eq!(own.sender, "@alice:localhost");
    assert_eq!(
        uplink.room(&room_id).await.unwrap().unread,
        1,
        "own messages are not unread"
    );

    // An event type uplink does not know, stored as it came.
    bob_room
        .send_raw("org.example.custom", json!({ "answer": 42 }))
        .await
        .unwrap();
    let custom = eventually("uplink to receive the custom event", || async {
        uplink
            .timeline(&room)
            .await
            .into_iter()
            .find(|e| e.kind == "org.example.custom")
    })
    .await;
    assert_eq!(custom.body, None);
    let raw: serde_json::Value = serde_json::from_str(&custom.raw).unwrap();
    assert_eq!(raw["content"]["answer"], 42);

    // Reading in uplink sends a receipt the other device sees.
    uplink.matrix.mark_read(room.id, custom.id).await.unwrap();
    assert_eq!(uplink.room(&room_id).await.unwrap().unread, 0);
    let phone_room = alice_phone.get_room(&room_id).unwrap();
    eventually("alice's other device to see the receipt", || async {
        alice_phone
            .sync_once(SyncSettings::default().timeout(Duration::ZERO))
            .await
            .unwrap();
        let receipt = phone_room
            .load_user_receipt(
                matrix_sdk::ruma::events::receipt::ReceiptType::Read,
                &matrix_sdk::ruma::events::receipt::ReceiptThread::Unthreaded,
                alice_phone.user_id().unwrap(),
            )
            .await
            .unwrap();
        receipt
            .filter(|(event_id, _)| event_id.as_str() == custom.event_id)
            .map(drop)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_on_another_device_reads_in_uplink() {
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let _bob_received = sync_messages(&bob);
    let uplink = Uplink::new(&homeserver);
    let _sync = uplink.pair_and_sync().await;
    let room_id = shared_room(&bob, &alice_phone).await;
    eventually("uplink to see the room", || async {
        uplink.room(&room_id).await
    })
    .await;

    let bob_room = bob.get_room(&room_id).unwrap();
    let sent = bob_room
        .send(
            matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(
                "read me elsewhere",
            ),
        )
        .await
        .unwrap();
    eventually("the message to be unread in uplink", || async {
        uplink.room(&room_id).await.filter(|r| r.unread == 1)
    })
    .await;

    alice_phone
        .sync_once(SyncSettings::default())
        .await
        .unwrap();
    alice_phone
        .get_room(&room_id)
        .unwrap()
        .send_single_receipt(
            matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType::Read,
            matrix_sdk::ruma::events::receipt::ReceiptThread::Unthreaded,
            sent.response.event_id,
        )
        .await
        .unwrap();
    eventually("the message to be read in uplink", || async {
        uplink.room(&room_id).await.filter(|r| r.unread == 0)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn restarting_resumes_from_the_recorded_sync() {
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let _bob_received = sync_messages(&bob);
    let uplink = Uplink::new(&homeserver);
    let sync = uplink.pair_and_sync().await;
    let room_id = shared_room(&bob, &alice_phone).await;
    let room = eventually("uplink to see the room", || async {
        uplink.room(&room_id).await
    })
    .await;
    let before = uplink.timeline(&room).await.len();

    // As after a restart: the new task restores the sealed session and
    // resumes rather than syncing from scratch.
    sync.abort();
    assert!(sync.await.unwrap_err().is_cancelled());
    uplink.start_sync();
    bob.get_room(&room_id)
        .unwrap()
        .send(
            matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(
                "after the restart",
            ),
        )
        .await
        .unwrap();
    let events = eventually("uplink to receive the message", || async {
        let events = uplink.timeline(&room).await;
        events
            .iter()
            .any(|e| e.body.as_deref() == Some("after the restart"))
            .then_some(events)
    })
    .await;
    assert_eq!(events.len(), before + 1, "no event is stored twice");
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_with_the_recovery_key_verifies_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    // The other device has set up cross-signing and Secret Storage.
    alice_phone
        .sync_once(SyncSettings::default())
        .await
        .unwrap();
    let mut password = matrix_sdk::ruma::api::client::uiaa::Password::new(
        matrix_sdk::ruma::api::client::uiaa::UserIdentifier::Matrix(
            matrix_sdk::ruma::api::client::uiaa::MatrixUserIdentifier::new("alice".into()),
        ),
        PASSWORD.to_owned(),
    );
    let session = match alice_phone.encryption().bootstrap_cross_signing(None).await {
        Ok(()) => None,
        Err(e) => Some(
            e.as_uiaa_response()
                .expect("bootstrapping asks for authentication")
                .session
                .clone(),
        ),
    };
    if let Some(session) = session {
        password.session = session;
        alice_phone
            .encryption()
            .bootstrap_cross_signing(Some(AuthData::Password(password)))
            .await
            .unwrap();
    }
    let recovery_key = alice_phone.encryption().recovery().enable().await.unwrap();

    let uplink = Uplink::new(&homeserver);
    let client = client::login(&uplink.account, &uplink.key, PASSWORD, "uplink")
        .await
        .unwrap();
    client::upload_keys(&client).await.unwrap();
    assert!(client::import_secrets(&client, &recovery_key)
        .await
        .unwrap());

    // The other device trusts it.
    let device_id = client.device_id().unwrap().to_owned();
    eventually(
        "the other device to see uplink's device verified",
        || async {
            alice_phone
                .sync_once(SyncSettings::default().timeout(Duration::ZERO))
                .await
                .unwrap();
            alice_phone
                .encryption()
                .get_device(alice_phone.user_id().unwrap(), &device_id)
                .await
                .unwrap()
                .filter(|device| device.is_verified())
        },
    )
    .await;
}
