//! Reaching an account's servers: TLS, and signing in with the sealed
//! password or an OAuth access token (SPEC §13.4, §13.7).

use crate::oauth::{self, Credentials, Grant, LoopbackFlow};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tokio_rustls::{
    rustls::{self, pki_types::CertificateDer, ClientConfig},
    TlsConnector,
};
use uplink_config::{Login, OAuthConfig};
use uplink_storage::seal::{self, MasterKey};
use url::Url;

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
pub fn store_password(key: &MasterKey, account: Login<'_>, password: &str) -> Result<()> {
    seal::write(
        key,
        account.id,
        &Password {
            password: password.to_owned(),
        },
        account.password_file,
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
    pub fn of(account: Login<'_>, key: Arc<MasterKey>, http: reqwest::Client) -> Self {
        match account.oauth {
            Some(oauth) => SignIn::OAuth(Credentials::new(http, grant(account, oauth), key)),
            None => SignIn::Password {
                account: account.id.to_owned(),
                key,
                file: account.password_file.to_owned(),
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

/// The XOAUTH2 initial response, as Google and Microsoft define it, before
/// base64.
pub fn xoauth2(user: &str, token: &str) -> String {
    format!("user={user}\x01auth=Bearer {token}\x01\x01")
}

/// How the account's refresh token is redeemed.
pub fn grant(account: Login<'_>, oauth: &OAuthConfig) -> Grant {
    let mut refresh_form = vec![("client_id", oauth.client_id.clone())];
    if let Some(secret) = &oauth.client_secret {
        refresh_form.push(("client_secret", secret.clone()));
    }
    Grant {
        account: account.id.to_owned(),
        token_file: oauth.token_file.clone(),
        endpoint: oauth.token_url.clone(),
        refresh_form,
    }
}

/// Runs the OAuth authorization of an account that signs in with it, and
/// stores its refresh token (see [`oauth::authorize_loopback`]).
pub async fn authorize(
    account: Login<'_>,
    key: &MasterKey,
    present: impl FnOnce(&Url),
    pasted: impl std::future::Future<Output = Result<Url>>,
) -> Result<()> {
    let oauth = account
        .oauth
        .with_context(|| format!("{} signs in with a password, not OAuth", account.id))?;
    let flow = LoopbackFlow {
        auth_url: &oauth.auth_url,
        token_url: &oauth.token_url,
        client_id: &oauth.client_id,
        client_secret: oauth.client_secret.as_deref(),
        login_hint: account.address,
        params: &[("scope", &oauth.scope)],
    };
    let refresh_token = oauth::authorize_loopback(&flow, present, pasted)
        .await?
        .context("the provider returned no refresh token; does the scope ask for one?")?;
    oauth::store(key, &grant(account, oauth), refresh_token)
}
