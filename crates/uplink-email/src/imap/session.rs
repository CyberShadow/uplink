//! Reaching an account's servers: TLS, and signing in with the sealed
//! password or an OAuth access token (SPEC §8.10, §13.4).

use crate::oauth::{self, Credentials, Grant, LoopbackFlow};
use anyhow::{bail, Context, Result};
use async_imap::imap_proto::{Response, Status};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tokio::net::TcpStream;
use tokio_rustls::{
    rustls::{
        self,
        pki_types::{CertificateDer, ServerName},
        ClientConfig,
    },
    TlsConnector,
};
use uplink_config::{ImapAccountConfig, OAuthConfig, Security};
use uplink_storage::seal::{self, MasterKey};
use url::Url;

pub type Stream = tokio_rustls::client::TlsStream<TcpStream>;
pub type Session = async_imap::Session<Stream>;

/// A connector that trusts the system's roots and `extra_roots` (which
/// tests' servers are signed by).
pub fn tls(extra_roots: Vec<CertificateDer<'static>>) -> Result<TlsConnector> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier =
        rustls_platform_verifier::Verifier::new_with_extra_roots(extra_roots, provider.clone())?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        // Any verifier but rustls' own is behind `dangerous`; this one
        // verifies as browsers do.
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// What a password file holds, once unsealed.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Password {
    password: String,
}

/// Seals `password` as the account's (`uplink password`).
pub fn store_password(key: &MasterKey, account: &ImapAccountConfig, password: &str) -> Result<()> {
    seal::write(
        key,
        &account.id,
        &Password {
            password: password.to_owned(),
        },
        &account.password_file,
    )
}

/// How the account signs in to its servers.
#[derive(Clone)]
pub enum SignIn {
    /// With the password sealed in `file`, read at each sign-in, so that
    /// a new one takes effect without a restart.
    Password {
        account: String,
        key: Arc<MasterKey>,
        file: PathBuf,
    },
    /// With an access token, by XOAUTH2.
    OAuth(Credentials),
}

/// What signs in, as of now.
pub enum Secret {
    Password(String),
    AccessToken(String),
}

impl SignIn {
    pub fn of(account: &ImapAccountConfig, key: Arc<MasterKey>, http: reqwest::Client) -> Self {
        match &account.oauth {
            Some(oauth) => SignIn::OAuth(Credentials::new(http, grant(account, oauth), key)),
            None => SignIn::Password {
                account: account.id.clone(),
                key,
                file: account.password_file.clone(),
            },
        }
    }

    pub async fn secret(&self) -> Result<Secret> {
        match self {
            SignIn::Password { account, key, file } => {
                let sealed = seal::read::<Password>(key, account, file).with_context(|| {
                    format!("no password for {account}: set it with `uplink password --account {account}`")
                })?;
                Ok(Secret::Password(sealed.password))
            }
            SignIn::OAuth(credentials) => {
                Ok(Secret::AccessToken(credentials.access_token().await?))
            }
        }
    }

    /// The server refused the secret: an access token is refreshed for the
    /// next sign-in, in case it was revoked before it expired.
    pub async fn refused(&self) {
        if let SignIn::OAuth(credentials) = self {
            credentials.invalidate().await;
        }
    }
}

/// How the account's refresh token is redeemed.
pub fn grant(account: &ImapAccountConfig, oauth: &OAuthConfig) -> Grant {
    let mut refresh_form = vec![("client_id", oauth.client_id.clone())];
    if let Some(secret) = &oauth.client_secret {
        refresh_form.push(("client_secret", secret.clone()));
    }
    Grant {
        account: account.id.clone(),
        token_file: oauth.token_file.clone(),
        endpoint: oauth.token_url.clone(),
        refresh_form,
    }
}

/// Runs the OAuth authorization of an account that signs in with it, and
/// stores its refresh token (see [`oauth::authorize_loopback`]).
pub async fn authorize(
    account: &ImapAccountConfig,
    key: &MasterKey,
    present: impl FnOnce(&Url),
    pasted: impl std::future::Future<Output = Result<Url>>,
) -> Result<()> {
    let oauth = account
        .oauth
        .as_ref()
        .with_context(|| format!("{} signs in with a password, not OAuth", account.id))?;
    let flow = LoopbackFlow {
        auth_url: &oauth.auth_url,
        token_url: &oauth.token_url,
        client_id: &oauth.client_id,
        client_secret: oauth.client_secret.as_deref(),
        scope: &oauth.scope,
        login_hint: &account.address,
        extra: &[],
    };
    let refresh_token = oauth::authorize_loopback(&flow, present, pasted)
        .await?
        .context("the provider returned no refresh token; does the scope ask for one?")?;
    oauth::store(key, &grant(account, oauth), refresh_token)
}

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
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
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
    let user = account.user();
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
