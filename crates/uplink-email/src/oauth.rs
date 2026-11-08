//! What OAuth providers share (SPEC §13): the sealed refresh token, the
//! token endpoint's answers, and access tokens refreshed as they expire.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uplink_storage::seal::{self, MasterKey};

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

#[cfg(test)]
mod tests {
    use super::*;

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
