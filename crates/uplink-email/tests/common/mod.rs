//! The Gmail account provider-facing tests run against: vercel-labs/emulate
//! by default (`UPLINK_EMULATE`, set by the dev shell and `nix flake
//! check`), or a real account when `UPLINK_TEST_GMAIL_CONFIG` names an
//! uplink.yaml and `UPLINK_TEST_GMAIL_ACCOUNT` one of its Gmail accounts,
//! authorized with `uplink oauth init` (SPEC §15.10). Tests arrange
//! provider-side state through the Gmail API itself, so they work the same
//! in both modes.

#![allow(dead_code)]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use serde_json::json;
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use uplink_config::{AccountConfig, Config, GmailAccountConfig, GmailOAuthConfig, GoogleEndpoints};
use uplink_email::{
    gmail::oauth::{self, Credentials},
    seal::{self, MasterKey, Token},
};

pub struct Provider {
    pub account: GmailAccountConfig,
    pub key: Arc<MasterKey>,
    http: reqwest::Client,
    credentials: Credentials,
    emulate: Option<tokio::process::Child>,
    delivered: std::sync::Mutex<Vec<String>>,
}

/// Whether the tests run against a real account.
pub fn real() -> bool {
    std::env::var_os("UPLINK_TEST_GMAIL_CONFIG").is_some()
}

