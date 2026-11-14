//! The parts of the Gmail REST API uplink uses (SPEC §8.1).

use crate::account::SendError;
use crate::oauth::Credentials;
use anyhow::{anyhow, bail, Context, Result};
use base64::{
    alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
    Engine,
};
use reqwest::{Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

/// Gmail's base64url, with or without padding.
const BASE64URL: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Gmail's quota units a user's requests may spend each second, on
/// average: Gmail allows 6,000 a minute per user (to projects created
/// since 1 May 2026, as the user's must be; older ones keep 15,000), and
/// refuses requests beyond that for the rest of the minute.
const UNITS_PER_SECOND: f64 = 80.0;
/// Units that may be spent at once, after a pause.
const UNITS_AT_ONCE: f64 = 400.0;
/// The longest wait before trying again a request refused for rate; the
/// waits double from a second up to this.
const MAX_RATE_WAIT: Duration = Duration::from_secs(64);

/// What each request costs, in quota units (Gmail's "Usage limits", as
/// of May 2026).
mod cost {
    pub const PROFILE: u32 = 1;
    pub const LABELS: u32 = 1;
    pub const MESSAGES: u32 = 5;
    /// `messages.get?format=minimal`.
    pub const MESSAGE_MINIMAL: u32 = 20;
    /// `messages.get?format=raw` (as `full` and `html`).
    pub const MESSAGE_RAW: u32 = 60;
    pub const HISTORY: u32 = 2;
    pub const BATCH_MODIFY: u32 = 50;
    pub const DRAFTS_CREATE: u32 = 10;
    pub const DRAFTS_SEND: u32 = 100;
    /// `threads.get`.
    pub const THREAD: u32 = 10;
    pub const WATCH: u32 = 100;
    /// `settings.sendAs.list`.
    pub const SEND_AS: u32 = 1;
}

/// One account's client. Its clones share the account's quota.
#[derive(Clone)]
pub struct Gmail {
    http: reqwest::Client,
    /// `.../gmail/v1/users/me`
    base: String,
    /// `.../upload/gmail/v1/users/me`, for media.
    upload: String,
    credentials: Credentials,
    pace: Arc<Pace>,
}

/// Keeps a user's requests within Gmail's quota: a token bucket of quota
/// units, refilled at [`UNITS_PER_SECOND`] up to [`UNITS_AT_ONCE`].
#[derive(Debug)]
struct Pace(Mutex<Bucket>);

#[derive(Debug)]
struct Bucket {
    /// Negative once requests are waiting for units.
    units: f64,
    at: Instant,
}

impl Pace {
    fn new() -> Self {
        Self(Mutex::new(Bucket {
            units: UNITS_AT_ONCE,
            at: Instant::now(),
        }))
    }

    /// Spends `cost` units, waiting until they are there.
    async fn spend(&self, cost: u32) {
        let wait = {
            let mut bucket = self.0.lock().expect("pace poisoned");
            let now = Instant::now();
            let refilled = (now - bucket.at).as_secs_f64() * UNITS_PER_SECOND;
            bucket.units = (bucket.units + refilled).min(UNITS_AT_ONCE) - f64::from(cost);
            bucket.at = now;
            Duration::from_secs_f64((-bucket.units).max(0.0) / UNITS_PER_SECOND)
        };
        tokio::time::sleep(wait).await;
    }
}

/// An answer to a request, read whole.
struct Reply {
    path: String,
    status: StatusCode,
    body: Vec<u8>,
}

impl Reply {
    /// Gmail's reason, if it refused the request for what it asked: a 4xx
    /// other than for authorization or rate, which passes.
    fn refusal(&self) -> Option<String> {
        (self.status.is_client_error()
            && self.status != StatusCode::UNAUTHORIZED
            && !refused_for_rate(self.status, &self.body))
        .then(|| {
            let body = String::from_utf8_lossy(&self.body).into_owned();
            error_message(&body).unwrap_or(body)
        })
    }

    fn check(self) -> Result<Self> {
        if self.status.is_success() {
            return Ok(self);
        }
        bail!(
            "{} failed with {}: {}",
            self.path,
            self.status,
            String::from_utf8_lossy(&self.body)
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub email_address: String,
    pub history_id: String,
    /// In every label, Spam and Trash too.
    pub messages_total: u32,
}

/// An address Gmail sends from (`users.settings.sendAs`): the account's
/// own, or an alias added in its settings ("Send mail as").
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAs {
    pub send_as_email: String,
    #[serde(default)]
    pub is_primary: bool,
    /// Of an alias: `accepted` once its owner has verified it.
    #[serde(default)]
    pub verification_status: Option<String>,
}

impl SendAs {
    /// Whether Gmail sends from it: mail from any other address it sends
    /// from the account's own instead.
    pub fn usable(&self) -> bool {
        self.is_primary || self.verification_status.as_deref() == Some("accepted")
    }
}

#[derive(Debug, Deserialize)]
pub struct Label {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRef {
    pub id: String,
}

/// A draft just made.
#[derive(Debug)]
pub struct Drafted {
    pub id: String,
    /// The thread it is in, which it is sent in.
    pub thread: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    #[serde(default)]
    pub messages: Vec<MessageRef>,
    pub next_page_token: Option<String>,
}

/// A message, as `format=minimal` or `format=raw` returns it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub thread_id: Option<String>,
    #[serde(default)]
    pub label_ids: Vec<String>,
    /// Unix ms, as a string.
    pub internal_date: String,
    raw: Option<String>,
}

impl Message {
    /// The raw RFC 5322 bytes; only `format=raw` has them.
    pub fn raw(&self) -> Result<Vec<u8>> {
        let raw = self
            .raw
            .as_ref()
            .with_context(|| format!("message {} was fetched without its content", self.id))?;
        BASE64URL
            .decode(raw)
            .with_context(|| format!("message {} has malformed content", self.id))
    }

    pub fn internal_date(&self) -> Result<i64> {
        self.internal_date.parse().with_context(|| {
            format!(
                "message {} has internalDate {}",
                self.id, self.internal_date
            )
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    #[serde(default)]
    pub history: Vec<HistoryRecord>,
    pub history_id: String,
    pub next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryRecord {
    /// Every message the record changed.
    #[serde(default)]
    pub messages: Vec<MessageRef>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchModify<'a> {
    pub ids: &'a [String],
    pub add_label_ids: &'a [String],
    pub remove_label_ids: &'a [String],
}

#[derive(Clone, Copy)]
pub enum Format {
    Minimal,
    Raw,
}

impl Gmail {
    pub fn new(http: reqwest::Client, api: &str, credentials: Credentials) -> Self {
        Self {
            http,
            base: format!("{}/gmail/v1/users/me", api.trim_end_matches('/')),
            upload: format!("{}/upload/gmail/v1/users/me", api.trim_end_matches('/')),
            credentials,
            pace: Arc::new(Pace::new()),
        }
    }

    pub async fn profile(&self) -> Result<Profile> {
        self.get("/profile", &[], cost::PROFILE)
            .await?
            .context("no profile")
    }

    /// The addresses the account sends from.
    pub async fn send_as(&self) -> Result<Vec<SendAs>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct List {
            #[serde(default)]
            send_as: Vec<SendAs>,
        }
        let list: List = self
            .get("/settings/sendAs", &[], cost::SEND_AS)
            .await?
            .context("no send-as addresses")?;
        Ok(list.send_as)
    }

    pub async fn labels(&self) -> Result<Vec<Label>> {
        #[derive(Deserialize)]
        struct Labels {
            #[serde(default)]
            labels: Vec<Label>,
        }
        let labels: Labels = self
            .get("/labels", &[], cost::LABELS)
            .await?
            .context("no labels")?;
        Ok(labels.labels)
    }

    /// A page of all messages, including spam and trash, newest first.
    pub async fn messages(&self, page_token: Option<&str>) -> Result<MessagePage> {
        let mut query = vec![("maxResults", "500"), ("includeSpamTrash", "true")];
        query.extend(page_token.map(|t| ("pageToken", t)));
        self.get("/messages", &query, cost::MESSAGES)
            .await?
            .context("no message list")
    }

    /// `None` if the message no longer exists.
    pub async fn message(&self, id: &str, format: Format) -> Result<Option<Message>> {
        let (format, cost) = match format {
            Format::Minimal => ("minimal", cost::MESSAGE_MINIMAL),
            Format::Raw => ("raw", cost::MESSAGE_RAW),
        };
        self.get(&format!("/messages/{id}"), &[("format", format)], cost)
            .await
    }

    /// `None` if `start` is too old: history is no longer available from
    /// there, and the mailbox needs a full sync.
    pub async fn history(
        &self,
        start: &str,
        page_token: Option<&str>,
    ) -> Result<Option<HistoryPage>> {
        let mut query = vec![("startHistoryId", start), ("maxResults", "500")];
        query.extend(page_token.map(|t| ("pageToken", t)));
        self.get("/history", &query, cost::HISTORY).await
    }

    /// Has Gmail publish the mailbox's changes to the Pub/Sub topic
    /// `topic`, for a week.
    pub async fn watch(&self, topic: &str) -> Result<()> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Watch<'a> {
            topic_name: &'a str,
        }
        self.send(
            Method::POST,
            "/watch",
            &[],
            Some(&Watch { topic_name: topic }),
            cost::WATCH,
        )
        .await?
        .check()?;
        Ok(())
    }

    /// `Err` with Gmail's reason if it refused the change.
    pub async fn batch_modify(&self, request: &BatchModify<'_>) -> Result<Result<(), String>> {
        let reply = self
            .send(
                Method::POST,
                "/messages/batchModify",
                &[],
                Some(request),
                cost::BATCH_MODIFY,
            )
            .await?;
        if let Some(reason) = reply.refusal() {
            return Ok(Err(reason));
        }
        reply.check()?;
        Ok(Ok(()))
    }

    /// Makes the RFC 5322 message `raw` a draft, in thread `thread_id` if
    /// given. This sends nothing, so whatever its outcome it counts as not
    /// sent. Uploads `raw` as multipart, which takes messages of up to
    /// 35 MB; a JSON body with the message inline is limited to 5 MB.
    pub async fn create_draft(
        &self,
        raw: &[u8],
        thread_id: Option<&str>,
    ) -> Result<Drafted, SendError> {
        #[derive(Serialize)]
        struct Metadata<'a> {
            message: MessageMetadata<'a>,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct MessageMetadata<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            thread_id: Option<&'a str>,
        }
        #[derive(Deserialize)]
        struct Draft {
            id: String,
            message: DraftMessage,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct DraftMessage {
            thread_id: String,
        }
        let metadata = serde_json::to_string(&Metadata {
            message: MessageMetadata { thread_id },
        })
        .expect("serializes");
        // Random, so not in the message.
        let boundary: String = rand::random::<[u8; 16]>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let body = [
            format!(
                "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata}\r\n\
                 --{boundary}\r\nContent-Type: message/rfc822\r\n\r\n"
            )
            .as_bytes(),
            raw,
            format!("\r\n--{boundary}--\r\n").as_bytes(),
        ]
        .concat();
        let url = format!("{}/drafts?uploadType=multipart", self.upload);
        let content_type = format!("multipart/related; boundary={boundary}");
        let reply = match self
            .post(&url, &content_type, body, cost::DRAFTS_CREATE)
            .await
        {
            Err(SendError::Unknown(e)) => return Err(SendError::NotSent(e)),
            reply => reply?,
        };
        let draft: Draft = reply
            .context("Gmail answered 404 to creating a draft")
            .and_then(|body| {
                serde_json::from_slice(&body).with_context(|| {
                    format!("decoding the draft: {}", String::from_utf8_lossy(&body))
                })
            })
            .map_err(SendError::NotSent)?;
        Ok(Drafted {
            id: draft.id,
            thread: draft.message.thread_id,
        })
    }

    /// Sends the draft `id`, as a new message (SPEC §8.2); that message's
    /// id. `None` if there is no such draft: one sent is gone, so sending
    /// it again sends nothing.
    pub async fn send_draft(&self, id: &str) -> Result<Option<String>, SendError> {
        #[derive(Serialize)]
        struct Request<'a> {
            id: &'a str,
        }
        let url = format!("{}/drafts/send", self.base);
        let body = serde_json::to_vec(&Request { id }).expect("serializes");
        let Some(body) = self
            .post(&url, "application/json", body, cost::DRAFTS_SEND)
            .await?
        else {
            return Ok(None);
        };
        let sent: MessageRef = serde_json::from_slice(&body)
            .with_context(|| {
                format!(
                    "decoding the sent message: {}",
                    String::from_utf8_lossy(&body)
                )
            })
            .map_err(SendError::Unknown)?;
        Ok(Some(sent.id))
    }

    /// The messages of thread `id`, as `format=minimal` gives them; `None`
    /// if there is no such thread.
    pub async fn thread(&self, id: &str) -> Result<Option<Vec<Message>>> {
        #[derive(Deserialize)]
        struct Thread {
            #[serde(default)]
            messages: Vec<Message>,
        }
        let thread: Option<Thread> = self
            .get(
                &format!("/threads/{id}"),
                &[("format", "minimal")],
                cost::THREAD,
            )
            .await?;
        Ok(thread.map(|t| t.messages))
    }

    /// POSTs `body`, a request that may send mail, refreshing the access
    /// token once if it is refused; the answer's body, or `None` on 404.
    /// Fails as a send does: before the request went out, or for a passing
    /// reason, [`SendError::NotSent`]; refused for what it asks,
    /// [`SendError::Rejected`]; otherwise [`SendError::Unknown`].
    async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
        cost: u32,
    ) -> Result<Option<Vec<u8>>, SendError> {
        for retry in [false, true] {
            self.pace.spend(cost).await;
            let token = self
                .credentials
                .access_token()
                .await
                .map_err(SendError::NotSent)?;
            let response = match self
                .http
                .post(url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body.clone())
                .send()
                .await
            {
                Ok(response) => response,
                Err(e) if e.is_connect() => return Err(SendError::NotSent(e.into())),
                Err(e) => return Err(SendError::Unknown(e.into())),
            };
            let status = response.status();
            if status.is_success() {
                return response
                    .bytes()
                    .await
                    .map(|body| Some(body.into()))
                    .map_err(|e| SendError::Unknown(e.into()));
            }
            if status == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if status == StatusCode::UNAUTHORIZED && !retry {
                self.credentials.invalidate().await;
                continue;
            }
            let body = response.text().await.unwrap_or_default();
            if status.is_server_error()
                || status == StatusCode::UNAUTHORIZED
                || refused_for_rate(status, body.as_bytes())
            {
                return Err(SendError::NotSent(anyhow!(
                    "Gmail answered {status}: {body}"
                )));
            }
            return Err(SendError::Rejected(error_message(&body).unwrap_or(body)));
        }
        unreachable!("the second attempt returns")
    }

    /// `None` on 404.
    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        cost: u32,
    ) -> Result<Option<T>> {
        let reply = self
            .send::<()>(Method::GET, path, query, None, cost)
            .await?;
        if reply.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let reply = reply.check()?;
        serde_json::from_slice(&reply.body)
            .with_context(|| format!("decoding {path}: {}", String::from_utf8_lossy(&reply.body)))
            .map(Some)
    }

    /// Sends a request costing `cost` quota units, within the quota:
    /// refreshing the access token once if it is refused, and trying again
    /// after a wait while it is refused for rate.
    async fn send<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&B>,
        cost: u32,
    ) -> Result<Reply> {
        let url = format!("{}{path}", self.base);
        let mut refreshed = false;
        let mut wait = Duration::from_secs(1);
        loop {
            self.pace.spend(cost).await;
            let mut request = self
                .http
                .request(method.clone(), &url)
                .query(query)
                .bearer_auth(self.credentials.access_token().await?);
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request
                .send()
                .await
                .with_context(|| format!("{method} {path}"))?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                self.credentials.invalidate().await;
                continue;
            }
            let reply = Reply {
                path: path.to_owned(),
                status,
                body: response
                    .bytes()
                    .await
                    .with_context(|| format!("{method} {path}"))?
                    .into(),
            };
            if refused_for_rate(status, &reply.body) && wait <= MAX_RATE_WAIT {
                tracing::info!("Gmail refused {method} {path} for rate; trying again in {wait:?}");
                tokio::time::sleep(wait).await;
                wait *= 2;
                continue;
            }
            return Ok(reply);
        }
    }
}

