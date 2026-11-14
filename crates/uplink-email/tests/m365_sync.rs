//! Microsoft 365 sync against emulate's Microsoft Graph; not yet against
//! a real account (SPEC §15.10).

mod common;

use base64::{engine::general_purpose::STANDARD, Engine};
use common::{
    dovecot::Dovecot,
    eventually, message,
    uplink::{label_set, Uplink},
};
use reqwest::Method;
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use uplink_config::{M365AccountConfig, M365OAuthConfig};
use uplink_email::{
    imap::session::{self, Connection},
    m365::{
        api::Graph,
        oauth,
        sync::{self, M365Sync},
    },
    mail_server::sign_in::SignIn,
    oauth::Credentials,
};
use uplink_protocol::{compose::SendState, mail::MailProvider, ServerResponse};
use uplink_storage::seal::MasterKey;

const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";

/// emulate's Microsoft 365, with Alice's account authorized for uplink;
/// a Dovecot of hers is its Exchange's IMAP, signing her in by a token
/// Graph takes.
struct Microsoft {
    account: M365AccountConfig,
    exchange: Dovecot,
    base: String,
    http: reqwest::Client,
    alice: Credentials,
    _emulate: tokio::process::Child,
}

impl Microsoft {
    async fn start(dir: &Path) -> Self {
        let (base, emulate) = common::start_emulate(
            dir,
            "microsoft",
            &format!(
                "microsoft:\n  users:\n    - email: {ALICE}\n      name: Alice\n    \
                 - email: {BOB}\n      name: Bob\n"
            ),
        )
        .await;
        let key_path = dir.join("master.key");
        let key = Arc::new(MasterKey::open(&key_path).unwrap());
        let exchange = Dovecot::start_with_tokens(dir, &format!("{base}/v1.0/me")).await;
        let account = M365AccountConfig {
            id: "test".into(),
            address: ALICE.into(),
            name: None,
            identities: vec![],
            oauth: M365OAuthConfig {
                client_id: "uplink-test".into(),
                authority: format!("{base}/common"),
                resource: base.clone(),
                imap_resource: base.clone(),
                token_file: dir.join("accounts/test.token"),
            },
            imap: exchange.server(),
        };
        let http = reqwest::Client::new();
        authorize(&http, &account, &key).await;
        Self {
            alice: Credentials::new(http.clone(), oauth::grant(&account), key),
            account,
            exchange,
            base,
            http,
            _emulate: emulate,
        }
    }

    /// Another mail client of Alice's, at Exchange's IMAP.
    async fn exchange_client(&self) -> Connection {
        let sign_in = SignIn::OAuth(self.alice.redeemed_as(oauth::imap_grant(&self.account)));
        session::connect(&self.account.imap, ALICE, &sign_in, &self.exchange.tls)
            .await
            .unwrap()
    }

    /// The sync task's provider.
    fn provider(&self) -> M365Sync {
        // Of Alice's refresh token, as the test's own calls are, which
        // emulate revokes as it rotates it.
        let credentials = self.alice.redeemed_as(oauth::grant(&self.account));
        let exchange = credentials.redeemed_as(oauth::imap_grant(&self.account));
        M365Sync::new(
            Graph::new(self.http.clone(), &self.account.oauth.resource, credentials),
            sync::watched(&self.account),
            SignIn::OAuth(exchange),
            self.exchange.tls.clone(),
        )
    }

