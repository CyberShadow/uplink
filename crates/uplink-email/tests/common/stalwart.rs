//! Stalwart (`UPLINK_STALWART`, set by the dev shell and `nix flake
//! check`), a real mail server, one per test, for the IMAP, POP3 and JMAP
//! tests; and the other mail clients they play, over IMAP or JMAP.

use futures_util::TryStreamExt;
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio_rustls::{rustls::pki_types::CertificateDer, TlsConnector};
use uplink_config::{
    ImapAccountConfig, JmapAccountConfig, Login, MailServer, Pop3AccountConfig, Security,
};
use uplink_email::{
    imap::session::{self, Connection},
    jmap::api::Jmap,
    mail_server::sign_in::{self, SignIn},
};
use uplink_storage::seal::MasterKey;

pub const ALICE: &str = "alice@example.com";
pub const BOB: &str = "bob@example.com";

/// A Stalwart of Alice's and Bob's, its IMAP, SMTP and JMAP over TLS, and
/// POP3 upgraded to it, by a certificate of the test's.
pub struct Stalwart {
    pub dir: std::path::PathBuf,
    imap: u16,
    smtp: u16,
    /// Its POP3, upgraded by STLS.
    pop3: u16,
    /// Its HTTP, for OAuth.
    pub http: u16,
    /// Its HTTP over TLS, for JMAP.
    https: u16,
    pub tls: TlsConnector,
    /// An HTTP client trusting its certificate.
    pub client: reqwest::Client,
    pub key: Arc<MasterKey>,
    _server: tokio::process::Child,
}

/// Whom a Stalwart knows, and how it signs them in.
pub enum Directory<'a> {
    /// Alice and Bob, by password.
    Passwords,
    /// Those that the OpenID provider at `userinfo` (its userinfo
    /// endpoint) gives an access token to, by the token, made accounts as
    /// they first sign in.
    OpenId { userinfo: &'a str },
}

impl Stalwart {
    /// A Stalwart of Alice's and Bob's, signing them in by password.
    pub async fn start(dir: &Path) -> Self {
        Self::start_with(dir, Directory::Passwords).await
    }