/// Whether Gmail refused a request for going over a rate limit: with 429,
/// or with 403 and a rate limit's reason (as it does for the per-user
/// quota).
fn refused_for_rate(status: StatusCode, body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Response {
        error: Error,
    }
    #[derive(Deserialize)]
    struct Error {
        #[serde(default)]
        errors: Vec<Reason>,
    }
    #[derive(Deserialize)]
    struct Reason {
        reason: String,
    }
    match status {
        StatusCode::TOO_MANY_REQUESTS => true,
        StatusCode::FORBIDDEN => serde_json::from_slice::<Response>(body).is_ok_and(|r| {
            r.error.errors.iter().any(|e| {
                matches!(
                    e.reason.as_str(),
                    "rateLimitExceeded" | "userRateLimitExceeded"
                )
            })
        }),
        _ => false,
    }
}

/// The message of a Google API error response.
fn error_message(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Response {
        error: Error,
    }
    #[derive(Deserialize)]
    struct Error {
        message: String,
    }
    serde_json::from_str::<Response>(body)
        .ok()
        .map(|r| r.error.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn spends_at_once_while_the_budget_lasts_then_at_its_rate() {
        let pace = Pace::new();
        let start = Instant::now();
        // 80 listings at 5 units: 400 at once, then 80 a second.
        for _ in 0..80 {
            pace.spend(cost::MESSAGES).await;
        }
        assert_eq!(Instant::now() - start, Duration::ZERO);
        for _ in 0..16 {
            pace.spend(cost::MESSAGES).await;
        }
        assert_eq!(Instant::now() - start, Duration::from_secs(1));
        // A pause refills the bucket, up to its size.
        tokio::time::sleep(Duration::from_secs(60)).await;
        let start = Instant::now();
        for _ in 0..81 {
            pace.spend(cost::MESSAGES).await;
        }
        let waited = Instant::now() - start;
        assert!(
            waited > Duration::ZERO && waited <= Duration::from_millis(63),
            "{waited:?}"
        );
    }

    #[test]
    fn tells_refusals_for_rate() {
        // As Gmail refuses once a user's per-minute quota is spent.
        let quota = br#"{"error": {"code": 403, "message": "Quota exceeded for quota metric 'Total Query Cost' and limit 'Units per minute per user' of service 'gmail.googleapis.com'",
            "errors": [{"message": "Quota exceeded", "domain": "usageLimits", "reason": "rateLimitExceeded"}],
            "status": "PERMISSION_DENIED"}}"#;
        assert!(refused_for_rate(StatusCode::FORBIDDEN, quota));
        let denied = br#"{"error": {"code": 403, "message": "Insufficient Permission",
            "errors": [{"message": "Insufficient Permission", "domain": "global", "reason": "insufficientPermissions"}]}}"#;
        assert!(!refused_for_rate(StatusCode::FORBIDDEN, denied));
        assert!(refused_for_rate(StatusCode::TOO_MANY_REQUESTS, b""));
        assert!(!refused_for_rate(StatusCode::BAD_REQUEST, quota));
    }
}
