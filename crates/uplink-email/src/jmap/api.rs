//! The parts of JMAP (RFC 8620, core; RFC 8621, mail) uplink uses: the
//! session, method calls, blobs, and the event source (SPEC §8.13).

use crate::mail_server::sign_in::{Secret, SignIn};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{header::HeaderValue, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";

/// How long a method call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a blob may take to go up or come down: a message may be tens
/// of megabytes.
const BLOB_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// An account's JMAP server.
pub struct Jmap {
    http: reqwest::Client,
    session_url: String,
    user: String,
    sign_in: SignIn,
    /// What signs in, kept until the server refuses it, as a connection
    /// would be: a new password applies from then on.
    authorization: tokio::sync::Mutex<Option<HeaderValue>>,
    /// Kept until a response tells of a newer one.
    session: Mutex<Option<Arc<Session>>>,
}

/// The JMAP session resource (RFC 8620 §2), as uplink uses it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    capabilities: HashMap<String, Value>,
    primary_accounts: HashMap<String, String>,
    api_url: String,
    download_url: String,
    upload_url: String,
    event_source_url: String,
    state: String,
    /// The account mail is of: the mail capability's primary account.
    #[serde(skip)]
    pub account_id: String,
    /// Limits of the core capability.
    #[serde(skip)]
    pub max_objects_in_get: usize,
    #[serde(skip)]
    pub max_objects_in_set: usize,
    #[serde(skip)]
    pub max_calls_in_request: usize,
}

/// A method's error response (RFC 8620 §3.6.2).
#[derive(Debug, Clone, Deserialize, thiserror::Error)]
#[error("{kind}{}", description.as_deref().map(|d| format!(": {d}")).unwrap_or_default())]
pub struct MethodError {
    #[serde(rename = "type")]
    pub kind: String,
    pub description: Option<String>,
}

/// Why an object was not created, updated or destroyed (RFC 8620 §5.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetError {
    #[serde(rename = "type")]
    pub kind: String,
    pub description: Option<String>,
    /// Of `alreadyExists`.
    pub existing_id: Option<String>,
}

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.description {
            Some(description) => write!(f, "{}: {description}", self.kind),
            None => write!(f, "{}", self.kind),
        }
    }
}

/// Method calls to make in one request.
#[derive(Default)]
pub struct Request {
    calls: Vec<Value>,
}

/// A call's place in its request.
#[derive(Debug, Clone, Copy)]
pub struct Call(usize);

impl Request {
    /// Adds a call of `method` with `arguments`, which name no account:
    /// the session's mail account is filled in.
    pub fn call(&mut self, method: &str, mut arguments: Value) -> Call {
        let i = self.calls.len();
        arguments
            .as_object_mut()
            .expect("arguments are an object")
            .insert("accountId".into(), Value::Null);
        self.calls.push(json!([method, arguments, i.to_string()]));
        Call(i)
    }

    /// An argument taking the result of `call` at `path` (RFC 8620 §3.7).
    pub fn result_of(&self, call: Call, path: &str) -> Value {
        let method = &self.calls[call.0][0];
        json!({ "resultOf": call.0.to_string(), "name": method, "path": path })
    }
}

/// The responses to a request's calls.
pub struct Responses(Vec<Value>);

impl Responses {
    /// The response to `call`, or its error.
    pub fn take<T: DeserializeOwned>(&mut self, call: Call) -> Result<T> {
        let Value::Array(response) = self.0[call.0].take() else {
            bail!("a response that is not an invocation");
        };
        let [name, arguments, _]: [Value; 3] = response
            .try_into()
            .map_err(|_| anyhow::anyhow!("a response that is not an invocation"))?;
        if name == "error" {
            return Err(serde_json::from_value::<MethodError>(arguments)?.into());
        }
        serde_json::from_value(arguments).with_context(|| format!("the response to {name}"))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    method_responses: Vec<Value>,
    session_state: String,
}

/// `Foo/get`'s response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Got<T> {
    pub state: String,
    pub list: Vec<T>,
    pub not_found: Vec<String>,
}

/// `Foo/changes`' response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    pub new_state: String,
    pub has_more_changes: bool,
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub destroyed: Vec<String>,
}

/// `Foo/query`'s response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Queried {
    pub ids: Vec<String>,
    pub total: Option<u32>,
}

