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
        let key = Arc::new(MasterKey::open(&key_path).unwrap());
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

    /// Pairs the account, as `uplink matrix pair` does, and starts syncing.
    async fn pair_and_sync(&self) -> tokio::task::JoinHandle<()> {
        client::pair(&self.account, &self.key, PASSWORD, "uplink")
            .await
            .unwrap();
        self.start_sync()
    }

    fn start_sync(&self) -> tokio::task::JoinHandle<()> {
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

    /// How many of the room's sends are not yet reconciled.
    async fn sends(&self, room: &Room) -> usize {
        match self.matrix.list_sends(room.id).await.unwrap() {
            ServerResponse::MatrixSends { sends, .. } => sends.len(),
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
    // A message in a room of two notifies, by the default push rules,
    // which the client evaluates once it decrypts it.
    assert_eq!((room.notifying, room.highlighted), (1, 0));

    // From uplink to the other client.
    uplink
        .matrix
        .send_message(room.id, text("hello from uplink"))
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
    // Once synced back, the sent message is in the timeline, and its
    // send reconciled.
    eventually("the sent message to sync back", || async {
        (uplink.sends(&room).await == 0).then_some(())
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

/// Sets up cross-signing for alice, from `client`.
async fn bootstrap_cross_signing(client: &Client) {
    let mut password = matrix_sdk::ruma::api::client::uiaa::Password::new(
        matrix_sdk::ruma::api::client::uiaa::UserIdentifier::Matrix(
            matrix_sdk::ruma::api::client::uiaa::MatrixUserIdentifier::new("alice".into()),
        ),
        PASSWORD.to_owned(),
    );
    let session = match client.encryption().bootstrap_cross_signing(None).await {
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
        client
            .encryption()
            .bootstrap_cross_signing(Some(AuthData::Password(password)))
            .await
            .unwrap();
    }
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
    bootstrap_cross_signing(&alice_phone).await;
    let recovery_key = alice_phone.encryption().recovery().enable().await.unwrap();

    let uplink = Uplink::new(&homeserver);
    let client = client::pair(&uplink.account, &uplink.key, PASSWORD, "uplink")
        .await
        .unwrap();
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

#[tokio::test(flavor = "multi_thread")]
async fn invitations_are_joined_or_declined_and_undone() {
    use matrix_sdk::ruma::api::client::room::{create_room::v3::Request as CreateRoom, Visibility};
    use uplink_protocol::matrix::MembershipChange;

    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    homeserver.register(dir.path(), "alice").await;
    let _bob_received = sync_messages(&bob);
    let uplink = Uplink::new(&homeserver);
    let _sync = uplink.pair_and_sync().await;
    let alice = matrix_sdk::ruma::user_id!("@alice:localhost");
    let invite = |name: &'static str, public: bool| {
        let bob = bob.clone();
        async move {
            let mut request = CreateRoom::new();
            request.name = Some(name.into());
            request.invite = vec![alice.to_owned()];
            if public {
                request.visibility = Visibility::Public;
                request.preset = Some(
                    matrix_sdk::ruma::api::client::room::create_room::v3::RoomPreset::PublicChat,
                );
            }
            bob.create_room(request).await.unwrap().room_id().to_owned()
        }
    };
    let change = |room, change| {
        let matrix = uplink.matrix.clone();
        async move {
            match matrix.change_membership(room, change).await.unwrap() {
                ServerResponse::Dispatched { tx_log_id } => tx_log_id,
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    let membership = |room_id: OwnedRoomId, wanted: Option<Membership>| {
        let uplink = &uplink;
        async move {
            eventually(&format!("{room_id} to be {wanted:?}"), || async {
                let room = uplink.room(&room_id).await;
                (room.as_ref().map(|r| (r.membership, r.pending)) == wanted.map(|m| (m, None)))
                    .then_some(room)
            })
            .await
        }
    };

    let lounge = invite("Lounge", true).await;
    let room = membership(lounge.clone(), Some(Membership::Invited))
        .await
        .unwrap();
    let join = change(room.id, MembershipChange::Join).await;
    membership(lounge.clone(), Some(Membership::Joined)).await;
    let bob_room = bob.get_room(&lounge).unwrap();
    eventually("bob to see alice join", || async {
        (bob_room.joined_members_count() == 2).then_some(())
    })
    .await;
    bob_room
        .send(
            matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain("welcome"),
        )
        .await
        .unwrap();
    eventually("the message in the joined room", || async {
        uplink
            .timeline(&room)
            .await
            .iter()
            .any(|e| e.body.as_deref() == Some("welcome"))
            .then_some(())
    })
    .await;

    // Undoing the join leaves; undoing that joins again.
    let leave = uplink.dispatcher.undo(join).await.unwrap();
    membership(lounge.clone(), None).await;
    uplink.dispatcher.undo(leave).await.unwrap();
    membership(lounge.clone(), Some(Membership::Joined)).await;

    let private = invite("Private", false).await;
    let room = membership(private.clone(), Some(Membership::Invited))
        .await
        .unwrap();
    change(room.id, MembershipChange::Leave).await;
    membership(private, None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_pairing_leaves_nothing_behind_and_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let mut uplink = Uplink::new(&homeserver);

    // The homeserver logs in a name other than the configured one, as it
    // does an mxid in another case: the session is not of the account.
    uplink.account.mxid = "alice".into();
    let error = client::pair(&uplink.account, &uplink.key, PASSWORD, "uplink")
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("not the configured alice"),
        "{error:#}"
    );
    assert!(!uplink.account.state_dir.exists(), "the stores are removed");
    let devices = alice_phone.devices().await.unwrap().devices;
    assert_eq!(
        devices.len(),
        1,
        "only the phone's device is left: {devices:?}"
    );

    uplink.account.mxid = "@alice:localhost".into();
    client::pair(&uplink.account, &uplink.key, PASSWORD, "uplink")
        .await
        .unwrap();
}

/// An 8×8 PNG.
const PNG: [u8; 74] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x02, 0x00, 0x00, 0x00, 0x4b, 0x6d, 0x29,
    0xdc, 0x00, 0x00, 0x00, 0x11, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x38, 0x63, 0x6c, 0x8c,
    0x15, 0x31, 0x0c, 0x2d, 0x09, 0x00, 0x0a, 0xca, 0x4c, 0x81, 0xd5, 0x4f, 0xd8, 0x6e, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[tokio::test(flavor = "multi_thread")]
async fn spaces_list_their_rooms_and_avatars_are_served() {
    use matrix_sdk::ruma::{
        api::client::room::create_room::v3::{CreationContent, Request as CreateRoom},
        serde::Raw,
    };

    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let create = |space: bool, name: &'static str| {
        let alice_phone = alice_phone.clone();
        async move {
            let mut request = CreateRoom::new();
            request.name = Some(name.into());
            if space {
                request.creation_content = Some(
                    Raw::<CreationContent>::from_json_string(r#"{"type":"m.space"}"#.into())
                        .unwrap(),
                );
            }
            alice_phone.create_room(request).await.unwrap()
        }
    };
    let space = create(true, "Space").await;
    let first = create(false, "First").await;
    let avatar = alice_phone
        .media()
        .upload(&"image/png".parse().unwrap(), PNG.to_vec(), None)
        .await
        .unwrap()
        .content_uri;
    space
        .send_state_event_raw("m.room.avatar", "", json!({ "url": avatar }))
        .await
        .unwrap();
    let list = |child: &matrix_sdk::Room, order: &'static str| {
        let space = space.clone();
        let child = child.room_id().to_owned();
        async move {
            space
                .send_state_event_raw(
                    "m.space.child",
                    child.as_str(),
                    json!({ "via": ["localhost"], "order": order }),
                )
                .await
                .unwrap();
        }
    };
    list(&first, "b").await;

    // Paired after: the first sync ends where pairing synced to.
    let uplink = Uplink::new(&homeserver);
    let _sync = uplink.pair_and_sync().await;
    let synced = eventually("uplink to see the space", || async {
        uplink
            .room(space.room_id())
            .await
            .filter(|r| r.space && r.avatar.is_some() && !r.children.is_empty())
    })
    .await;
    assert_eq!(synced.children, vec![first.room_id().to_string()]);
    assert_eq!(synced.avatar.as_deref(), Some(avatar.as_str()));
    assert!(!uplink.room(first.room_id()).await.unwrap().space);

    // A room listed later, before the first by its order.
    let second = create(false, "Second").await;
    list(&second, "a").await;
    eventually("uplink to see the second child", || async {
        let children = uplink.room(space.room_id()).await?.children;
        (children == vec![second.room_id().to_string(), first.room_id().to_string()]).then_some(())
    })
    .await;

    let (mime, image) = uplink
        .matrix
        .avatar(synced.id, avatar.to_string())
        .await
        .unwrap();
    assert_eq!(mime, "image/png");
    assert!(!image.is_empty());
    let other = uplink
        .matrix
        .avatar(synced.id, "mxc://localhost/other".into())
        .await
        .unwrap_err();
    assert_eq!(other.code, uplink_protocol::ErrorCode::NotFound);
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

/// Replies, edits, reactions, redactions and files, sent by uplink to an
/// encrypted room, come back folded into the events they are of.
#[tokio::test(flavor = "multi_thread")]
async fn replies_edits_reactions_redactions_and_files() {
    use uplink_protocol::matrix::MatrixContent;
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let _bob_received = sync_messages(&bob);
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
    bob.get_room(&room_id)
        .unwrap()
        .send(matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain("hello"))
        .await
        .unwrap();
    let hello = eventually("uplink to receive bob's message", || async {
        uplink
            .timeline(&room)
            .await
            .into_iter()
            .find(|e| e.body.as_deref() == Some("hello"))
    })
    .await;

    // Sends `content`, and waits for it to sync back; returns the event.
    let send = |content: MatrixContent| {
        let uplink = &uplink;
        let room = &room;
        async move {
            uplink.matrix.send_message(room.id, content).await.unwrap();
            eventually("the send to sync back", || async {
                (uplink.sends(room).await == 0).then_some(())
            })
            .await;
            uplink.timeline(room).await
        }
    };
    let find = |timeline: &[RoomEvent], event_id: &str| {
        timeline
            .iter()
            .find(|e| e.event_id == event_id)
            .unwrap()
            .clone()
    };

    // A reply, in Markdown.
    let timeline = send(MatrixContent::Text {
        body: "**hi** bob".into(),
        markdown: true,
        emote: false,
        reply_to: Some(hello.event_id.clone()),
        replaces: None,
        thread: None,
    })
    .await;
    let reply = timeline.last().unwrap().clone();
    assert_eq!(reply.body.as_deref(), Some("**hi** bob"));
    assert_eq!(reply.replied.as_ref().unwrap().event_id, hello.event_id);
    let raw: serde_json::Value = serde_json::from_str(&reply.raw).unwrap();
    assert_eq!(raw["content"]["formatted_body"], "<strong>hi</strong> bob");

    // Its edit.
    let timeline = send(MatrixContent::Text {
        body: "hi again".into(),
        markdown: true,
        emote: false,
        reply_to: None,
        replaces: Some(reply.event_id.clone()),
        thread: None,
    })
    .await;
    let edited = find(&timeline, &reply.event_id);
    let content: serde_json::Value =
        serde_json::from_str(&edited.edit.as_ref().unwrap().content).unwrap();
    assert_eq!(content["body"], "hi again");
    assert_eq!(timeline.last().unwrap().event_id, reply.event_id, "folded");

    // Into a thread on the reply, twice: the second answers the first by
    // fallback.
    for body in ["threaded", "threaded again"] {
        send(MatrixContent::Text {
            body: body.into(),
            markdown: true,
            emote: false,
            reply_to: None,
            replaces: None,
            thread: Some(reply.event_id.clone()),
        })
        .await;
    }
    let timeline = uplink.timeline(&room).await;
    let root = find(&timeline, &reply.event_id);
    let summary = root.thread.as_ref().unwrap();
    assert_eq!(summary.replies, 2);
    assert_eq!(summary.latest.body.as_deref(), Some("threaded again"));
    let uplink_protocol::ServerResponse::Thread { replies, .. } =
        uplink.matrix.list_thread(room.id, root.id).await.unwrap()
    else {
        panic!()
    };
    let raw: serde_json::Value = serde_json::from_str(&replies[1].raw).unwrap();
    assert_eq!(
        raw["content"]["m.relates_to"],
        serde_json::json!({ "rel_type": "m.thread", "event_id": reply.event_id,
                            "is_falling_back": true,
                            "m.in_reply_to": { "event_id": replies[0].event_id } })
    );
    assert!(!timeline.iter().any(|e| e.thread_root.is_some()));

    // A reaction, and taking it back.
    let timeline = send(MatrixContent::Reaction {
        event: hello.event_id.clone(),
        key: "👍".into(),
    })
    .await;
    let reactions = find(&timeline, &hello.event_id).reactions;
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].key, "👍");
    let [reactor] = &reactions[0].senders[..] else {
        panic!()
    };
    assert_eq!(reactor.sender, "@alice:localhost");
    let timeline = send(MatrixContent::Redaction {
        event: reactor.event_id.clone(),
    })
    .await;
    assert!(find(&timeline, &hello.event_id).reactions.is_empty());

    // A file, encrypted as the room is.
    let png = b"\x89PNG\r\n\x1a\nnot really".to_vec();
    let blob = uplink_storage::store::Store::blobs(uplink.dir.path())
        .put(&png)
        .unwrap();
    let timeline = send(MatrixContent::File {
        blob: uplink_protocol::compose::BlobId(blob.0),
        name: "dot.png".into(),
        mime: "image/png".into(),
        reply_to: None,
    })
    .await;
    let image = timeline.last().unwrap();
    assert_eq!(image.msgtype.as_deref(), Some("m.image"));
    assert_eq!(image.body.as_deref(), Some("dot.png"));
    let raw: serde_json::Value = serde_json::from_str(&image.raw).unwrap();
    let mxc = raw["content"]["file"]["url"].as_str().unwrap().to_owned();
    // Fetched for the web client, decrypted.
    let media = uplink
        .matrix
        .media(room.id, image.id, mxc.clone(), false)
        .await
        .unwrap();
    assert_eq!(media.data, png);
    assert_eq!(
        (media.mime.as_str(), media.name.as_deref(), media.inline),
        ("image/png", Some("dot.png"), true)
    );
    let refused = uplink
        .matrix
        .media(room.id, image.id, mxc, true)
        .await
        .err()
        .unwrap();
    assert_eq!(refused.code, uplink_protocol::ErrorCode::Invalid);
    let refused = uplink
        .matrix
        .media(room.id, image.id, "mxc://localhost/other".into(), false)
        .await
        .err()
        .unwrap();
    assert_eq!(refused.code, uplink_protocol::ErrorCode::NotFound);

    // Deleting the reply.
    let timeline = send(MatrixContent::Redaction {
        event: reply.event_id.clone(),
    })
    .await;
    let deleted = find(&timeline, &reply.event_id);
    assert!(deleted.redacted);
    assert_eq!(deleted.edit, None, "a redacted message shows no edit");
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_and_receipts_are_noticed() {
    use uplink_protocol::Notice;
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let _bob_received = sync_messages(&bob);
    let uplink = Uplink::new(&homeserver);
    let mut notices = uplink.dispatcher.hub().notices();
    let _sync = uplink.pair_and_sync().await;
    let room_id = shared_room(&bob, &alice_phone).await;
    let room = eventually("uplink to see the room", || async {
        uplink
            .room(&room_id)
            .await
            .filter(|r| r.membership == Membership::Joined)
    })
    .await;
    let bob_room = bob.get_room(&room_id).unwrap();

    // Others typing; uplink's own typing is not listed.
    uplink.matrix.set_typing(room.id, true).await.unwrap();
    bob_room.typing_notice(true).await.unwrap();
    let typing = loop {
        match &*tokio::time::timeout(Duration::from_secs(30), notices.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Notice::Typing(typing) if !typing.users.is_empty() => break typing.clone(),
            _ => {}
        }
    };
    assert_eq!(typing.room, room.id);
    assert_eq!(
        typing
            .users
            .iter()
            .map(|u| u.user.as_str())
            .collect::<Vec<_>>(),
        ["@bob:localhost"]
    );

    // Bob reads uplink's message.
    uplink
        .matrix
        .send_message(room.id, text("read me"))
        .await
        .unwrap();
    let sent = eventually("the message to sync back", || async {
        uplink
            .timeline(&room)
            .await
            .into_iter()
            .find(|e| e.body.as_deref() == Some("read me"))
    })
    .await;
    bob_room
        .send_single_receipt(
            matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType::Read,
            matrix_sdk::ruma::events::receipt::ReceiptThread::Unthreaded,
            sent.event_id.parse().unwrap(),
        )
        .await
        .unwrap();
    loop {
        let notice = tokio::time::timeout(Duration::from_secs(30), notices.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(&*notice, Notice::ReceiptsChanged(r) if r.room == room.id) {
            break;
        }
    }
    let receipts = match uplink.matrix.list_receipts(room.id).await.unwrap() {
        ServerResponse::Receipts { receipts, .. } => receipts,
        other => panic!("unexpected {other:?}"),
    };
    let bob_read = receipts
        .iter()
        .find(|r| r.member.user == "@bob:localhost")
        .unwrap();
    assert_eq!(bob_read.event, sent.id);
    assert!(receipts.iter().all(|r| r.member.user != "@alice:localhost"));
}

#[tokio::test(flavor = "multi_thread")]
async fn events_undecryptable_are_decrypted_once_their_keys_are_imported() {
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let _bob_received = sync_messages(&bob);
    let room_id = shared_room(&bob, &alice_phone).await;
    // Before uplink's device exists: it gets no key.
    bob.get_room(&room_id)
        .unwrap()
        .send(
            matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(
                "before uplink",
            ),
        )
        .await
        .unwrap();

    let uplink = Uplink::new(&homeserver);
    let _sync = uplink.pair_and_sync().await;
    let room = eventually("uplink to see the room", || async {
        uplink
            .room(&room_id)
            .await
            .filter(|r| r.membership == Membership::Joined)
    })
    .await;
    let undecryptable = eventually("the message, undecryptable", || async {
        uplink
            .timeline(&room)
            .await
            .into_iter()
            .find(|e| e.encryption == Encryption::Undecryptable)
    })
    .await;
    assert_eq!(undecryptable.sender, "@bob:localhost");

    let security = match uplink.matrix.get_security("mx").await.unwrap() {
        ServerResponse::MatrixSecurity { security } => security,
        other => panic!("unexpected {other:?}"),
    };
    let current: Vec<_> = security.devices.iter().filter(|d| d.current).collect();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].device_id, security.device_id);
    assert_eq!(security.devices.len(), 2, "uplink's and the phone's");

    // The phone has the key: it exports it, and uplink imports it.
    let mut phone_received = sync_messages(&alice_phone);
    assert_eq!(phone_received.recv().await.unwrap(), "before uplink");
    let keys = dir.path().join("keys.txt");
    alice_phone
        .encryption()
        .export_room_keys(keys.clone(), "export passphrase", |_| true)
        .await
        .unwrap();
    let export = std::fs::read(dir.path().join("keys.txt")).unwrap();
    let blob = uplink_storage::store::Store::blobs(uplink.dir.path())
        .put(&export)
        .unwrap();
    let blob = uplink_protocol::compose::BlobId(blob.0);
    assert!(uplink
        .matrix
        .import_room_keys("mx", &blob, "wrong passphrase")
        .await
        .is_err());
    match uplink
        .matrix
        .import_room_keys("mx", &blob, "export passphrase")
        .await
        .unwrap()
    {
        ServerResponse::RoomKeysImported { imported, total } => {
            assert!(imported >= 1);
            assert!(total >= imported);
        }
        other => panic!("unexpected {other:?}"),
    }
    let decrypted = eventually("the message, decrypted", || async {
        uplink
            .timeline(&room)
            .await
            .into_iter()
            .find(|e| e.id == undecryptable.id && e.encryption == Encryption::Decrypted)
    })
    .await;
    assert_eq!(decrypted.body.as_deref(), Some("before uplink"));
    assert_eq!(decrypted.kind, "m.room.message");

    // And uplink exports what it has.
    let export = uplink.matrix.export_room_keys("mx", "again").await.unwrap();
    assert!(String::from_utf8(export)
        .unwrap()
        .starts_with("-----BEGIN MEGOLM SESSION DATA-----"));
}

#[tokio::test(flavor = "multi_thread")]
async fn verifying_with_another_device_by_emoji() {
    use matrix_sdk::encryption::verification::{Verification, VerificationRequestState};
    use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
    use matrix_sdk::stream::StreamExt;
    use uplink_protocol::matrix::{VerificationAnswer, VerificationPhase};

    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    alice_phone
        .sync_once(SyncSettings::default())
        .await
        .unwrap();
    bootstrap_cross_signing(&alice_phone).await;
    // The phone accepts uplink's request, and the comparison uplink
    // starts, and passes it on.
    let (compared, mut comparing) = mpsc::unbounded_channel();
    alice_phone.add_event_handler(
        move |event: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let compared = compared.clone();
            async move {
                let request = client
                    .encryption()
                    .get_verification_request(&event.sender, &event.content.transaction_id)
                    .await
                    .unwrap();
                // Handlers run as sync goes: what follows needs it to.
                tokio::spawn(async move {
                    let mut changes = request.changes();
                    request.accept().await.unwrap();
                    while let Some(state) = changes.next().await {
                        if let VerificationRequestState::Transitioned {
                            verification: Verification::SasV1(sas),
                        } = state
                        {
                            sas.accept().await.unwrap();
                            compared.send(sas).unwrap();
                            return;
                        }
                    }
                });
            }
        },
    );
    let phone = alice_phone.clone();
    tokio::spawn(async move { phone.sync(SyncSettings::default()).await });

    let uplink = Uplink::new(&homeserver);
    let _sync = uplink.pair_and_sync().await;
    let security = |uplink: &Uplink| {
        let matrix = uplink.matrix.clone();
        async move {
            match matrix.get_security("mx").await.unwrap() {
                ServerResponse::MatrixSecurity { security } => security,
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    let unverified = eventually("uplink's client", || async {
        match uplink.matrix.get_security("mx").await {
            Ok(ServerResponse::MatrixSecurity { security }) => Some(security),
            Ok(other) => panic!("unexpected {other:?}"),
            Err(_) => None,
        }
    })
    .await;
    assert!(!unverified.verified);
    // Once uplink's client knows the account's cross-signing identity.
    let asked = eventually("uplink to ask to verify", || async {
        match uplink.matrix.verify_device("mx", None).await {
            Ok(ServerResponse::MatrixSecurity { security }) => Some(security),
            Ok(other) => panic!("unexpected {other:?}"),
            Err(_) => None,
        }
    })
    .await;
    let flow_id = asked.verifications.last().unwrap().flow_id.clone();
    assert!(asked.verifications.last().unwrap().we_started);

    let sas = tokio::time::timeout(Duration::from_secs(30), comparing.recv())
        .await
        .unwrap()
        .unwrap();
    let emoji = eventually("both to show emoji", || async {
        let shown = security(&uplink).await;
        let phase = &shown
            .verifications
            .iter()
            .find(|v| v.flow_id == flow_id)?
            .phase;
        match (phase, sas.emoji()) {
            (VerificationPhase::Compare { emoji, decimals }, Some(phone)) => {
                assert_eq!(decimals.len(), 3);
                Some((emoji.clone(), phone))
            }
            _ => None,
        }
    })
    .await;
    assert_eq!(
        emoji
            .0
            .iter()
            .map(|e| e.symbol.as_str())
            .collect::<Vec<_>>(),
        emoji.1.iter().map(|e| e.symbol).collect::<Vec<_>>()
    );

    uplink
        .matrix
        .answer_verification("mx", &flow_id, VerificationAnswer::Confirm)
        .await
        .unwrap();
    sas.confirm().await.unwrap();
    let verified = eventually("uplink's device to be verified", || async {
        let shown = security(&uplink).await;
        (shown.verified
            && shown
                .verifications
                .iter()
                .any(|v| v.flow_id == flow_id && v.phase == VerificationPhase::Done))
        .then_some(shown)
    })
    .await;
    assert!(verified.devices.iter().all(|d| d.verified));
}

/// A page of uplink's timeline of the room, with its gaps.
async fn page(
    uplink: &Uplink,
    room: &Room,
    page: TimelinePage,
) -> (
    Vec<RoomEvent>,
    bool,
    Vec<uplink_protocol::matrix::TimelineGap>,
) {
    match uplink
        .matrix
        .list_timeline(room.id, page, 500)
        .await
        .unwrap()
    {
        ServerResponse::Timeline {
            events,
            more_before,
            gaps,
            ..
        } => (events, more_before, gaps),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gaps_are_filled_from_the_homeserver() {
    use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    let alice_phone = homeserver.register(dir.path(), "alice").await;
    let _bob_received = sync_messages(&bob);
    // Not encrypted: uplink's device would have no keys of what came
    // before it.
    let bob_room = bob.create_room(Default::default()).await.unwrap();
    bob_room
        .invite_user_by_id(alice_phone.user_id().unwrap())
        .await
        .unwrap();
    alice_phone
        .sync_once(SyncSettings::default())
        .await
        .unwrap();
    alice_phone
        .join_room_by_id(bob_room.room_id())
        .await
        .unwrap();
    let send = |body: String| {
        let bob_room = bob_room.clone();
        async move {
            bob_room
                .send(RoomMessageEventContent::text_plain(body))
                .await
                .unwrap();
        }
    };
    for i in 0..60 {
        send(format!("before {i}")).await;
    }

    let uplink = Uplink::new(&homeserver);
    let sync = uplink.pair_and_sync().await;
    let room = eventually("uplink to see the room", || async {
        uplink.room(bob_room.room_id()).await
    })
    .await;
    // The first sync brings the latest events only: a gap before them.
    let (events, more_before, gaps) = page(&uplink, &room, TimelinePage::Latest).await;
    assert!(!more_before);
    assert!(events.len() < 60, "a sync's timeline is limited");
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].before, Some(events[0].id));

    // A restart, and events while it was down: a sync leaves a gap.
    sync.abort();
    assert!(sync.await.unwrap_err().is_cancelled());
    for i in 0..40 {
        send(format!("meanwhile {i}")).await;
    }
    uplink.start_sync();
    eventually("uplink to sync the latest", || async {
        let (events, _, _) = page(&uplink, &room, TimelinePage::Latest).await;
        (events.last()?.body.as_deref() == Some("meanwhile 39")).then_some(())
    })
    .await;
    let (_, _, gaps) = page(&uplink, &room, TimelinePage::Latest).await;
    assert_eq!(gaps.len(), 2, "before the first events, and amid them");

    // Fill them all, by pages.
    let mut fills = 0;
    loop {
        let (_, _, gaps) = page(&uplink, &room, TimelinePage::Latest).await;
        let Some(gap) = gaps.first() else { break };
        uplink.matrix.fill_gap(room.id, gap.id).await.unwrap();
        fills += 1;
        assert!(fills < 20, "filling ends");
    }
    let (events, more_before, gaps) = page(&uplink, &room, TimelinePage::Latest).await;
    assert!(!more_before && gaps.is_empty());
    let bodies: Vec<&str> = events.iter().filter_map(|e| e.body.as_deref()).collect();
    let expected: Vec<String> = (0..60)
        .map(|i| format!("before {i}"))
        .chain((0..40).map(|i| format!("meanwhile {i}")))
        .collect();
    assert_eq!(bodies, expected, "every message, in order, once");
    assert_eq!(events[0].kind, "m.room.create", "back to the room's start");
    assert!(events.windows(2).all(|w| w[0].pos < w[1].pos));
    // Paging by position, as clients do.
    let middle = events
        .iter()
        .position(|e| e.body.as_deref() == Some("before 30"))
        .unwrap();
    let (earlier, _, _) = page(
        &uplink,
        &room,
        TimelinePage::Before {
            event: events[middle].id,
        },
    )
    .await;
    assert_eq!(earlier.len(), middle);
    assert_eq!(earlier.last().unwrap().id, events[middle - 1].id);
}

#[tokio::test(flavor = "multi_thread")]
async fn members_are_listed_and_managed_and_the_room_renamed() {
    use matrix_sdk::ruma::{api::client::room::create_room::v3::Request as CreateRoom, user_id};
    use uplink_protocol::matrix::{
        MatrixContent, MatrixSendState, MemberAction, MemberInfo, MemberState, MembershipChange,
    };
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    homeserver.register(dir.path(), "alice").await;
    let carol = homeserver.register(dir.path(), "carol").await;
    homeserver.register(dir.path(), "dave").await;
    let _bob_received = sync_messages(&bob);
    let uplink = Uplink::new(&homeserver);
    let _sync = uplink.pair_and_sync().await;
    let alice = user_id!("@alice:localhost");
    let carol_id = user_id!("@carol:localhost");
    let mut request = CreateRoom::new();
    request.invite = vec![alice.to_owned(), carol_id.to_owned()];
    let bob_room = bob.create_room(request).await.unwrap();
    carol.sync_once(SyncSettings::default()).await.unwrap();
    carol.join_room_by_id(bob_room.room_id()).await.unwrap();
    let room = eventually("uplink to be invited", || async {
        uplink.room(bob_room.room_id()).await
    })
    .await;
    uplink
        .matrix
        .change_membership(room.id, MembershipChange::Join)
        .await
        .unwrap();
    eventually("bob to see alice join", || async {
        (bob_room.joined_members_count() == 3).then_some(())
    })
    .await;
    // A moderator: of more power than carol, and less than bob.
    bob_room
        .update_power_levels(vec![(alice, 50.into())])
        .await
        .unwrap();

    let members = || async {
        match uplink.matrix.list_members(room.id).await.unwrap() {
            ServerResponse::Members {
                members, powers, ..
            } => (members, powers),
            other => panic!("unexpected {other:?}"),
        }
    };
    let member =
        |members: &[MemberInfo], user: &str| members.iter().find(|m| m.user == user).cloned();
    let (listed, powers) = eventually("alice to be a moderator", || async {
        let (members, powers) = members().await;
        (member(&members, alice.as_str())?.power == Some(50)).then_some((members, powers))
    })
    .await;
    assert!(powers.invite && powers.name && powers.topic);
    let bob_listed = member(&listed, "@bob:localhost").unwrap();
    // The room's creator, of a room of version 12: above any power.
    assert_eq!((bob_listed.power, bob_listed.kickable), (None, false));
    let carol_listed = member(&listed, carol_id.as_str()).unwrap();
    assert_eq!(carol_listed.membership, MemberState::Joined);
    assert!(carol_listed.kickable && carol_listed.bannable);

    let send = |content: MatrixContent| {
        let uplink = &uplink;
        let room = &room;
        async move {
            uplink.matrix.send_message(room.id, content).await.unwrap();
            eventually("the send to go", || async {
                (uplink.sends(room).await == 0).then_some(())
            })
            .await;
        }
    };
    send(MatrixContent::Name {
        name: "Renamed".into(),
    })
    .await;
    send(MatrixContent::Topic {
        topic: "Of things".into(),
    })
    .await;
    eventually("the name and topic", || async {
        let room = uplink.room(bob_room.room_id()).await?;
        (room.name == "Renamed" && room.topic.as_deref() == Some("Of things")).then_some(())
    })
    .await;

    // Pinning an event, and unpinning it, as a moderator may.
    eventually("uplink to see it may pin", || async {
        uplink.room(bob_room.room_id()).await?.may_pin.then_some(())
    })
    .await;
    let pinned = uplink
        .timeline(&room)
        .await
        .last()
        .unwrap()
        .event_id
        .clone();
    for pin in [true, false] {
        send(MatrixContent::Pin {
            event: pinned.clone(),
            pinned: pin,
        })
        .await;
        let want = if pin { vec![pinned.clone()] } else { vec![] };
        eventually("the pins to sync back", || async {
            (uplink.room(bob_room.room_id()).await?.pinned == want).then_some(())
        })
        .await;
    }

    let membership_of = |user: &'static str, wanted: MemberState| {
        let members = &members;
        async move {
            eventually(&format!("{user} to be {wanted:?}"), || async {
                (member(&members().await.0, user)?.membership == wanted).then_some(())
            })
            .await
        }
    };
    let change = |action, user: &str, reason: Option<&str>| MatrixContent::Member {
        user: user.into(),
        action,
        reason: reason.map(Into::into),
    };
    send(change(MemberAction::Kick, carol_id.as_str(), Some("noisy"))).await;
    membership_of("@carol:localhost", MemberState::Left).await;
    send(change(MemberAction::Ban, carol_id.as_str(), None)).await;
    membership_of("@carol:localhost", MemberState::Banned).await;
    send(change(MemberAction::Unban, carol_id.as_str(), None)).await;
    membership_of("@carol:localhost", MemberState::Left).await;
    send(change(MemberAction::Invite, "@dave:localhost", None)).await;
    membership_of("@dave:localhost", MemberState::Invited).await;
    // Kicking bob, of more power, is refused: the send needs a decision.
    uplink
        .matrix
        .send_message(room.id, change(MemberAction::Kick, "@bob:localhost", None))
        .await
        .unwrap();
    eventually("the kick to be refused", || async {
        match uplink.matrix.list_sends(room.id).await.unwrap() {
            ServerResponse::MatrixSends { sends, .. } => sends
                .first()
                .filter(|s| s.state == MatrixSendState::Refused)
                .map(|_| ()),
            other => panic!("unexpected {other:?}"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rooms_are_created_direct_chats_started_and_rooms_joined_by_address() {
    use matrix_sdk::ruma::api::client::room::{
        create_room::v3::{Request as CreateRoom, RoomPreset},
        Visibility,
    };
    use uplink_protocol::matrix::NewRoom;
    let dir = tempfile::tempdir().unwrap();
    let homeserver = Homeserver::start(dir.path()).await;
    let bob = homeserver.register(dir.path(), "bob").await;
    homeserver.register(dir.path(), "alice").await;
    let _bob_received = sync_messages(&bob);
    let uplink = Uplink::new(&homeserver);
    let _sync = uplink.pair_and_sync().await;
    eventually("uplink to connect", || async {
        uplink.matrix.get_security("mx").await.ok()
    })
    .await;
    let joined =
        |response: Result<ServerResponse, uplink_protocol::ServerError>| match response.unwrap() {
            ServerResponse::MatrixRoomJoined { room_id } => OwnedRoomId::try_from(room_id).unwrap(),
            other => panic!("unexpected {other:?}"),
        };
    let listed = |room_id: OwnedRoomId| {
        let uplink = &uplink;
        async move {
            eventually("uplink to list the room joined", || async {
                uplink
                    .room(&room_id)
                    .await
                    .filter(|r| r.membership == Membership::Joined)
            })
            .await
        }
    };

    let created = joined(
        uplink
            .matrix
            .create_room(
                "mx",
                NewRoom {
                    name: " Plans ".into(),
                    topic: Some("For later".into()),
                    invite: vec!["@bob:localhost".into()],
                    encrypted: true,
                    alias: None,
                },
            )
            .await,
    );
    let room = listed(created.clone()).await;
    assert_eq!(room.name, "Plans");
    assert_eq!(room.topic.as_deref(), Some("For later"));
    assert!(room.encrypted);
    eventually("bob to be invited", || async {
        bob.get_room(&created)
            .filter(|r| r.state() == matrix_sdk::RoomState::Invited)
    })
    .await;

    // A direct chat; started again, the same.
    let direct = joined(
        uplink
            .matrix
            .start_direct_chat("mx", "@bob:localhost")
            .await,
    );
    assert_ne!(direct, created);
    listed(direct.clone()).await;
    eventually("the direct chat to be known as one", || async {
        let again = joined(
            uplink
                .matrix
                .start_direct_chat("mx", "@bob:localhost")
                .await,
        );
        (again == direct).then_some(())
    })
    .await;

    // Bob's public room, by its address.
    let mut request = CreateRoom::new();
    request.room_alias_name = Some("lobby".into());
    request.visibility = Visibility::Public;
    request.preset = Some(RoomPreset::PublicChat);
    let lobby = bob.create_room(request).await.unwrap();
    let joined_lobby = joined(uplink.matrix.join_room("mx", "#lobby:localhost").await);
    assert_eq!(joined_lobby, lobby.room_id());
    listed(joined_lobby).await;

    let unknown = uplink
        .matrix
        .join_room("mx", "#nowhere:localhost")
        .await
        .unwrap_err();
    assert_eq!(
        unknown.code,
        uplink_protocol::ErrorCode::Invalid,
        "{unknown:?}"
    );
    let not_a_user = uplink
        .matrix
        .start_direct_chat("mx", "bob")
        .await
        .unwrap_err();
    assert_eq!(not_a_user.code, uplink_protocol::ErrorCode::Invalid);
}
