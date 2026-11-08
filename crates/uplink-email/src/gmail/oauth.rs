//! Google OAuth: the one-time authorization (`uplink oauth init`) and
//! access-token refresh (SPEC §13.2).

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::Mutex,
};
use uplink_config::GmailAccountConfig;
use uplink_storage::seal::{self, MasterKey};
use url::Url;

pub const SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";

/// Refresh an access token this long before it expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// Runs the authorization-code flow for `account` and stores the refresh
/// token it yields. The user opens the URL passed to `present`; Google
/// redirects to a loopback listener, or, if the browser is on another
/// machine, the user supplies the URL it was redirected to through
/// `pasted` (e.g. [`read_pasted_url`]).
pub async fn authorize(
    account: &GmailAccountConfig,
    key: &MasterKey,
    present: impl FnOnce(&Url),
    pasted: impl std::future::Future<Output = Result<Url>>,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding the loopback redirect listener")?;
    let redirect_uri = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
    let verifier = random_token();
    let state = random_token();
    let mut url = Url::parse(&account.endpoints.auth).context("parsing endpoints.auth")?;
    url.query_pairs_mut()
        .append_pair("client_id", &account.oauth.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("login_hint", &account.address);
    present(&url);

    let redirected = tokio::select! {
        url = receive_redirect(&listener) => url?,
        url = pasted => url?,
    };
    let params: std::collections::HashMap<String, String> =
        redirected.query_pairs().into_owned().collect();
    if let Some(error) = params.get("error") {
        bail!("authorization failed: {error}");
    }
    ensure!(
        params.get("state") == Some(&state),
        "the redirect is not from this authorization"
    );
    let code = params
        .get("code")
        .context("the redirect carries no authorization code")?;

    let response: TokenResponse = token_request(
        &reqwest::Client::new(),
        account,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri),
            ("client_id", &account.oauth.client_id),
            ("client_secret", &account.oauth.client_secret),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    let refresh_token = response
        .refresh_token
        .context("Google returned no refresh token")?;
    seal::write(
        key,
        &account.id,
        &Token { refresh_token },
        &account.oauth.token_file,
    )?;
    Ok(())
}

/// Accepts connections until one is the redirect, and answers it.
async fn receive_redirect(listener: &TcpListener) -> Result<Url> {
    loop {
        let (stream, _) = listener.accept().await?;
        let mut stream = BufReader::new(stream);
        let mut request_line = String::new();
        stream.read_line(&mut request_line).await?;
        // "GET /?code=...&state=... HTTP/1.1"
        let Some(target) = request_line.split_whitespace().nth(1) else {
            continue;
        };
        let url = Url::parse("http://127.0.0.1/")?.join(target)?;
        if !url.query_pairs().any(|(k, _)| k == "code" || k == "error") {
            continue;
        }
        let body = "uplink: authorization received; you can close this window.\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.get_mut().write_all(response.as_bytes()).await?;
        return Ok(url);
    }
}

/// Reads lines from stdin until one is a URL with a query; never returns
/// once stdin is closed.
pub async fn read_pasted_url() -> Result<Url> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        match Url::parse(line.trim()) {
            Ok(url) if url.query().is_some() => return Ok(url),
            _ => eprintln!("That is not the URL the browser was redirected to; try again."),
        }
    }
    // Without stdin, wait for the browser.
    std::future::pending().await
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// What a token file holds, once unsealed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub refresh_token: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
}

/// The token endpoint refused the grant (e.g. `invalid_grant`: the token
/// was revoked); the account needs authorizing again.
#[derive(Debug, thiserror::Error)]
#[error("Google refused the token request: {0}")]
pub struct Refused(pub String);

async fn token_request(
    http: &reqwest::Client,
    account: &GmailAccountConfig,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    let response = http
        .post(&account.endpoints.token)
        .form(form)
        .send()
        .await
        .context("requesting a token")?;
    let status = response.status();
    if status.is_success() {
        return response.json().await.context("decoding the token response");
    }
    let body = response.text().await?;
    match serde_json::from_str::<TokenError>(&body) {
        Ok(e) if status.is_client_error() => Err(Refused(match e.error_description {
            Some(description) => format!("{}: {description}", e.error),
            None => e.error,
        })
        .into()),
        _ => bail!("token request failed with {status}: {body}"),
    }
}

/// Access tokens for an account, refreshed as they expire.
#[derive(Clone)]
pub struct Credentials {
    http: reqwest::Client,
    account: Arc<GmailAccountConfig>,
    key: Arc<MasterKey>,
    current: Arc<Mutex<Option<(String, Instant)>>>,
}

impl Credentials {
    pub fn new(http: reqwest::Client, account: GmailAccountConfig, key: Arc<MasterKey>) -> Self {
        Self {
            http,
            account: Arc::new(account),
            key,
            current: Arc::new(Mutex::new(None)),
        }
    }

    pub fn token_file(&self) -> &PathBuf {
        &self.account.oauth.token_file
    }

    /// A valid access token.
    pub async fn access_token(&self) -> Result<String> {
        let mut current = self.current.lock().await;
        if let Some((token, expires)) = &*current {
            if Instant::now() + EXPIRY_MARGIN < *expires {
                return Ok(token.clone());
            }
        }
        let token = self.refresh().await?;
        *current = Some(token.clone());
        Ok(token.0)
    }

    /// Forgets the access token, e.g. after the API rejected it.
    pub async fn invalidate(&self) {
        *self.current.lock().await = None;
    }

    async fn refresh(&self) -> Result<(String, Instant)> {
        let account = &self.account;
        let stored = seal::read::<Token>(&self.key, &account.id, &account.oauth.token_file)?;
        let requested = Instant::now();
        let response = token_request(
            &self.http,
            account,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &stored.refresh_token),
                ("client_id", &account.oauth.client_id),
                ("client_secret", &account.oauth.client_secret),
            ],
        )
        .await?;
        // Google may rotate the refresh token (SPEC §15.4).
        if let Some(refresh_token) = response.refresh_token {
            if refresh_token != stored.refresh_token {
                seal::write(
                    &self.key,
                    &account.id,
                    &Token { refresh_token },
                    &account.oauth.token_file,
                )?;
            }
        }
        Ok((
            response.access_token,
            requested + Duration::from_secs(response.expires_in),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge() {
        // RFC 7636, appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
