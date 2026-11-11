//! An IMAP connection to an account's server, signed in (SPEC §8.10).

use crate::mail_server::sign_in::{self, Secret, SignIn};
use anyhow::{bail, Context, Result};
use async_imap::imap_proto::{Response, Status};
use tokio::net::TcpStream;
use tokio_rustls::{rustls::pki_types::ServerName, TlsConnector};
use uplink_config::{ImapAccountConfig, Security};

pub type Stream = tokio_rustls::client::TlsStream<TcpStream>;
pub type Session = async_imap::Session<Stream>;

/// The XOAUTH2 SASL mechanism, as Google and Microsoft define it.
struct XOAuth2<'a> {
    user: &'a str,
    token: &'a str,
    sent: bool,
}

impl async_imap::Authenticator for XOAuth2<'_> {
    type Response = String;

    /// The initial response; then, to a challenge (which carries the
    /// error), an empty one, after which the server refuses.
    fn process(&mut self, _challenge: &[u8]) -> String {
        if std::mem::replace(&mut self.sent, true) {
            return String::new();
        }
        sign_in::xoauth2(self.user, self.token)
    }
}

/// An IMAP connection, signed in, and what the server can do.
pub struct Connection {
    pub session: Session,
    /// CONDSTORE (RFC 7162): a mailbox's flag changes since a mod-sequence.
    pub condstore: bool,
    /// MOVE (RFC 6851).
    pub moves: bool,
    /// UIDPLUS (RFC 4315): `UID EXPUNGE`, of some messages only.
    pub uidplus: bool,
    /// IDLE (RFC 2177): changes told as they happen.
    pub idle: bool,
}

/// Connects to `account`'s IMAP server and signs in.
pub async fn connect(
    account: &ImapAccountConfig,
    sign_in: &SignIn,
    tls: &TlsConnector,
) -> Result<Connection> {
    let host = &account.imap.host;
    let port = account.imap_port();
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    let name = ServerName::try_from(host.clone())?;
    let client = match account.imap.security {
        Security::Tls => {
            let mut client = async_imap::Client::new(tls.connect(name, tcp).await?);
            greeting(&mut client).await?;
            client
        }
        Security::Starttls => {
            let mut plain = async_imap::Client::new(tcp);
            greeting(&mut plain).await?;
            plain.run_command_and_check_ok("STARTTLS", None).await?;
            async_imap::Client::new(tls.connect(name, plain.into_inner()).await?)
        }
    };
    let user = account.login().user;
    let signed_in = match sign_in.secret().await? {
        Secret::Password(password) => client.login(user, &password).await,
        Secret::AccessToken(token) => {
            client
                .authenticate(
                    "XOAUTH2",
                    XOAuth2 {
                        user,
                        token: &token,
                        sent: false,
                    },
                )
                .await
        }
    };
    let mut session = match signed_in {
        Ok(session) => session,
        Err((async_imap::error::Error::No(reason), _)) => {
            sign_in.refused().await;
            bail!("{host} refused to sign in {user}: {reason}");
        }
        Err((e, _)) => return Err(e).with_context(|| format!("signing in to {host}")),
    };
    let capabilities = session.capabilities().await?;
    Ok(Connection {
        condstore: capabilities.has_str("CONDSTORE"),
        moves: capabilities.has_str("MOVE"),
        uidplus: capabilities.has_str("UIDPLUS"),
        idle: capabilities.has_str("IDLE"),
        session,
    })
}

/// Reads the server's greeting, which must let the client sign in.
async fn greeting<T>(client: &mut async_imap::Client<T>) -> Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    let response = client
        .read_response()
        .await?
        .context("the server closed the connection before greeting")?;
    match response.parsed() {
        Response::Data {
            status: Status::Ok, ..
        } => Ok(()),
        other => bail!("the server greeted with {other:?}"),
    }
}