    pub async fn start_with(dir: &Path, directory: Directory<'_>) -> Self {
        let (directory, directories) = match directory {
            Directory::Passwords => (
                "memory",
                format!(
                    r#"[directory.memory]
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
email = ["{BOB}"]"#
                ),
            ),
            Directory::OpenId { userinfo } => (
                "openid",
                format!(
                    r#"[directory.openid]
type = "oidc"
endpoint.url = "{userinfo}"
endpoint.method = "userinfo"
fields.email = "email""#
                ),
            ),
        };
        let stalwart = std::env::var_os("UPLINK_STALWART")
            .expect("UPLINK_STALWART must name the stalwart binary (the dev shell sets it)");
        let dir = dir.join("stalwart");
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        std::fs::write(dir.join("cert.pem"), cert.pem()).unwrap();
        std::fs::write(dir.join("key.pem"), key_pair.serialize_pem()).unwrap();
        let ([imap, smtp, pop3, http, https], _picking) = super::free_ports().await;
        let d = dir.display();
        let config = format!(
            r#"
[server]
hostname = "mail.example.com"
# JMAP's URLs, which are of the hostname otherwise.
[http]
url = "'https://localhost:{https}'"
[server.listener.imaps]
bind = "127.0.0.1:{imap}"
protocol = "imap"
tls.implicit = true
[server.listener.submissions]
bind = "127.0.0.1:{smtp}"
protocol = "smtp"
tls.implicit = true
[server.listener.pop3]
bind = "127.0.0.1:{pop3}"
protocol = "pop3"
[server.listener.http]
bind = "127.0.0.1:{http}"
protocol = "http"
[server.listener.https]
bind = "127.0.0.1:{https}"
protocol = "http"
tls.implicit = true
[certificate.default]
cert = "%{{file:{d}/cert.pem}}%"
private-key = "%{{file:{d}/key.pem}}%"
default = true
[storage]
data = "rocksdb"
fts = "rocksdb"
blob = "rocksdb"
lookup = "rocksdb"
directory = "{directory}"
[store.rocksdb]
type = "rocksdb"
path = "{d}/data"
{directories}
# Received headers, as on mail from the internet (port 25), which POP3
# dates mail by.
[session.data.add-headers]
received = true
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
        let tls = sign_in::tls(vec![CertificateDer::from(cert.der().to_vec())]).unwrap();
        let client = reqwest::Client::builder()
            .tls_certs_merge([reqwest::Certificate::from_der(cert.der()).unwrap()])
            .build()
            .unwrap();
        let key = Arc::new(MasterKey::open(&dir.join("master.key")).unwrap());
        let stalwart = Self {
            dir,
            imap,
            smtp,
            pop3,
            http,
            https,
            tls,
            client,
            key,
            _server: server,
        };
        for _ in 0..300 {
            let mut up = true;
            for port in [imap, smtp, pop3, http, https] {
                up &= tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_ok();
            }
            if up {
                return stalwart;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("Stalwart did not start");
    }

    fn server(&self, port: u16, security: Security) -> MailServer {
        MailServer {
            host: "localhost".into(),
            port: Some(port),
            security,
        }
    }

    /// `user`'s IMAP account, its password sealed for uplink.
    pub fn account(&self, user: &str) -> ImapAccountConfig {
        let account = ImapAccountConfig {
            id: format!("test-{user}"),
            address: format!("{user}@example.com"),
            user: Some(user.into()),
            imap: self.server(self.imap, Security::Tls),
            smtp: self.server(self.smtp, Security::Tls),
            oauth: None,
            password_file: self.dir.join(format!("{user}.password")),
        };
        self.seal_password(account.login());
        account
    }

    /// `user`'s POP3 account, its password sealed for uplink.
    pub fn pop3_account(&self, user: &str, delete_after_download: bool) -> Pop3AccountConfig {
        let account = Pop3AccountConfig {
            id: format!("test-{user}"),
            address: format!("{user}@example.com"),
            user: Some(user.into()),
            pop3: self.server(self.pop3, Security::Starttls),
            smtp: self.server(self.smtp, Security::Tls),
            delete_after_download,
            oauth: None,
            password_file: self.dir.join(format!("{user}.password")),
        };
        self.seal_password(account.login());
        account
    }

    /// `user`'s JMAP account, its password sealed for uplink.
    pub fn jmap_account(&self, user: &str) -> JmapAccountConfig {
        let account = JmapAccountConfig {
            id: format!("test-{user}"),
            address: format!("{user}@example.com"),
            user: Some(user.into()),
            session_url: format!("https://localhost:{}/.well-known/jmap", self.https)
                .try_into()
                .unwrap(),
            oauth: None,
            password_file: self.dir.join(format!("{user}.password")),
        };
        self.seal_password(account.login());
        account
    }

    /// `account`'s JMAP server.
    pub fn jmap_of(&self, account: &JmapAccountConfig) -> Jmap {
        Jmap::new(
            self.client.clone(),
            account.session_url.as_str(),
            account.login().user,
            SignIn::of(account.login(), self.key.clone(), self.client.clone()),
        )
    }

    /// Another mail client, signed in as `user` by JMAP.
    pub fn jmap(&self, user: &str) -> Jmap {
        self.jmap_of(&self.jmap_account(user))
    }

    fn seal_password(&self, login: Login<'_>) {
        let password = format!("{}-secret", login.user);
        sign_in::store_password(&self.key, login, &password).unwrap();
    }

    pub fn sign_in(&self, login: Login<'_>) -> SignIn {
        SignIn::of(login, self.key.clone(), reqwest::Client::new())
    }

    /// Another mail client, signed in as `user`.
    pub async fn client(&self, user: &str) -> Connection {
        let account = self.account(user);
        session::connect(&account, &self.sign_in(account.login()), &self.tls)
            .await
            .unwrap()
    }
}

pub async fn append(c: &mut Connection, folder: &str, raw: &[u8]) {
    c.session.append(folder, None, None, raw).await.unwrap();
}

/// The UID and flags of the message with `tag` in its subject, in
/// `folder`, if there are both.
pub async fn find(c: &mut Connection, folder: &str, tag: &str) -> Option<(u32, Vec<String>)> {
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

pub async fn store(c: &mut Connection, folder: &str, uid: u32, query: &str) {
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
