//! The parts of the Gmail REST API uplink uses (SPEC §8.1).

use super::oauth::Credentials;
use crate::account::SendError;
use anyhow::{anyhow, bail, Context, Result};
use base64::{
    alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
    Engine,
};
use reqwest::{Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Gmail's base64url, with or without padding.
const BASE64URL: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Clone)]
pub struct Gmail {
    http: reqwest::Client,
    /// `.../gmail/v1/users/me`
    base: String,
    /// `.../upload/gmail/v1/users/me`, for media.
    upload: String,
    credentials: Credentials,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub email_address: String,
    pub history_id: String,
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
        }
    }

    pub async fn profile(&self) -> Result<Profile> {
        self.get("/profile", &[]).await?.context("no profile")
    }

    pub async fn labels(&self) -> Result<Vec<Label>> {
        #[derive(Deserialize)]
        struct Labels {
            #[serde(default)]
            labels: Vec<Label>,
        }
        let labels: Labels = self.get("/labels", &[]).await?.context("no labels")?;
        Ok(labels.labels)
    }

    /// A page of all messages, including spam and trash, newest first.
    pub async fn messages(&self, page_token: Option<&str>) -> Result<MessagePage> {
        let mut query = vec![("maxResults", "500"), ("includeSpamTrash", "true")];
        query.extend(page_token.map(|t| ("pageToken", t)));
        self.get("/messages", &query)
            .await?
            .context("no message list")
    }

    /// `None` if the message no longer exists.
    pub async fn message(&self, id: &str, format: Format) -> Result<Option<Message>> {
        let format = match format {
            Format::Minimal => "minimal",
            Format::Raw => "raw",
        };
        self.get(&format!("/messages/{id}"), &[("format", format)])
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
        self.get("/history", &query).await
    }

    pub async fn batch_modify(&self, request: &BatchModify<'_>) -> Result<()> {
        let response = self
            .send(Method::POST, "/messages/batchModify", &[], Some(request))
            .await?;
        check(response).await?;
        Ok(())
    }

    /// Sends the RFC 5322 message `raw`, in thread `thread_id` if given.
    /// Sends `raw` as a multipart upload, which takes messages of up to
    /// 35 MB; a JSON body with the message inline is limited to 5 MB.
    pub async fn send_message(&self, raw: &[u8], thread_id: Option<&str>) -> Result<(), SendError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Metadata<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            thread_id: Option<&'a str>,
        }
        let metadata = serde_json::to_string(&Metadata { thread_id }).expect("serializes");
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
        let content_type = format!("multipart/related; boundary={boundary}");
        let url = format!("{}/messages/send?uploadType=multipart", self.upload);
        for retry in [false, true] {
            let token = self
                .credentials
                .access_token()
                .await
                .map_err(SendError::NotSent)?;
            let response = match self
                .http
                .post(&url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, &content_type)
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
                return Ok(());
            }
            if status == StatusCode::UNAUTHORIZED && !retry {
                self.credentials.invalidate().await;
                continue;
            }
            let body = response.text().await.unwrap_or_default();
            if status.is_server_error()
                || status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::UNAUTHORIZED
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
    ) -> Result<Option<T>> {
        let response = self.send::<()>(Method::GET, path, query, None).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = check(response).await?;
        let body = response.bytes().await?;
        serde_json::from_slice(&body)
            .with_context(|| format!("decoding {path}: {}", String::from_utf8_lossy(&body)))
            .map(Some)
    }

    /// Sends a request, refreshing the access token once if it is refused.
    async fn send<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&B>,
    ) -> Result<reqwest::Response> {
        let url = format!("{}{path}", self.base);
        for retry in [false, true] {
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
            if response.status() == StatusCode::UNAUTHORIZED && !retry {
                self.credentials.invalidate().await;
                continue;
            }
            return Ok(response);
        }
        unreachable!("the second attempt returns")
    }
}

async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let url = response.url().path().to_owned();
    let body = response.text().await.unwrap_or_default();
    bail!("{url} failed with {status}: {body}")
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
