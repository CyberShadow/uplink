//! The parts of Microsoft Graph's mail API uplink uses (SPEC §8.3–8.4).

use crate::account::SendError;
use crate::oauth::Credentials;
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{header, Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::sync::Arc;
use uplink_config::M365AccountConfig;
use uplink_storage::seal::MasterKey;

/// Ids that stay the same when a message moves between folders, times in
/// UTC, and pages of a size that keeps requests few.
const PREFER: &str = "IdType=\"ImmutableId\", outlook.timezone=\"UTC\", odata.maxpagesize=100";

/// The message properties sync needs.
const SELECT: &str =
    "parentFolderId,conversationId,receivedDateTime,isRead,flag,importance,categories";

#[derive(Clone)]
pub struct Graph {
    http: reqwest::Client,
    /// `.../v1.0/me`
    base: String,
    credentials: Credentials,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub child_folder_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub parent_folder_id: String,
    pub conversation_id: Option<String>,
    /// RFC 3339.
    pub received_date_time: String,
    pub is_read: bool,
    pub flag: Flag,
    pub importance: Importance,
    #[serde(default)]
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Flag {
    pub flag_status: FlagStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FlagStatus {
    NotFlagged,
    Flagged,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Importance {
    Low,
    Normal,
    High,
}

/// An item of a delta page: a message as it is now, or one that left the
/// folder.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Delta {
    Removed {
        id: String,
        #[serde(rename = "@removed")]
        _removed: serde_json::Value,
    },
    Message(Message),
}

/// A page of a delta query, of `T`s.
#[derive(Debug, Deserialize)]
pub struct DeltaPage<T> {
    pub value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

/// The changes `PATCH /messages/{id}` makes; what is `None` stays.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_read: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag: Option<Flag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<Importance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
}

impl MessageUpdate {
    pub fn is_empty(&self) -> bool {
        self.is_read.is_none()
            && self.flag.is_none()
            && self.importance.is_none()
            && self.categories.is_none()
    }
}

#[derive(Deserialize)]
struct List<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

impl Graph {
    /// The account's Graph, with its token sealed under `key`.
    pub fn of(config: &M365AccountConfig, key: Arc<MasterKey>, http: reqwest::Client) -> Self {
        let credentials = Credentials::new(http.clone(), super::oauth::grant(config), key);
        Self::new(http, &config.oauth.resource, credentials)
    }

    /// The API of `resource`, e.g. `https://graph.microsoft.com`.
    pub fn new(http: reqwest::Client, resource: &str, credentials: Credentials) -> Self {
        Self {
            http,
            base: format!("{}/v1.0/me", resource.trim_end_matches('/')),
            credentials,
        }
    }

    /// `None` if there is no such folder; `folder` is an id or a
    /// well-known name.
    pub async fn folder(&self, folder: &str) -> Result<Option<Folder>> {
        self.get(&self.url(&format!("/mailFolders/{folder}"))).await
    }

    /// The top-level folders, or `parent`'s children.
    pub async fn folders(&self, parent: Option<&str>) -> Result<Vec<Folder>> {
        let mut url = self.url(&match parent {
            None => "/mailFolders".to_owned(),
            Some(parent) => format!("/mailFolders/{parent}/childFolders"),
        });
        let mut folders = vec![];
        loop {
            let page: List<Folder> = self.get(&url).await?.context("no folder list")?;
            folders.extend(page.value);
            match page.next_link {
                Some(next) => url = next,
                None => return Ok(folders),
            }
        }
    }

    /// Where a folder's delta query starts, listing all its messages.
    pub fn delta_start(&self, folder: &str) -> String {
        self.url(&format!(
            "/mailFolders/{folder}/messages/delta?$select={SELECT}"
        ))
    }

    /// A page of a delta query, at a next or delta link; `None` if the
    /// delta link has expired, and the folder needs listing anew.
    pub async fn delta<T: DeserializeOwned>(&self, link: &str) -> Result<Option<DeltaPage<T>>> {
        let response = self.send::<()>(Method::GET, link, None).await?;
        if response.status() == StatusCode::GONE {
            return Ok(None);
        }
        decode(link, check(response).await?).await.map(Some)
    }

    /// `None` if the message no longer exists.
    pub async fn message(&self, id: &str) -> Result<Option<Message>> {
        self.get(&self.url(&format!("/messages/{id}?$select={SELECT}")))
            .await
    }

    /// The message's MIME; `None` if it no longer exists.
    pub async fn mime(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let response = self
            .send::<()>(
                Method::GET,
                &self.url(&format!("/messages/{id}/$value")),
                None,
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(check(response).await?.bytes().await?.to_vec()))
    }

    pub async fn update(&self, id: &str, update: &MessageUpdate) -> Result<Changed> {
        let url = self.url(&format!("/messages/{id}"));
        changed(self.send(Method::PATCH, &url, Some(update)).await?).await
    }

    /// Moves the message to `folder`, an id or a well-known name.
    pub async fn move_to(&self, id: &str, folder: &str) -> Result<Changed> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Move<'a> {
            destination_id: &'a str,
        }
        let url = self.url(&format!("/messages/{id}/move"));
        let body = Move {
            destination_id: folder,
        };
        changed(self.send(Method::POST, &url, Some(&body)).await?).await
    }

    /// Sends the RFC 5322 message `raw`, which Graph takes as base64 MIME,
    /// keeping a copy in Sent Items.
    pub async fn send_mail(&self, raw: &[u8]) -> Result<(), SendError> {
        let url = self.url("/sendMail");
        let body = STANDARD.encode(raw);
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
                .header(header::CONTENT_TYPE, "text/plain")
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
                    "Graph answered {status}: {body}"
                )));
            }
            return Err(SendError::Rejected(error_message(&body).unwrap_or(body)));
        }
        unreachable!("the second attempt returns")
    }

    /// The URL of `path` below `/me`.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// `None` on 404.
    async fn get<T: DeserializeOwned>(&self, url: &str) -> Result<Option<T>> {
        let response = self.send::<()>(Method::GET, url, None).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        decode(url, check(response).await?).await.map(Some)
    }

    /// Sends a request, refreshing the access token once if it is refused.
    async fn send<B: Serialize>(
        &self,
        method: Method,
        url: &str,
        body: Option<&B>,
    ) -> Result<reqwest::Response> {
        for retry in [false, true] {
            let mut request = self
                .http
                .request(method.clone(), url)
                .header("Prefer", PREFER)
                .bearer_auth(self.credentials.access_token().await?);
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request
                .send()
                .await
                .with_context(|| format!("{method} {url}"))?;
            if response.status() == StatusCode::UNAUTHORIZED && !retry {
                self.credentials.invalidate().await;
                continue;
            }
            return Ok(response);
        }
        unreachable!("the second attempt returns")
    }
}

