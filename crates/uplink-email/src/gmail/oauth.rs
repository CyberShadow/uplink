//! Google OAuth: the one-time authorization (`uplink oauth init`) and
//! access-token refresh (SPEC §13.2).

use crate::oauth::{self, Grant, LoopbackFlow};
use anyhow::{Context, Result};
use uplink_config::GmailAccountConfig;
use uplink_storage::seal::MasterKey;
use url::Url;

/// Gmail's, to read, label and send; Pub/Sub's, for the topic and
/// subscription push notifications go through (SPEC §8.1).
pub const SCOPE: &str =
    "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/pubsub";

/// Runs the authorization-code flow for `account` and stores the refresh
/// token it yields (see [`oauth::authorize_loopback`]).
pub async fn authorize(
    account: &GmailAccountConfig,
    key: &MasterKey,
    present: impl FnOnce(&Url),
    pasted: impl std::future::Future<Output = Result<Url>>,
) -> Result<()> {
    let flow = LoopbackFlow {
        auth_url: &account.endpoints.auth,
        token_url: &account.endpoints.token,
        client_id: &account.oauth.client_id,
        client_secret: Some(&account.oauth.client_secret),
        scope: SCOPE,
        login_hint: &account.address,
        // For a refresh token, every time.
        extra: &[("access_type", "offline"), ("prompt", "consent")],
    };
    let refresh_token = oauth::authorize_loopback(&flow, present, pasted)
        .await?
        .context("Google returned no refresh token")?;
    oauth::store(key, &grant(account), refresh_token)
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