/// The real account the environment names, and its refresh token.
fn real_account() -> (GmailAccountConfig, Token) {
    let path = std::env::var_os("UPLINK_TEST_GMAIL_CONFIG").unwrap();
    let id = std::env::var("UPLINK_TEST_GMAIL_ACCOUNT")
        .expect("UPLINK_TEST_GMAIL_ACCOUNT names the account in UPLINK_TEST_GMAIL_CONFIG");
    let config = Config::load(Path::new(&path)).unwrap();
    let account = config
        .accounts
        .into_iter()
        .find_map(|account| match account {
            AccountConfig::Gmail(gmail) if gmail.id == id => Some(gmail),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Gmail account {id} in {}", path.display()));
    let key = MasterKey::load(
        config
            .master_key_file
            .as_ref()
            .expect("the configuration has a master_key_file"),
    )
    .unwrap();
    let token = seal::read_token(&key, &account.id, &account.oauth.token_file).unwrap();
    (account, token)
}

impl Provider {
    /// `history_limit` configures the emulator to keep only that many
    /// history records (only emulated tests may ask for it).
    pub async fn start(dir: &Path, history_limit: Option<u32>) -> Self {
        let key_path = dir.join("master.key");
        std::fs::write(&key_path, MasterKey::generate()).unwrap();
        let key = Arc::new(MasterKey::load(&key_path).unwrap());
        let token_file = dir.join("accounts/test.token");
        let http = reqwest::Client::new();

        let (account, emulate) = if real() {
            assert!(
                history_limit.is_none(),
                "a real account's history can't be limited"
            );
            // A copy of the account under the test's own key and
            // directory, so the configured one is left as it was.
            let (real, token) = real_account();
            let account = GmailAccountConfig {
                id: "test".into(),
                oauth: GmailOAuthConfig {
                    token_file,
                    ..real.oauth
                },
                ..real
            };
            seal::write_token(&key, &account.id, &token, &account.oauth.token_file).unwrap();
            (account, None)
        } else {
            let (base, child) = start_emulate(dir, history_limit).await;
            let account = GmailAccountConfig {
                id: "test".into(),
                address: "alice@example.com".into(),
                oauth: GmailOAuthConfig {
                    client_id: "uplink-test".into(),
                    client_secret: "uplink-test-secret".into(),
                    token_file,
                },
                endpoints: GoogleEndpoints {
                    auth: format!("{base}/o/oauth2/v2/auth"),
                    token: format!("{base}/oauth2/token"),
                    api: base.clone(),
                },
            };
            authorize(&http, &account, &key, &base).await;
            (account, Some(child))
        };
        Self {
            credentials: Credentials::new(http.clone(), account.clone(), key.clone()),
            account,
            key,
            http,
            emulate,
            delivered: Default::default(),
        }
    }

    pub fn emulated(&self) -> bool {
        self.emulate.is_some()
    }

    /// Delivers a message to the inbox, unread; returns its Gmail id.
    pub async fn deliver(&self, raw: &[u8]) -> String {
        #[derive(Deserialize)]
        struct Imported {
            id: String,
        }
        let imported: Imported = self
            .call(
                reqwest::Method::POST,
                "/messages/import",
                Some(json!({
                    "raw": URL_SAFE_NO_PAD.encode(raw),
                    "labelIds": ["INBOX", "UNREAD"],
                })),
            )
            .await
            .json()
            .await
            .unwrap();
        self.delivered.lock().unwrap().push(imported.id.clone());
        imported.id
    }

    pub async fn labels(&self, id: &str) -> Vec<String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Minimal {
            #[serde(default)]
            label_ids: Vec<String>,
        }
        let mut labels = self
            .call(
                reqwest::Method::GET,
                &format!("/messages/{id}?format=minimal"),
                None,
            )
            .await
            .json::<Minimal>()
            .await
            .unwrap()
            .label_ids;
        labels.sort();
        labels
    }

    pub async fn modify(&self, id: &str, add: &[&str], remove: &[&str]) {
        self.call(
            reqwest::Method::POST,
            &format!("/messages/{id}/modify"),
            Some(json!({ "addLabelIds": add, "removeLabelIds": remove })),
        )
        .await;
    }

    /// The status of `history.list` from `start`.
    pub async fn history_status(&self, start: &str) -> reqwest::StatusCode {
        let url = format!(
            "{}/gmail/v1/users/me/history",
            self.account.endpoints.api.trim_end_matches('/')
        );
        self.http
            .get(url)
            .query(&[("startHistoryId", start)])
            .bearer_auth(self.credentials.access_token().await.unwrap())
            .send()
            .await
            .unwrap()
            .status()
    }

    /// Deletes a message permanently (emulated accounts only: uplink's
    /// scope can't).
    pub async fn delete(&self, id: &str) {
        assert!(self.emulated());
        self.call(reqwest::Method::DELETE, &format!("/messages/{id}"), None)
            .await;
    }

    /// Has `clean_up` remove message `id` too.
    pub fn clean_up_later(&self, id: String) {
        self.delivered.lock().unwrap().push(id);
    }

    /// Moves what the tests delivered to the trash, so a real account
    /// stays as it was.
    pub async fn clean_up(&self) {
        let delivered = std::mem::take(&mut *self.delivered.lock().unwrap());
        for id in delivered {
            self.call(
                reqwest::Method::POST,
                &format!("/messages/{id}/trash"),
                None,
            )
            .await;
        }
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> reqwest::Response {
        let url = format!(
            "{}/gmail/v1/users/me{path}",
            self.account.endpoints.api.trim_end_matches('/')
        );
        let mut request = self
            .http
            .request(method, &url)
            .bearer_auth(self.credentials.access_token().await.unwrap());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        response
    }
}

async fn start_emulate(dir: &Path, history_limit: Option<u32>) -> (String, tokio::process::Child) {
    let emulate = std::env::var_os("UPLINK_EMULATE")
        .expect("UPLINK_EMULATE must name the emulate binary (the dev shell sets it)");
    let config = dir.join("emulate.yaml");
    std::fs::write(
        &config,
        format!(
            "google:\n{}  users:\n    - email: alice@example.com\n      name: Alice\n  \
             oauth_clients:\n    - client_id: uplink-test\n      client_secret: uplink-test-secret\n      \
             redirect_uris: [\"http://127.0.0.1/\"]\n",
            history_limit
                .map(|l| format!("  gmail_history_limit: {l}\n"))
                .unwrap_or_default(),
        ),
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = tokio::process::Command::new(emulate)
        .args(["start", "--service", "google", "--port", &port.to_string()])
        .arg("--config")
        .arg(&config)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let ready = format!("{base}/.well-known/openid-configuration");
    for _ in 0..300 {
        if reqwest::get(&ready)
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return (base, child);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("emulate did not start");
}

/// Runs `uplink oauth init`'s flow, signing in on emulate's form as a
/// browser would.
async fn authorize(
    http: &reqwest::Client,
    account: &GmailAccountConfig,
    key: &MasterKey,
    base: &str,
) {
    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
    let browser = async {
        let url: url::Url = url_rx.await.unwrap();
        let params: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        let mut form: Vec<(&str, &str)> = [
            "redirect_uri",
            "scope",
            "state",
            "client_id",
            "code_challenge",
            "code_challenge_method",
        ]
        .iter()
        .map(|k| (*k, params[*k].as_str()))
        .collect();
        form.push(("email", "alice@example.com"));
        let no_redirects = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let signed_in = no_redirects
            .post(format!("{base}/o/oauth2/v2/auth/callback"))
            .form(&form)
            .send()
            .await
            .unwrap();
        assert_eq!(signed_in.status(), 302);
        let location = signed_in.headers()["location"].to_str().unwrap().to_owned();
        assert!(location.starts_with(&params["redirect_uri"]));
        assert!(http
            .get(location)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
    };
    let (authorized, ()) = tokio::join!(
        oauth::authorize(
            account,
            key,
            |url| url_tx.send(url.clone()).unwrap(),
            std::future::pending(),
        ),
        browser,
    );
    authorized.unwrap();
}

/// Random, to tell the test run's messages apart from others'.
pub fn nonce() -> String {
    let mut nonce = [0u8; 8];
    rand::fill(&mut nonce);
    nonce.iter().map(|b| format!("{b:02x}")).collect()
}

/// A message unique to the test run, findable by `tag`.
pub fn message(tag: &str, body: &str) -> Vec<u8> {
    let nonce = nonce();
    format!(
        "Message-ID: <{tag}.{nonce}@uplink.test>\r\nFrom: Tester <tester@uplink.test>\r\n\
         To: alice@example.com\r\nDate: Tue, 3 Nov 2026 10:00:00 +0000\r\n\
         Subject: {tag} {nonce}\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

/// Polls `f` until it returns `Some`.
pub async fn eventually<T, F: std::future::Future<Output = Option<T>>>(
    what: &str,
    mut f: impl FnMut() -> F,
) -> T {
    for _ in 0..400 {
        if let Some(t) = f().await {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}