async fn decode<T: DeserializeOwned>(url: &str, response: reqwest::Response) -> Result<T> {
    let body = response.bytes().await?;
    serde_json::from_slice(&body)
        .with_context(|| format!("decoding {url}: {}", String::from_utf8_lossy(&body)))
}

/// What Graph made of a change to a message.
#[derive(Debug, PartialEq)]
pub enum Changed {
    Done,
    /// The message no longer exists.
    Gone,
    /// Refused, for this reason: a 4xx other than for authorization or
    /// rate, which pass.
    Refused(String),
}

async fn changed(response: reqwest::Response) -> Result<Changed> {
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Ok(Changed::Gone);
    }
    if status.is_client_error()
        && status != StatusCode::UNAUTHORIZED
        && status != StatusCode::TOO_MANY_REQUESTS
    {
        let body = response.text().await.unwrap_or_default();
        return Ok(Changed::Refused(error_message(&body).unwrap_or(body)));
    }
    check(response).await.map(|_| Changed::Done)
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

/// The message of a Graph error response.
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

    #[test]
    fn delta_items_are_messages_or_removals() {
        let page: DeltaPage<Delta> = serde_json::from_str(
            r#"{"value": [
                {"id": "a", "@removed": {"reason": "deleted"}},
                {"id": "b", "parentFolderId": "f", "conversationId": "c",
                 "receivedDateTime": "2026-11-03T10:00:00Z", "isRead": false,
                 "flag": {"flagStatus": "flagged"}, "importance": "high",
                 "categories": ["Red"]}
            ], "@odata.deltaLink": "https://graph/delta?$deltatoken=1"}"#,
        )
        .unwrap();
        assert!(matches!(&page.value[0], Delta::Removed { id, .. } if id == "a"));
        let Delta::Message(m) = &page.value[1] else {
            panic!("a message")
        };
        assert_eq!(m.flag.flag_status, FlagStatus::Flagged);
        assert_eq!(m.importance, Importance::High);
    }
}
