//! IMAP and SMTP sync against Stalwart (`UPLINK_STALWART`, set by the dev
//! shell and `nix flake check`), a real mail server, one per test. The
//! other side is played as another mail client would, over IMAP.

mod common;

use common::{
    eventually, message,
    uplink::{label_set, Uplink},
};
use futures_util::TryStreamExt;
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio_rustls::{rustls::pki_types::CertificateDer, TlsConnector};
use uplink_config::{ImapAccountConfig, MailServer, OAuthConfig, Security};
use uplink_email::imap::{
    session::{self, Connection, SignIn},
    smtp,
    sync::ImapSync,
};
use uplink_email::oauth;
use uplink_protocol::{mail::MailProvider, ServerResponse};
use uplink_storage::seal::MasterKey;

const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";

/// A Stalwart of Alice's and Bob's, its IMAP and SMTP over TLS by a
/// certificate of the test's.
struct Stalwart {
    dir: std::path::PathBuf,
    imap: u16,
    smtp: u16,
    /// Its HTTP, for OAuth.
    http: u16,
    tls: TlsConnector,
    key: Arc<MasterKey>,
    _server: tokio::process::Child,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Stalwart {
    async fn start(dir: &Path) -> Self {
        let stalwart = std::env::var_os("UPLINK_STALWART")
            .expect("UPLINK_STALWART must name the stalwart binary (the dev shell sets it)");
        let dir = dir.join("stalwart");
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        std::fs::write(dir.join("cert.pem"), cert.pem()).unwrap();
        std::fs::write(dir.join("key.pem"), key_pair.serialize_pem()).unwrap();
        let (imap, smtp, http) = (free_port(), free_port(), free_port());
        let d = dir.display();
        let config = format!(
            r#"
[server]
hostname = "mail.example.com"
[server.listener.imaps]
bind = "127.0.0.1:{imap}"
protocol = "imap"
tls.implicit = true
[server.listener.submissions]
bind = "127.0.0.1:{smtp}"
protocol = "smtp"
tls.implicit = true
[server.listener.http]
bind = "127.0.0.1:{http}"
protocol = "http"
[certificate.default]
cert = "%{{file:{d}/cert.pem}}%"
private-key = "%{{file:{d}/key.pem}}%"
default = true
[storage]
data = "rocksdb"
fts = "rocksdb"
blob = "rocksdb"
lookup = "rocksdb"
directory = "memory"
[store.rocksdb]
type = "rocksdb"
path = "{d}/data"
[directory.memory]
type = "memory"
[[directory.memory.principals]]
name = "alice"
class = "individual"
secret = "alice-secret"
email = ["{ALICE}"]
[[directory.memory.principals]]
name = "bob"
class = "individual"
secret = "bob-secret"
email = ["{BOB}"]
[spam-filter]
enable = false
[tracer.log]
type = "log"
level = "info"
path = "{d}"
prefix = "stalwart.log"
enable = true
"#
        );
        std::fs::write(dir.join("config.toml"), config).unwrap();
        let server = tokio::process::Command::new(stalwart)
            .arg("--config")
            .arg(dir.join("config.toml"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let tls = session::tls(vec![CertificateDer::from(cert.der().to_vec())]).unwrap();
        let key = Arc::new(MasterKey::open(&dir.join("master.key")).unwrap());
        let stalwart = Self {
            dir,
            imap,
            smtp,
            http,
            tls,
            key,
            _server: server,
        };
        for _ in 0..300 {
            if tokio::net::TcpStream::connect(("127.0.0.1", imap))
                .await
                .is_ok()
                && tokio::net::TcpStream::connect(("127.0.0.1", smtp))
                    .await
                    .is_ok()
            {
                return stalwart;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("Stalwart did not start");
    }

    /// `user`'s account, its password sealed for uplink.
    fn account(&self, user: &str) -> ImapAccountConfig {
        let account = ImapAccountConfig {
            id: format!("test-{user}"),
            address: format!("{user}@example.com"),
            user: Some(user.into()),
            imap: MailServer {
                host: "localhost".into(),
                port: Some(self.imap),
                security: Security::Tls,
            },
            smtp: MailServer {
                host: "localhost".into(),
                port: Some(self.smtp),
                security: Security::Tls,
            },
            oauth: None,
            password_file: self.dir.join(format!("{user}.password")),
        };
        session::store_password(&self.key, &account, &format!("{user}-secret")).unwrap();
        account
    }

    fn sign_in(&self, account: &ImapAccountConfig) -> SignIn {
        SignIn::of(account, self.key.clone(), reqwest::Client::new())
    }

    /// uplink's side of Alice's account.
    fn provider(&self) -> ImapSync {
        let account = self.account("alice");
        ImapSync::new(account.clone(), self.sign_in(&account), self.tls.clone())
    }

    /// Another mail client, signed in as `user`.
    async fn client(&self, user: &str) -> Connection {
        let account = self.account(user);
        session::connect(&account, &self.sign_in(&account), &self.tls)
            .await
            .unwrap()
    }
}

async fn append(c: &mut Connection, folder: &str, raw: &[u8]) {
    c.session.append(folder, None, None, raw).await.unwrap();
}

/// The UID and flags of the message with `tag` in its subject, in
/// `folder`, if there are both.
async fn find(c: &mut Connection, folder: &str, tag: &str) -> Option<(u32, Vec<String>)> {
    if let Err(e) = c.session.select(folder).await {
        assert!(format!("{e}").contains("NONEXISTENT"), "{e}");
        return None;
    }
    let uids = c
        .session
        .uid_search(format!("SUBJECT \"{tag} \""))
        .await
        .unwrap();
    let uid = *uids.iter().next()?;
    let fetches: Vec<_> = c
        .session
        .uid_fetch(uid.to_string(), "(UID FLAGS)")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    let flags = fetches[0].flags().map(|f| format!("{f:?}")).collect();
    Some((uid, flags))
}

async fn store(c: &mut Connection, folder: &str, uid: u32, query: &str) {
    c.session.select(folder).await.unwrap();
    let _: Vec<_> = c
        .session
        .uid_store(uid.to_string(), query)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
}

fn dispatched(response: ServerResponse) -> uplink_protocol::TxLogId {
    let ServerResponse::Dispatched { tx_log_id } = response else {
        panic!("{response:?}")
    };
    tx_log_id
}

#[tokio::test(flavor = "multi_thread")]
async fn mirrors_the_folders_and_pushes_changes() {
    let uplink = Uplink::new(MailProvider::Imap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    other.session.create("Projects").await.unwrap();

    // Mail present before the first sync, and mail arriving afterwards.
    append(&mut other, "INBOX", &message("first", "hello world")).await;
    let sync = uplink.start(stalwart.provider(), ALICE);
    let first = uplink.arrived("first").await;
    assert_eq!(
        uplink.labels(first),
        [("INBOX".into(), true, true), ("UNREAD".into(), true, true)]
    );
    let folders: Vec<(String, String)> = uplink
        .reader()
        .prepare("SELECT external_id, name FROM label WHERE folder ORDER BY 1")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    // Stalwart's special-use folders, an Archive to be made, and Projects.
    assert_eq!(
        folders,
        [
            ("ARCHIVE", "ARCHIVE"),
            ("DRAFT", "DRAFT"),
            ("INBOX", "INBOX"),
            ("SENT", "SENT"),
            ("SPAM", "SPAM"),
            ("TRASH", "TRASH"),
            ("mailbox:Projects", "Projects"),
        ]
        .map(|(id, name)| (id.to_owned(), name.to_owned()))
    );
    append(&mut other, "INBOX", &message("second", "a quixotic body")).await;
    let second = uplink.arrived("second").await;

    // Archiving moves the message to an Archive, made for it; it is the
    // same entry there. Undo moves it back.
    let in_inbox = uplink.external_id(first);
    let archived = dispatched(
        uplink
            .mail
            .modify_labels(vec![first], vec![], vec![uplink.label("INBOX")])
            .await
            .unwrap(),
    );
    eventually("the archive to reach the server", || async {
        find(&mut stalwart.client("alice").await, "Archive", "first")
            .await
            .map(drop)
    })
    .await;
    eventually("the archive to be observed", || async {
        (uplink.labels(first)
            == [
                ("ARCHIVE".into(), true, true),
                ("UNREAD".into(), true, true),
            ]
            && uplink.external_id(first) != in_inbox)
            .then_some(())
    })
    .await;
    uplink.dispatcher.undo(archived).await.unwrap();
    eventually("the undo to reach the server", || async {
        find(&mut stalwart.client("alice").await, "INBOX", "first")
            .await
            .map(drop)
    })
    .await;
    eventually("the undo to be observed", || async {
        (label_set(&uplink.labels(first)) == ["INBOX", "UNREAD"]).then_some(())
    })
    .await;

    // Starring and reading flag the message and mark it read.
    uplink
        .mail
        .modify_labels(
            vec![second],
            vec![uplink.label("STARRED")],
            vec![uplink.label("UNREAD")],
        )
        .await
        .unwrap();
    eventually("the star to reach the server", || async {
        let (_, flags) = find(&mut stalwart.client("alice").await, "INBOX", "second").await?;
        (flags.contains(&"Seen".into()) && flags.contains(&"Flagged".into())).then_some(())
    })
    .await;

    // Changes another client makes show up in uplink: a move and a
    // keyword, the entry the same; then an expunge.
    eventually("the star to be observed", || async {
        (label_set(&uplink.labels(second)) == ["INBOX", "STARRED"]).then_some(())
    })
    .await;
    let (uid, _) = find(&mut other, "INBOX", "second").await.unwrap();
    store(&mut other, "INBOX", uid, "+FLAGS (Red)").await;
    other
        .session
        .uid_mv(uid.to_string(), "Projects")
        .await
        .unwrap();
    eventually("the move to be observed", || async {
        (label_set(&uplink.labels(second)) == ["STARRED", "kw:Red", "mailbox:Projects"])
            .then_some(())
    })
    .await;
    let (uid, _) = find(&mut other, "INBOX", "first").await.unwrap();
    store(&mut other, "INBOX", uid, "+FLAGS (\\Deleted)").await;
    let _: Vec<_> = other
        .session
        .expunge()
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    eventually("the expunge to be observed", || async {
        uplink.entry("first").is_none().then_some(())
    })
    .await;

    // Removing a keyword in uplink removes it at the server.
    uplink
        .mail
        .modify_labels(vec![second], vec![], vec![uplink.label("kw:Red")])
        .await
        .unwrap();
    eventually("the keyword's removal to reach the server", || async {
        let (_, flags) = find(&mut stalwart.client("alice").await, "Projects", "second").await?;
        (!flags.iter().any(|f| f.contains("Red"))).then_some(())
    })
    .await;
    eventually("the keyword's removal to be observed", || async {
        (!label_set(&uplink.labels(second)).contains(&"kw:Red")).then_some(())
    })
    .await;

    // A restarted sync resumes from the recorded state: idle polls record
    // nothing, and new mail still arrives.
    let settled = uplink.settled().await;
    sync.abort();
    let _ = sync.await;
    let sync = uplink.start(stalwart.provider(), ALICE);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(uplink.log_len(), settled, "the restarted sync re-synced");
    append(&mut other, "INBOX", &message("third", "after restart")).await;
    uplink.arrived("third").await;
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_sync_lists_the_mailbox_in_steps() {
    let uplink = Uplink::new(MailProvider::Imap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    for i in 0..120 {
        append(&mut other, "INBOX", &message(&format!("m{i:03}"), "")).await;
    }
    let sync = uplink.start(stalwart.provider(), ALICE);
    eventually("the listing to be done", || async {
        uplink.synced().then_some(())
    })
    .await;
    let entries: i64 = uplink
        .reader()
        .query_row("SELECT count(*) FROM mail_entry", [], |r| r.get(0))
        .unwrap();
    assert_eq!(entries, 120);
    // In steps, each a log entry.
    let steps: i64 = uplink
        .reader()
        .query_row(
            "SELECT count(*) FROM tx_log WHERE op_type = 'mail.observed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(steps, 3);
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_mail_by_smtp_and_files_it_in_sent() {
    let uplink = Uplink::new(MailProvider::Imap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    append(&mut other, "INBOX", &message("original", "lunch?")).await;
    let sync = uplink.start(stalwart.provider(), ALICE);
    let original = uplink.arrived("original").await;
    let (original_message, _, original_thread) = uplink.message_of(original);

    // A reply to Bob: delivered with uplink's Message-ID, and synced back
    // from Sent as the same message, in the original's conversation.
    let reply = format!("reply {}", common::nonce());
    uplink.send(BOB, &reply, Some(original_message)).await;
    let (uid, _) = eventually("the reply to reach Bob", || async {
        find(&mut stalwart.client("bob").await, "INBOX", "reply").await
    })
    .await;
    let mut bob = stalwart.client("bob").await;
    bob.session.select("INBOX").await.unwrap();
    let fetched: Vec<_> = bob
        .session
        .uid_fetch(
            uid.to_string(),
            "BODY.PEEK[HEADER.FIELDS (MESSAGE-ID IN-REPLY-TO)]",
        )
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    let headers = String::from_utf8(fetched[0].header().unwrap().to_vec()).unwrap();
    let sent = eventually("the reply's copy to be synced", || async {
        uplink.entry("reply")
    })
    .await;
    assert_eq!(label_set(&uplink.labels(sent)), ["SENT"]);
    let (_, message_id, thread) = uplink.message_of(sent);
    assert!(headers.contains(&format!("<{message_id}>")), "{headers}");
    assert!(message_id.ends_with("@test.uplink"), "{message_id}");
    assert_eq!(thread, original_thread);
    assert!(headers.contains("In-Reply-To: <original."), "{headers}");
    eventually("the outbox to empty", || async {
        uplink.outbox().await.is_empty().then_some(())
    })
    .await;
    sync.abort();
}

/// An account that signs in by XOAUTH2: Stalwart grants a refresh token
/// (as its own sign-in page would; it refuses the loopback redirect of
/// `uplink oauth init`, which is not HTTPS), and uplink redeems it for
/// the access token that signs in to IMAP and SMTP.
#[tokio::test(flavor = "multi_thread")]
async fn signs_in_by_xoauth2() {
    let dir = tempfile::tempdir().unwrap();
    let stalwart = Stalwart::start(dir.path()).await;
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{}", stalwart.http);
    let code: serde_json::Value = http
        .post(format!("{base}/api/oauth"))
        .basic_auth("alice", Some("alice-secret"))
        .body(r#"{"type": "code", "client_id": "uplink"}"#)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let token: serde_json::Value = http
        .post(format!("{base}/auth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", "uplink"),
            ("code", code["data"]["code"].as_str().unwrap()),
            ("redirect_uri", ""),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let refresh_token = token["refresh_token"].as_str().unwrap();

    let oauth = OAuthConfig {
        client_id: "uplink".into(),
        client_secret: None,
        auth_url: format!("{base}/authorize"),
        token_url: format!("{base}/auth/token"),
        scope: String::new(),
        token_file: stalwart.dir.join("alice.token"),
    };
    let account = ImapAccountConfig {
        oauth: Some(oauth.clone()),
        ..stalwart.account("alice")
    };
    let grant = session::grant(&account, &oauth);
    oauth::store(&stalwart.key, &grant, refresh_token.into()).unwrap();
    let sign_in = stalwart.sign_in(&account);
    assert!(matches!(sign_in, SignIn::OAuth(_)));

    let mut alice = session::connect(&account, &sign_in, &stalwart.tls)
        .await
        .unwrap();
    alice.session.select("INBOX").await.unwrap();
    let raw = format!(
        "Message-ID: <oauth@uplink.test>\r\nFrom: {ALICE}\r\nTo: {BOB}\r\n\
         Date: Tue, 3 Nov 2026 10:00:00 +0000\r\nSubject: oauth \r\n\r\nhi\r\n"
    );
    smtp::send(&account, &sign_in, &stalwart.tls, raw.as_bytes())
        .await
        .unwrap();
    eventually("the message to reach Bob", || async {
        find(&mut stalwart.client("bob").await, "INBOX", "oauth").await
    })
    .await;
}
