//! Google OAuth: the one-time authorization (`uplink oauth init`) and
//! access-token refresh (SPEC §13.2).

use crate::oauth::{self, token_request, Grant};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use uplink_config::GmailAccountConfig;
use uplink_storage::seal::MasterKey;
use url::Url;

pub const SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";

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

    let response = token_request(
        &reqwest::Client::new(),
        &account.endpoints.token,
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
    oauth::store(key, &grant(account), refresh_token)
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

/// How `account`'s refresh token is redeemed.
pub fn grant(account: &GmailAccountConfig) -> Grant {
    Grant {
        account: account.id.clone(),
        token_file: account.oauth.token_file.clone(),
        endpoint: account.endpoints.token.clone(),
        refresh_form: vec![
            ("client_id", account.oauth.client_id.clone()),
            ("client_secret", account.oauth.client_secret.clone()),
        ],
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
