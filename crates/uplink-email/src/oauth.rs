//! What OAuth providers share (SPEC §13): the sealed refresh token, the
//! token endpoint's answers, and access tokens refreshed as they expire.

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Deserializer, Serialize};
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
use uplink_storage::seal::{self, MasterKey};
use url::Url;

/// Refresh an access token this long before it expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// What a token file holds, once unsealed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub refresh_token: String,
}

#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    #[serde(deserialize_with = "seconds")]
    pub expires_in: u64,
    pub refresh_token: Option<String>,
}

/// A number of seconds, which Azure AD's v1 endpoint sends as a string.
fn seconds<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Seconds {
        Number(u64),
        String(String),
    }
    match Seconds::deserialize(deserializer)? {
        Seconds::Number(n) => Ok(n),
        Seconds::String(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
}

/// The token endpoint refused the grant (e.g. `invalid_grant`: the token
/// was revoked); the account needs authorizing again.
#[derive(Debug, thiserror::Error)]
#[error("the token endpoint refused the request: {0}")]
pub struct Refused(pub String);

pub(crate) async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    let response = http
        .post(endpoint)
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

/// Where and how an account's refresh token is redeemed.
#[derive(Debug, Clone)]
pub struct Grant {
    /// The account the token is sealed for.
    pub account: String,
    pub token_file: PathBuf,
    pub endpoint: String,
    /// What a refresh request carries besides the refresh token, e.g. the
    /// client id.
    pub refresh_form: Vec<(&'static str, String)>,
}

/// Access tokens for an account, refreshed as they expire.
#[derive(Clone)]
pub struct Credentials {
    http: reqwest::Client,
    grant: Arc<Grant>,
    key: Arc<MasterKey>,
    current: Arc<Mutex<Option<(String, Instant)>>>,
}

impl Credentials {
    pub fn new(http: reqwest::Client, grant: Grant, key: Arc<MasterKey>) -> Self {
        Self {
            http,
            grant: Arc::new(grant),
            key,
            current: Arc::new(Mutex::new(None)),
        }
    }

    pub fn token_file(&self) -> &PathBuf {
        &self.grant.token_file
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
        let grant = &self.grant;
        let stored = seal::read::<Token>(&self.key, &grant.account, &grant.token_file)?;
        let requested = Instant::now();
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", stored.refresh_token.as_str()),
        ];
        form.extend(grant.refresh_form.iter().map(|(k, v)| (*k, v.as_str())));
        let response = token_request(&self.http, &grant.endpoint, &form).await?;
        // The refresh token may rotate (SPEC §15.4).
        if let Some(refresh_token) = response.refresh_token {
            if refresh_token != stored.refresh_token {
                store(&self.key, grant, refresh_token)?;
            }
        }
        Ok((
            response.access_token,
            requested + Duration::from_secs(response.expires_in),
        ))
    }
}

/// Seals `refresh_token` into the grant's token file.
pub(crate) fn store(key: &MasterKey, grant: &Grant, refresh_token: String) -> Result<()> {
    seal::write(
        key,
        &grant.account,
        &Token { refresh_token },
        &grant.token_file,
    )
}

/// An installed application's authorization: the authorization-code
/// flow with PKCE and a loopback redirect on any port (RFC 8252 §7.3).
pub struct LoopbackFlow<'a> {
    pub auth_url: &'a str,
    pub token_url: &'a str,
    pub client_id: &'a str,
    pub client_secret: Option<&'a str>,
    /// The account's address, for the provider to sign in as.
    pub login_hint: &'a str,
    /// The provider's own parameters of the authorization request: what
    /// access is asked for (e.g. `scope`, or Entra ID v1's `resource`),
    /// and anything else it wants.
    pub params: &'a [(&'a str, &'a str)],
}

/// Runs `flow`; returns the refresh token it yields, if the provider
/// gave one. The user opens the URL passed to `present`; the provider
/// redirects to a loopback listener, or, if the browser is on another
/// machine, the user supplies the URL it was redirected to through
/// `pasted` (e.g. [`read_pasted_url`]).
pub async fn authorize_loopback(
    flow: &LoopbackFlow<'_>,
    present: impl FnOnce(&Url),
    pasted: impl std::future::Future<Output = Result<Url>>,
) -> Result<Option<String>> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding the loopback redirect listener")?;
    let redirect_uri = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
    let verifier = random_token();
    let state = random_token();
    let mut url = Url::parse(flow.auth_url)
        .with_context(|| format!("parsing the authorization endpoint {}", flow.auth_url))?;
    url.query_pairs_mut()
        .append_pair("client_id", flow.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("code_challenge", &challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("login_hint", flow.login_hint)
        .extend_pairs(flow.params);
    present(&url);

    let redirected = tokio::select! {
        url = receive_redirect(&listener) => url?,
        url = pasted => url?,
    };
    let params: std::collections::HashMap<String, String> =
        redirected.query_pairs().into_owned().collect();
    if let Some(error) = params.get("error") {
        let description = params.get("error_description").map_or("", |d| d.as_str());
        bail!("authorization failed: {error} {description}");
    }
    ensure!(
        params.get("state") == Some(&state),
        "the redirect is not from this authorization"
    );
    let code = params
        .get("code")
        .context("the redirect carries no authorization code")?;

    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &redirect_uri),
        ("client_id", flow.client_id),
        ("code_verifier", &verifier),
    ];
    if let Some(secret) = flow.client_secret {
        form.push(("client_secret", secret));
    }
    let response = token_request(&reqwest::Client::new(), flow.token_url, &form).await?;
    Ok(response.refresh_token)
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
/// once stdin is closed. Reads on a thread of its own, left blocked once
/// the browser's redirect arrives: a read through tokio's stdin would keep
/// the runtime from shutting down until a line was typed.
pub async fn read_pasted_url() -> Result<Url> {
    let (lines, mut pasted) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            if lines.blocking_send(line).is_err() {
                return;
            }
        }
    });
    while let Some(line) = pasted.recv().await {
        match Url::parse(line?.trim()) {
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

    #[test]
    fn expiry_is_a_number_or_a_string() {
        let parse = |json| {
            serde_json::from_str::<TokenResponse>(json)
                .unwrap()
                .expires_in
        };
        assert_eq!(parse(r#"{"access_token":"a","expires_in":3599}"#), 3599);
        assert_eq!(parse(r#"{"access_token":"a","expires_in":"3599"}"#), 3599);
    }
}
