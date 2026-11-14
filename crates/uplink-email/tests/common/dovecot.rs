//! Dovecot (`UPLINK_DOVECOT`, set by the dev shell and `nix flake check`),
//! an IMAP server with NOTIFY (RFC 5465), which Stalwart lacks; one per
//! test, run as the test's own user.

use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio_rustls::{rustls::pki_types::CertificateDer, TlsConnector};
use uplink_config::{ImapAccountConfig, MailServer, Security};
use uplink_email::{
    imap::session::{self, Connection},
    mail_server::sign_in::{self, SignIn},
};
use uplink_storage::seal::MasterKey;

/// A Dovecot of Alice's, its IMAP over TLS by a certificate of the test's,
/// her mailbox with an Alerts folder besides the Inbox.
pub struct Dovecot {
    dir: std::path::PathBuf,
    imap: u16,
    pub tls: TlsConnector,
    pub key: Arc<MasterKey>,
    _server: tokio::process::Child,
}

/// The output of `id` with `flag`: the test's user or group.
fn id(flag: &str) -> String {
    let out = std::process::Command::new("id").arg(flag).output().unwrap();
    assert!(out.status.success(), "id {flag}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// How Dovecot signs Alice in.
enum Auth<'a> {
    /// By her password.
    Password,
    /// By XOAUTH2, a token being valid if `url` answers with her address
    /// as `mail` to a request bearing it (as Microsoft Graph's `/me`
    /// does).
    Token { url: &'a str },
}

impl Dovecot {
    pub async fn start(dir: &Path) -> Self {
        Self::start_with(dir, Auth::Password).await
    }

    /// Signing Alice in by XOAUTH2, by a token `url` (e.g. Graph's `/me`)
    /// takes, answering with her address as `mail`.
    pub async fn start_with_tokens(dir: &Path, url: &str) -> Self {
        Self::start_with(dir, Auth::Token { url }).await
    }

    async fn start_with(dir: &Path, auth: Auth<'_>) -> Self {
        let dovecot = std::env::var_os("UPLINK_DOVECOT")
            .expect("UPLINK_DOVECOT must name the dovecot binary (the dev shell sets it)");
        let dir = dir.join("dovecot");
        std::fs::create_dir_all(&dir).unwrap();
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        std::fs::write(dir.join("cert.pem"), cert.pem()).unwrap();
        std::fs::write(dir.join("key.pem"), key_pair.serialize_pem()).unwrap();
        std::fs::write(dir.join("passwd"), "alice:{PLAIN}alice-secret::::::\n").unwrap();
        let passdb = match auth {
            Auth::Password => format!(
                "auth_mechanisms = plain
passdb passwd-file {{
  passwd_file_path = {}/passwd
}}",
                dir.display()
            ),
            Auth::Token { url } => format!(
                "auth_mechanisms = xoauth2
auth_username_format = %{{user}}
passdb oauth2 {{
  mechanisms_filter = xoauth2
}}
oauth2 {{
  introspection_mode = auth
  introspection_url = {url}
  username_attribute = mail
}}"
            ),
        };
        let ([imap], _picking) = super::free_ports().await;
        let d = dir.display();
        let (user, group) = (id("-un"), id("-gn"));
        let (uid, gid) = (id("-u"), id("-g"));
        // Its sockets are under `base_dir`, whose path must be short (as a
        // Unix socket's is).
        let config = format!(
            r#"dovecot_config_version = 2.4.4
dovecot_storage_version = 2.4.4
base_dir = {d}/run
state_dir = {d}/state
log_path = {d}/log
protocols = imap
listen = 127.0.0.1
default_internal_user = {user}
default_internal_group = {group}
default_login_user = {user}
{passdb}
userdb static {{
  fields {{
    uid = {uid}
    gid = {gid}
    home = {d}/home/%{{user}}
  }}
}}
mail_driver = maildir
mail_path = ~/Maildir
namespace inbox {{
  inbox = yes
  separator = /
  mailbox Alerts {{
    auto = create
  }}
}}
ssl = required
ssl_server_cert_file = {d}/cert.pem
ssl_server_key_file = {d}/key.pem
service imap-login {{
  chroot =
  inet_listener imap {{
    port = 0
  }}
  inet_listener imaps {{
    port = {imap}
    ssl = yes
  }}
}}
service anvil {{
  chroot =
}}
"#
        );
        std::fs::write(dir.join("dovecot.conf"), config).unwrap();
        let server = tokio::process::Command::new(dovecot)
            .arg("-F")
            .arg("-c")
            .arg(dir.join("dovecot.conf"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let dovecot = Self {
            tls: sign_in::tls(vec![CertificateDer::from(cert.der().to_vec())]).unwrap(),
            key: Arc::new(MasterKey::open(&dir.join("master.key")).unwrap()),
            dir,
            imap,
            _server: server,
        };
        for _ in 0..300 {
            if tokio::net::TcpStream::connect(("127.0.0.1", imap))
                .await
                .is_ok()
            {
                return dovecot;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "Dovecot did not start: {}",
            std::fs::read_to_string(dovecot.dir.join("log")).unwrap_or_default()
        );
    }

    /// Alice's account, her password sealed for uplink. Dovecot has no
    /// SMTP: uplink sends nothing in these tests.
    /// Its IMAP server, as an account names it.
    pub fn server(&self) -> MailServer {
        MailServer {
            host: "localhost".into(),
            port: Some(self.imap),
            security: Security::Tls,
        }
    }

    pub fn account(&self) -> ImapAccountConfig {
        let server = self.server();
        let account = ImapAccountConfig {
            id: "test-alice".into(),
            address: "alice@example.com".into(),
            name: None,
            identities: vec![],
            user: Some("alice".into()),
            imap: server.clone(),
            smtp: server,
            oauth: None,
            watch: vec![],
            password_file: self.dir.join("alice.password"),
        };
        sign_in::store_password(&self.key, account.login(), "alice-secret").unwrap();
        account
    }

    pub fn sign_in(&self, account: &ImapAccountConfig) -> SignIn {
        SignIn::of(account.login(), self.key.clone(), reqwest::Client::new())
    }

    /// Another mail client, signed in as Alice.
    pub async fn client(&self) -> Connection {
        let account = self.account();
        session::connect(
            &account.imap,
            account.login().user,
            &self.sign_in(&account),
            &self.tls,
        )
        .await
        .unwrap()
    }
}