/// `Foo/set`'s response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Set {
    #[serde(default)]
    pub created: HashMap<String, Value>,
    #[serde(default)]
    pub updated: HashMap<String, Value>,
    #[serde(default)]
    pub not_created: HashMap<String, SetError>,
    #[serde(default)]
    pub not_updated: HashMap<String, SetError>,
}

/// `Email/import`'s response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Imported {
    #[serde(default)]
    pub created: HashMap<String, Created>,
    #[serde(default)]
    pub not_created: HashMap<String, SetError>,
}

#[derive(Debug, Deserialize)]
pub struct Created {
    pub id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub role: Option<String>,
}

/// The properties of an email uplink reads.
pub const EMAIL_PROPERTIES: [&str; 6] = [
    "id",
    "threadId",
    "mailboxIds",
    "keywords",
    "receivedAt",
    "blobId",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Email {
    pub id: String,
    pub thread_id: String,
    pub mailbox_ids: HashMap<String, bool>,
    pub keywords: HashMap<String, bool>,
    /// UTC, RFC 3339.
    pub received_at: String,
    pub blob_id: String,
}

#[derive(Debug, Deserialize)]
pub struct Identity {
    pub id: String,
    pub email: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Uploaded {
    blob_id: String,
}

impl Jmap {
    /// The server whose session is at `session_url`, signed in to as
    /// `user`.
    pub fn new(http: reqwest::Client, session_url: &str, user: &str, sign_in: SignIn) -> Self {
        Self {
            http,
            session_url: session_url.to_owned(),
            user: user.to_owned(),
            sign_in,
            authorization: tokio::sync::Mutex::new(None),
            session: Mutex::new(None),
        }
    }

    async fn authorization(&self) -> Result<HeaderValue> {
        let mut authorization = self.authorization.lock().await;
        if let Some(value) = &*authorization {
            return Ok(value.clone());
        }
        let value = match self.sign_in.secret().await? {
            Secret::Password(password) => format!(
                "Basic {}",
                STANDARD.encode(format!("{}:{password}", self.user))
            ),
            Secret::AccessToken(token) => format!("Bearer {token}"),
        };
        let mut value = HeaderValue::from_str(&value).context("the secret is not a header's")?;
        value.set_sensitive(true);
        *authorization = Some(value.clone());
        Ok(value)
    }

    /// Sends `request` (built with its authorization), reading the answer
    /// whole; fails unless it succeeded. A refusal to sign in forgets
    /// what signed in, for the next request to sign in anew.
    async fn send(&self, what: &str, request: reqwest::RequestBuilder) -> Result<Vec<u8>> {
        let response = request
            .header(reqwest::header::AUTHORIZATION, self.authorization().await?)
            .send()
            .await
            .with_context(|| format!("requesting {what}"))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .with_context(|| format!("reading {what}"))?;
        if status == StatusCode::UNAUTHORIZED {
            *self.authorization.lock().await = None;
            self.sign_in.refused().await;
            bail!("the server refused to sign in {}", self.user);
        }
        if !status.is_success() {
            bail!(
                "{what} failed with {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        Ok(body.to_vec())
    }

    /// The session, fetched if not known.
    pub async fn session(&self) -> Result<Arc<Session>> {
        if let Some(session) = &*self.session.lock().expect("session poisoned") {
            return Ok(session.clone());
        }
        let body = self
            .send(
                "the JMAP session",
                self.http.get(&self.session_url).timeout(CALL_TIMEOUT),
            )
            .await?;
        let mut session: Session =
            serde_json::from_slice(&body).context("reading the JMAP session")?;
        for capability in [CORE, MAIL, SUBMISSION] {
            if !session.capabilities.contains_key(capability) {
                bail!("the server has no {capability}");
            }
        }
        session.account_id = session
            .primary_accounts
            .get(MAIL)
            .context("the server names no mail account")?
            .clone();
        let core = &session.capabilities[CORE];
        let limit = |name: &str| -> Result<usize> {
            Ok(core[name]
                .as_u64()
                .with_context(|| format!("the server gives no {name}"))?
                .try_into()?)
        };
        session.max_objects_in_get = limit("maxObjectsInGet")?;
        session.max_objects_in_set = limit("maxObjectsInSet")?;
        session.max_calls_in_request = limit("maxCallsInRequest")?;
        let session = Arc::new(session);
        *self.session.lock().expect("session poisoned") = Some(session.clone());
        Ok(session)
    }

    /// Makes `request`'s calls, in one request.
    pub async fn call(&self, request: Request) -> Result<Responses> {
        let session = self.session().await?;
        anyhow::ensure!(
            request.calls.len() <= session.max_calls_in_request,
            "the server takes {} calls in a request, not {}",
            session.max_calls_in_request,
            request.calls.len()
        );
        let mut calls = request.calls;
        for call in &mut calls {
            call[1]["accountId"] = session.account_id.clone().into();
        }
        let body = json!({ "using": [CORE, MAIL, SUBMISSION], "methodCalls": calls });
        let body = self
            .send(
                "a JMAP request",
                self.http
                    .post(&session.api_url)
                    .json(&body)
                    .timeout(CALL_TIMEOUT),
            )
            .await?;
        let envelope: Envelope =
            serde_json::from_slice(&body).context("reading a JMAP response")?;
        if envelope.session_state != session.state {
            *self.session.lock().expect("session poisoned") = None;
        }
        // Each call is answered, in order, by one response (none of uplink's
        // calls make the server add others, but for onSuccess updates,
        // which follow the call they are of).
        let mut responses: Vec<Value> = vec![Value::Null; calls.len()];
        for response in envelope.method_responses {
            let id = response[2]
                .as_str()
                .context("a response without its call's id")?;
            let i: usize = id.parse().with_context(|| format!("a response to {id}"))?;
            let slot = responses
                .get_mut(i)
                .with_context(|| format!("a response to {id}"))?;
            if slot.is_null() {
                *slot = response;
            }
        }
        if let Some(i) = responses.iter().position(Value::is_null) {
            bail!("the server did not answer call {i}");
        }
        Ok(Responses(responses))
    }

    /// Uploads `raw`; returns its blob id.
    pub async fn upload(&self, raw: &[u8], content_type: &str) -> Result<String> {
        let session = self.session().await?;
        let url = session
            .upload_url
            .replace("{accountId}", &encode(&session.account_id));
        let body = self
            .send(
                "an upload",
                self.http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, content_type)
                    .body(raw.to_vec())
                    .timeout(BLOB_TIMEOUT),
            )
            .await?;
        let uploaded: Uploaded = serde_json::from_slice(&body).context("reading an upload")?;
        Ok(uploaded.blob_id)
    }

    /// The blob of `blob_id`.
    pub async fn download(&self, blob_id: &str) -> Result<Vec<u8>> {
        let session = self.session().await?;
        let url = session
            .download_url
            .replace("{accountId}", &encode(&session.account_id))
            .replace("{blobId}", &encode(blob_id))
            .replace("{name}", "message.eml")
            .replace("{type}", &encode("message/rfc822"));
        self.send(
            &format!("blob {blob_id}"),
            self.http.get(url).timeout(BLOB_TIMEOUT),
        )
        .await
    }

    /// Opens the event source (RFC 8620 §7.3), telling of changes to
    /// `types`, pinging every `ping`: the response, whose body is the
    /// stream of events.
    pub async fn event_source(&self, types: &str, ping: Duration) -> Result<reqwest::Response> {
        let session = self.session().await?;
        let url = session
            .event_source_url
            .replace("{types}", types)
            .replace("{closeafter}", "no")
            .replace("{ping}", &ping.as_secs().to_string());
        let response = self
            .http
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.authorization().await?)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .context("opening the event source")?;
        if response.status() == StatusCode::UNAUTHORIZED {
            *self.authorization.lock().await = None;
            self.sign_in.refused().await;
            bail!("the server refused to sign in {}", self.user);
        }
        if !response.status().is_success() {
            bail!("the event source failed with {}", response.status());
        }
        Ok(response)
    }
}

/// `s`, percent-encoded for a URL's path or query.
fn encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_are_taken_by_call() {
        let mut request = Request::default();
        let query = request.call("Email/query", json!({}));
        let get = request.call(
            "Email/get",
            json!({ "#ids": request.result_of(query, "/ids") }),
        );
        assert_eq!(
            request.calls[1][1]["#ids"],
            json!({ "resultOf": "0", "name": "Email/query", "path": "/ids" })
        );
        let mut responses = Responses(vec![
            json!(["Email/query", { "ids": ["a"], "total": null }, "0"]),
            json!(["error", { "type": "cannotCalculateChanges" }, "1"]),
        ]);
        assert_eq!(responses.take::<Queried>(query).unwrap().ids, ["a"]);
        let error = responses.take::<Got<Email>>(get).unwrap_err();
        assert_eq!(
            error.downcast_ref::<MethodError>().unwrap().kind,
            "cannotCalculateChanges"
        );
    }
}