    /// An access token for another of the tenant's users.
    async fn token_of(&self, user: &str) -> String {
        let authority = &self.account.oauth.authority;
        let client_id = &self.account.oauth.client_id;
        let redirect_uri = "http://127.0.0.1/redirect";
        let mut url = url::Url::parse(&format!("{authority}/oauth2/authorize")).unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("resource", &self.account.oauth.resource);
        let redirected = url::Url::parse(&sign_in(&self.base, url, user).await).unwrap();
        let (_, code) = redirected.query_pairs().find(|(k, _)| k == "code").unwrap();
        let response: Value = self
            .http
            .post(format!("{}/oauth2/token", self.account.oauth.authority))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", redirect_uri),
                ("client_id", client_id),
            ])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        response["access_token"].as_str().unwrap().to_owned()
    }

    /// Calls Graph as Alice, or as the holder of `token`.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> Value {
        let token = match token {
            Some(token) => token.to_owned(),
            None => self.alice.access_token().await.unwrap(),
        };
        let mut request = self
            .http
            .request(method, format!("{}/v1.0/me{path}", self.base))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        response.json().await.unwrap_or(Value::Null)
    }

    /// Delivers a message to Alice's Inbox, unread; returns its id.
    async fn deliver(&self, raw: &[u8]) -> String {
        let response = self
            .http
            .post(format!("{}/v1.0/me/mailFolders/inbox/messages", self.base))
            .bearer_auth(self.alice.access_token().await.unwrap())
            .header("Content-Type", "text/plain")
            .body(STANDARD.encode(raw))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let message: Value = response.json().await.unwrap();
        message["id"].as_str().unwrap().to_owned()
    }

    async fn message(&self, id: &str) -> Value {
        self.call(Method::GET, &format!("/messages/{id}"), None, None)
            .await
    }

    async fn folder_id(&self, folder: &str) -> String {
        let folder = self
            .call(Method::GET, &format!("/mailFolders/{folder}"), None, None)
            .await;
        folder["id"].as_str().unwrap().to_owned()
    }

    async fn update(&self, id: &str, update: Value) {
        self.call(
            Method::PATCH,
            &format!("/messages/{id}"),
            Some(update),
            None,
        )
        .await;
    }

    async fn move_to(&self, id: &str, folder: &str) {
        self.call(
            Method::POST,
            &format!("/messages/{id}/move"),
            Some(json!({ "destinationId": folder })),
            None,
        )
        .await;
    }
}

/// Signs `user` in on emulate's page, where the authorization `url`
/// leads, as a browser would; returns where Entra ID redirected to.
async fn sign_in(base: &str, url: url::Url, user: &str) -> String {
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let page = browser.get(url).send().await.unwrap();
    assert_eq!(page.status(), 302);
    let page = url::Url::parse(base)
        .unwrap()
        .join(page.headers()["location"].to_str().unwrap())
        .unwrap();
    let params: HashMap<String, String> = page.query_pairs().into_owned().collect();
    let mut form: Vec<(&str, &str)> = [
        "redirect_uri",
        "scope",
        "client_id",
        "state",
        "code_challenge",
        "code_challenge_method",
    ]
    .iter()
    .filter_map(|k| Some((*k, params.get(*k)?.as_str())))
    .collect();
    form.push(("email", user));
    let signed_in = browser
        .post(format!("{base}/oauth2/v2.0/authorize/callback"))
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(signed_in.status(), 302);
    let location = signed_in.headers()["location"].to_str().unwrap().to_owned();
    assert!(location.starts_with(&params["redirect_uri"]), "{location}");
    location
}

/// Runs `uplink oauth init`'s flow for `account`, signing Alice in as a
/// browser would: the redirect followed back to the flow's listener.
async fn authorize(http: &reqwest::Client, account: &M365AccountConfig, key: &MasterKey) {
    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
    let (authorized, ()) = tokio::join!(
        oauth::authorize(
            account,
            key,
            |url| url_tx.send(url.clone()).unwrap(),
            std::future::pending(),
        ),
        async {
            let location = sign_in(
                &account.oauth.resource,
                url_rx.await.unwrap(),
                &account.address,
            )
            .await;
            assert!(http
                .get(location)
                .send()
                .await
                .unwrap()
                .status()
                .is_success());
        },
    );
    authorized.unwrap();
}

fn dispatched(response: ServerResponse) -> uplink_protocol::TxLogId {
    let ServerResponse::Dispatched { tx_log_id } = response else {
        panic!("{response:?}")
    };
    tx_log_id
}

