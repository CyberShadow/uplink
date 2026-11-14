//! The parts of Cloud Pub/Sub uplink uses: the topic Gmail publishes an
//! account's changes to, and the pull subscription they are read from
//! (SPEC §8.1).

use crate::oauth::Credentials;
use anyhow::{bail, Context, Result};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

/// How long a pull may take: Pub/Sub answers one with no messages after a
/// wait of its own choosing, and one taking longer than this is taken for
/// lost without a word (a NAT forgetting the connection).
const PULL_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// How long a message is kept unpulled: a notification only says that
/// something changed, and the sync that follows a restart finds what did.
/// Pub/Sub's shortest.
const RETENTION: &str = "600s";
const PUBLISHER: &str = "roles/pubsub.publisher";

#[derive(Clone)]
pub struct PubSub {
    http: reqwest::Client,
    /// `.../v1`
    base: String,
    credentials: Credentials,
}

/// An answer to a request, read whole.
struct Reply {
    status: StatusCode,
    body: Vec<u8>,
}

impl Reply {
    fn check(self, what: &str) -> Result<Self> {
        if self.status.is_success() {
            return Ok(self);
        }
        bail!(
            "{what} failed with {}: {}",
            self.status,
            String::from_utf8_lossy(&self.body)
        )
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, what: &str) -> Result<T> {
        serde_json::from_slice(&self.body)
            .with_context(|| format!("decoding {what}: {}", String::from_utf8_lossy(&self.body)))
    }
}

impl PubSub {
    pub fn new(http: reqwest::Client, endpoint: &str, credentials: Credentials) -> Self {
        Self {
            http,
            base: format!("{}/v1", endpoint.trim_end_matches('/')),
            credentials,
        }
    }

    /// Makes the topic `name` (`projects/<project>/topics/<id>`), unless
    /// there is one.
    pub async fn create_topic(&self, name: &str) -> Result<()> {
        let reply = self.send(Method::PUT, name, Some(&json!({})), None).await?;
        if reply.status == StatusCode::CONFLICT {
            return Ok(());
        }
        reply.check(&format!("creating topic {name}"))?;
        Ok(())
    }

    /// Lets `member` publish to `topic`, keeping whom else its policy lets
    /// do what.
    pub async fn allow_publisher(&self, topic: &str, member: &str) -> Result<()> {
        let what = format!("the policy of {topic}");
        let mut policy: Value = self
            .send(
                Method::POST,
                &format!("{topic}:getIamPolicy"),
                Some(&json!({})),
                None,
            )
            .await?
            .check(&what)?
            .json(&what)?;
        let bindings = policy
            .as_object_mut()
            .with_context(|| format!("{what} is not an object"))?
            .entry("bindings")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("{what} has bindings not a list"))?;
        let binding = bindings
            .iter_mut()
            .find(|b| b["role"] == PUBLISHER && b.get("condition").is_none());
        match binding {
            Some(binding) => {
                let members = binding["members"]
                    .as_array_mut()
                    .with_context(|| format!("{what} has a binding without members"))?;
                if members.iter().any(|m| m == member) {
                    return Ok(());
                }
                members.push(member.into());
            }
            None => bindings.push(json!({ "role": PUBLISHER, "members": [member] })),
        }
        // The policy read carries its etag, so a change meanwhile is refused.
        self.send(
            Method::POST,
            &format!("{topic}:setIamPolicy"),
            Some(&json!({ "policy": policy })),
            None,
        )
        .await?
        .check(&format!("setting {what}"))?;
        Ok(())
    }

    /// Makes the pull subscription `name` to `topic`, never to expire,
    /// unless there is one; one there must be to `topic`.
    pub async fn create_subscription(&self, name: &str, topic: &str) -> Result<()> {
        #[derive(Deserialize)]
        struct Subscription {
            topic: String,
        }
        let body = json!({
            "topic": topic,
            "messageRetentionDuration": RETENTION,
            // With no time to live, it never expires (by default, after a
            // month unused).
            "expirationPolicy": {},
        });
        let reply = self.send(Method::PUT, name, Some(&body), None).await?;
        if reply.status != StatusCode::CONFLICT {
            reply.check(&format!("creating subscription {name}"))?;
            return Ok(());
        }
        let existing: Subscription = self
            .send(Method::GET, name, None, None)
            .await?
            .check(&format!("reading subscription {name}"))?
            .json(name)?;
        if existing.topic != topic {
            bail!(
                "subscription {name} is to {}, not {topic} (as when the topic was deleted): \
                 delete the subscription for uplink to make it anew",
                existing.topic
            );
        }
        Ok(())
    }

    /// The ack ids of the messages published to `subscription`'s topic and
    /// not yet acknowledged; waits a while for one if there is none, and
    /// gives none after.
    pub async fn pull(&self, subscription: &str) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Pulled {
            #[serde(default)]
            received_messages: Vec<Received>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Received {
            ack_id: String,
        }
        let what = format!("pulling {subscription}");
        let pulled: Pulled = match self
            .send(
                Method::POST,
                &format!("{subscription}:pull"),
                Some(&json!({ "maxMessages": 100 })),
                Some(PULL_TIMEOUT),
            )
            .await
        {
            Ok(reply) => reply.check(&what)?.json(&what)?,
            Err(e)
                if e.downcast_ref::<reqwest::Error>()
                    .is_some_and(|e| e.is_timeout()) =>
            {
                return Ok(vec![]);
            }
            Err(e) => return Err(e),
        };
        Ok(pulled
            .received_messages
            .into_iter()
            .map(|m| m.ack_id)
            .collect())
    }

    /// Acknowledges messages pulled, so they are not delivered again.
    pub async fn acknowledge(&self, subscription: &str, ack_ids: &[String]) -> Result<()> {
        self.send(
            Method::POST,
            &format!("{subscription}:acknowledge"),
            Some(&json!({ "ackIds": ack_ids })),
            None,
        )
        .await?
        .check(&format!("acknowledging messages of {subscription}"))?;
        Ok(())
    }

    /// Sends a request about the resource `path` (`projects/...`, with any
    /// `:method`), refreshing the access token once if it is refused.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Option<Duration>,
    ) -> Result<Reply> {
        let url = format!("{}/{path}", self.base);
        for refreshed in [false, true] {
            let mut request = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(self.credentials.access_token().await?);
            if let Some(body) = body {
                request = request.json(body);
            }
            if let Some(timeout) = timeout {
                request = request.timeout(timeout);
            }
            let response = request.send().await?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                self.credentials.invalidate().await;
                continue;
            }
            return Ok(Reply {
                status,
                body: response.bytes().await?.into(),
            });
        }
        unreachable!("the second attempt returns")
    }
}
