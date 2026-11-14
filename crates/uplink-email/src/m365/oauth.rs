//! Entra ID (Azure AD) v1 OAuth as a public client: the one-time
//! authorization (`uplink oauth init`) and access-token refresh
//! (SPEC §13.3).

use crate::oauth::{self, Grant, LoopbackFlow};
use anyhow::{Context, Result};
use uplink_config::M365AccountConfig;
use uplink_storage::seal::MasterKey;
use url::Url;

/// Runs the authorization-code flow for `account` and stores the refresh
/// token it yields (see [`oauth::authorize_loopback`]). The access asked
/// for is the registration's permissions on the resource.
pub async fn authorize(
    account: &M365AccountConfig,
    key: &MasterKey,
    present: impl FnOnce(&Url),
    pasted: impl std::future::Future<Output = Result<Url>>,
) -> Result<()> {
    let auth_url = format!("{}/oauth2/authorize", authority(account));
    let grant = grant(account);
    let flow = LoopbackFlow {
        auth_url: &auth_url,
        token_url: &grant.endpoint,
        client_id: &account.oauth.client_id,
        client_secret: None,
        login_hint: &account.address,
        params: &[("resource", &account.oauth.resource)],
    };
    let refresh_token = oauth::authorize_loopback(&flow, present, pasted)
        .await?
        .context("Entra ID returned no refresh token")?;
    oauth::store(key, &grant, refresh_token)
}

fn authority(account: &M365AccountConfig) -> &str {
    account.oauth.authority.trim_end_matches('/')
}

/// How `account`'s refresh token is redeemed: for a token for the
/// resource, stated again, as the v1 endpoint allows any resource.
pub fn grant(account: &M365AccountConfig) -> Grant {
    Grant {
        account: account.id.clone(),
        token_file: account.oauth.token_file.clone(),
        endpoint: format!("{}/oauth2/token", authority(account)),
        refresh_form: vec![
            ("client_id", account.oauth.client_id.clone()),
            ("resource", account.oauth.resource.clone()),
        ],
    }
}

/// How `account`'s refresh token is redeemed for a token for Exchange's
/// IMAP: the same, for its resource (SPEC §8.3).
pub fn imap_grant(account: &M365AccountConfig) -> Grant {
    let mut grant = grant(account);
    grant.refresh_form = vec![
        ("client_id", account.oauth.client_id.clone()),
        ("resource", account.oauth.imap_resource.clone()),
    ];
    grant
}