/// The Inbox is watched by IDLE at Exchange's IMAP, signed in by a token
/// for it (SPEC §8.3): what it tells of wakes the sync, which takes
/// Graph's deltas, though the poll is an hour away.
#[tokio::test(flavor = "multi_thread")]
async fn the_inbox_is_watched_by_idle_at_exchange() {
    let uplink = Uplink::new(MailProvider::M365);
    let microsoft = Microsoft::start(uplink.dir.path()).await;
    let sync = uplink.start_polling(microsoft.provider(), ALICE, Duration::from_secs(3600));
    eventually("the first sync", || async { uplink.synced().then_some(()) }).await;
    // The watch begun, its beginning's sync taken.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(uplink.watch_failing().await, None);

    // Graph's mailbox has it; the poll does not come.
    microsoft.deliver(&message("pushed", "")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(uplink.entry("pushed"), None, "not polled meanwhile");
    // Exchange's IMAP tells of it.
    let mut other = microsoft.exchange_client().await;
    other
        .session
        .append("INBOX", None, None, message("told", "").as_slice())
        .await
        .unwrap();
    uplink.arrived("pushed").await;
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn mirrors_the_mailbox_and_pushes_changes() {
    let uplink = Uplink::new(MailProvider::M365);
    let microsoft = Microsoft::start(uplink.dir.path()).await;
    let projects = microsoft
        .call(
            Method::POST,
            "/mailFolders/inbox/childFolders",
            Some(json!({ "displayName": "Projects" })),
            None,
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Mail present before the first sync, and mail arriving afterwards.
    let first = microsoft.deliver(&message("first", "hello world")).await;
    let sync = uplink.start(microsoft.provider(), ALICE);
    let first_entry = uplink.arrived("first").await;
    assert_eq!(
        uplink.labels(first_entry),
        [("INBOX".into(), true, true), ("UNREAD".into(), true, true)]
    );
    let name: String = uplink
        .reader()
        .query_row(
            "SELECT name FROM label WHERE external_id = ?1 AND kind = 'folder'",
            [&projects],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name, "Inbox/Projects");
    let second = microsoft
        .deliver(&message("second", "a quixotic body"))
        .await;
    let second_entry = uplink.arrived("second").await;

    // Archiving moves the message to the Archive; undo moves it back.
    let archive = microsoft.folder_id("archive").await;
    let inbox = microsoft.folder_id("inbox").await;
    let archived = dispatched(
        uplink
            .mail
            .modify_labels(vec![first_entry], vec![], vec![uplink.label("INBOX")])
            .await
            .unwrap(),
    );
    eventually("the archive to reach Graph", || async {
        (microsoft.message(&first).await["parentFolderId"] == archive.as_str()).then_some(())
    })
    .await;
    eventually("the archive to be observed", || async {
        (uplink.labels(first_entry)
            == [
                ("ARCHIVE".into(), true, true),
                ("UNREAD".into(), true, true),
            ])
        .then_some(())
    })
    .await;
    uplink.dispatcher.undo(archived).await.unwrap();
    eventually("the undo to reach Graph", || async {
        (microsoft.message(&first).await["parentFolderId"] == inbox.as_str()).then_some(())
    })
    .await;

    // Starring and reading flag the message and mark it read.
    uplink
        .mail
        .modify_labels(
            vec![second_entry],
            vec![uplink.label("STARRED")],
            vec![uplink.label("UNREAD")],
        )
        .await
        .unwrap();
    eventually("the star to reach Graph", || async {
        let message = microsoft.message(&second).await;
        (message["flag"]["flagStatus"] == "flagged" && message["isRead"] == true).then_some(())
    })
    .await;

    // Changes made in Outlook show up in uplink: a move, importance and a
    // category; then a deletion.
    microsoft.move_to(&second, &projects).await;
    microsoft
        .update(
            &second,
            json!({ "importance": "high", "categories": ["Red"] }),
        )
        .await;
    eventually("the move to be observed", || async {
        let labels = uplink.labels(second_entry);
        let expected = ["IMPORTANT", projects.as_str(), "STARRED", "cat:Red"];
        let mut expected = expected.to_vec();
        expected.sort();
        (label_set(&labels) == expected).then_some(())
    })
    .await;
    microsoft
        .call(Method::DELETE, &format!("/messages/{first}"), None, None)
        .await;
    eventually("the deletion to be observed", || async {
        uplink.entry("first").is_none().then_some(())
    })
    .await;

    // Removing a category in uplink removes it in Outlook.
    uplink
        .mail
        .modify_labels(vec![second_entry], vec![], vec![uplink.label("cat:Red")])
        .await
        .unwrap();
    eventually("the category removal to reach Graph", || async {
        (microsoft.message(&second).await["categories"] == json!([])).then_some(())
    })
    .await;

    // A restarted sync resumes from the recorded delta links: idle polls
    // record nothing, and new mail still arrives.
    eventually("the removal to be observed", || async {
        (!label_set(&uplink.labels(second_entry)).contains(&"cat:Red")).then_some(())
    })
    .await;
    // The poll after a push reports the change pushed.
    let settled = uplink.settled().await;
    sync.abort();
    let _ = sync.await;
    let sync = uplink.start(microsoft.provider(), ALICE);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(uplink.log_len(), settled, "the restarted sync re-synced");
    microsoft.deliver(&message("third", "after restart")).await;
    uplink.arrived("third").await;
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_a_folder_anew_once_its_delta_link_expired() {
    let uplink = Uplink::new(MailProvider::M365);
    let microsoft = Microsoft::start(uplink.dir.path()).await;
    let kept = microsoft.deliver(&message("kept", "")).await;
    let deleted = microsoft.deliver(&message("deleted", "")).await;
    let sync = uplink.start(microsoft.provider(), ALICE);
    let kept_entry = uplink.arrived("kept").await;
    uplink.arrived("deleted").await;
    sync.abort();
    let _ = sync.await;

    // Changes while uplink is away, and an Inbox delta link Graph has
    // forgotten.
    microsoft.update(&kept, json!({ "isRead": true })).await;
    microsoft
        .call(Method::DELETE, &format!("/messages/{deleted}"), None, None)
        .await;
    microsoft.deliver(&message("new", "")).await;
    let inbox = microsoft.folder_id("inbox").await;
    uplink_storage::open(uplink.dir.path())
        .unwrap()
        .execute(
            "UPDATE account SET sync_state = json_set(sync_state, '$.folders.' || ?1,
               replace(json_extract(sync_state, '$.folders.' || ?1), '$deltatoken=', '$deltatoken=999'))",
            [&inbox],
        )
        .unwrap();

    let sync = uplink.start(microsoft.provider(), ALICE);
    uplink.arrived("new").await;
    eventually("the deletion to be observed", || async {
        uplink.entry("deleted").is_none().then_some(())
    })
    .await;
    assert_eq!(uplink.labels(kept_entry), [("INBOX".into(), true, true)]);
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_mail_keeping_its_message_id() {
    let uplink = Uplink::new(MailProvider::M365);
    let microsoft = Microsoft::start(uplink.dir.path()).await;
    microsoft.deliver(&message("original", "lunch?")).await;
    let sync = uplink.start(microsoft.provider(), ALICE);
    let original_entry = uplink.arrived("original").await;
    let (original_message, original_message_id, original_thread) =
        uplink.message_of(original_entry);

    // A reply to Bob: sent after the delay, delivered with uplink's
    // Message-ID, and synced back from Sent Items as the same message, in
    // the original's conversation.
    let reply = format!("reply {}", common::nonce());
    uplink.send(BOB, &reply, Some(original_message)).await;
    assert!(matches!(
        &uplink.outbox().await[..],
        [(_, SendState::Queued)]
    ));
    let reply_entry = uplink.arrived("reply").await;
    let (_, reply_message_id, reply_thread) = uplink.message_of(reply_entry);
    assert_eq!(reply_thread, original_thread);
    assert_eq!(label_set(&uplink.labels(reply_entry)), ["SENT"]);
    eventually("the outbox to empty", || async {
        uplink.outbox().await.is_empty().then_some(())
    })
    .await;

    let bob = microsoft.token_of(BOB).await;
    let received = microsoft
        .call(
            Method::GET,
            "/mailFolders/inbox/messages/delta",
            None,
            Some(&bob),
        )
        .await;
    let received = &received["value"][0];
    assert_eq!(received["subject"], reply.as_str());
    assert_eq!(
        received["internetMessageId"],
        format!("<{reply_message_id}>")
    );
    let id = received["id"].as_str().unwrap();
    let raw = microsoft
        .http
        .get(format!("{}/v1.0/me/messages/{id}/$value", microsoft.base))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        raw.contains(&format!("In-Reply-To: <{original_message_id}>")),
        "{raw}"
    );

    // Sent again, as if its outcome had been unknown and settled as not
    // sent: it is sent as the same draft, which went out already, so Bob
    // gets it once only.
    sync.abort();
    let _ = sync.await;
    let db = uplink_storage::open(uplink.dir.path()).unwrap();
    let requeued = db
        .execute(
            "UPDATE mail_send SET state = 'queued', message = NULL, synced = 0, not_before = 0
             WHERE provider_draft IS NOT NULL",
            [],
        )
        .unwrap();
    assert_eq!(requeued, 1);
    let sync = uplink.start(microsoft.provider(), ALICE);
    eventually("the send to be sent again", || async {
        let state: String = db
            .query_row("SELECT state FROM mail_send", [], |r| r.get(0))
            .unwrap();
        (state == "sent").then_some(())
    })
    .await;
    let received = microsoft
        .call(
            Method::GET,
            "/mailFolders/inbox/messages/delta",
            None,
            Some(&bob),
        )
        .await;
    assert_eq!(received["value"].as_array().unwrap().len(), 1, "{received}");
    sync.abort();
}
