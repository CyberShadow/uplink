//! Entra ID (Azure AD) v1 OAuth as a public client: the one-time
//! authorization (`uplink oauth init`) and access-token refresh
//! (SPEC §13.3).

use crate::oauth::{self, token_request, Grant};
use anyhow::{bail, ensure, Context, Result};
use uplink_config::M365AccountConfig;
use uplink_storage::seal::MasterKey;
use url::Url;

/// Where the user signs in for `account`.
pub fn authorize_url(account: &M365AccountConfig) -> Result<Url> {
    let mut url = Url::parse(&format!("{}/oauth2/authorize", authority(account)))
        .context("parsing oauth.authority")?;
    url.query_pairs_mut()
        .append_pair("client_id", &account.oauth.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &account.oauth.redirect_uri)
        .append_pair("response_mode", "query")
        .append_pair("login_hint", &account.address)
        .append_pair("resource", &account.oauth.resource);
    Ok(url)
}

/// The authorization code in what the user pasted: the URL (or URN)
/// Entra ID redirected to, a "…code=…" string, or the bare code.
pub fn pasted_code(pasted: &str) -> Result<String> {
    let pasted = pasted.trim();
    if let Ok(url) = Url::parse(pasted) {
        if url.query().is_some() {
            let params: std::collections::HashMap<String, String> =
                url.query_pairs().into_owned().collect();
            if let Some(error) = params.get("error") {
                let description = params.get("error_description").map_or("", |d| d.as_str());
                bail!("authorization failed: {error} {description}");
            }
            return params
                .get("code")
                .cloned()
                .context("the URL carries no authorization code");
        }
    }
    let code = match pasted.find("code=") {
        Some(at) => pasted[at + "code=".len()..]
            .split(['&', ' ', '\t'])
            .next()
            .expect("split yields one item"),
        None => pasted,
    };
    ensure!(
        !code.is_empty() && !code.contains(char::is_whitespace),
        "that is not an authorization code"
    );
    Ok(code.to_owned())
}

/// Redeems the authorization code, storing the refresh token it yields.
pub async fn redeem(
    http: &reqwest::Client,
    account: &M365AccountConfig,
    key: &MasterKey,
    code: &str,
) -> Result<()> {
    let grant = grant(account);
    let response = token_request(
        http,
        &grant.endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &account.oauth.redirect_uri),
            ("client_id", &account.oauth.client_id),
        ],
    )
    .await?;
    let refresh_token = response
        .refresh_token
        .context("Azure AD returned no refresh token")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_found_in_what_is_pasted() {
        let code = |s| pasted_code(s).unwrap();
        assert_eq!(
            code("https://login.microsoftonline.com/common/oauth2/nativeclient?code=0.AXo&session_state=x"),
            "0.AXo"
        );
        assert_eq!(code("Success code=0.AXo&state=s"), "0.AXo");
        assert_eq!(code("  0.AXo\n"), "0.AXo");
        assert!(pasted_code(
            "https://login.microsoftonline.com/common/oauth2/nativeclient?error=access_denied"
        )
        .is_err());
        assert!(pasted_code("not a code").is_err());
    }
}
