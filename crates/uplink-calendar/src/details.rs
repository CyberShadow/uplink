//! What an event opened shows (SPEC §10.2): all the calendar has of it,
//! read from its provider's JSON.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use uplink_protocol::{
    calendar::{Attendee, AttendeeKind, CalendarEvent, EventDetails, EventResponse, Person},
    mail::RemoteHost,
};

/// A Graph event, as far as its details are read.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    organizer: Option<Recipient>,
    #[serde(default)]
    attendees: Vec<GraphAttendee>,
    body: Option<Body>,
    web_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recipient {
    email_address: EmailAddress,
}

#[derive(Deserialize)]
struct EmailAddress {
    name: Option<String>,
    address: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphAttendee {
    email_address: EmailAddress,
    #[serde(rename = "type")]
    kind: AttendeeKind,
    status: Status,
}

#[derive(Deserialize)]
struct Status {
    response: EventResponse,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    content_type: ContentType,
    content: String,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ContentType {
    Text,
    Html,
}

fn person(address: EmailAddress) -> Result<Person> {
    let name = address
        .name
        .filter(|name| !name.is_empty())
        .or_else(|| address.address.clone())
        .context("a person without a name or an address")?;
    Ok(Person {
        name,
        address: address.address,
    })
}

/// The details of `event`, of its provider's JSON `raw`.
pub fn details(event: CalendarEvent, raw: &str) -> Result<EventDetails> {
    let json: serde_json::Value = serde_json::from_str(raw).context("an event's JSON")?;
    let graph: Event = serde_json::from_value(json.clone()).context("an event's details")?;
    let (body_html, body_text, remote) = match graph.body {
        Some(Body {
            content_type: ContentType::Html,
            content,
        }) => {
            let (html, remote) = uplink_email::parse::sanitized_html(&content, HashMap::new());
            (Some(html), None, remote)
        }
        Some(Body {
            content_type: ContentType::Text,
            content,
        }) => (None, Some(content), Default::default()),
        None => (None, None, Default::default()),
    };
    Ok(EventDetails {
        event,
        organizer: graph
            .organizer
            .map(|o| person(o.email_address))
            .transpose()?,
        attendees: graph
            .attendees
            .into_iter()
            .map(|a| {
                Ok(Attendee {
                    person: person(a.email_address)?,
                    kind: a.kind,
                    response: a.status.response,
                })
            })
            .collect::<Result<_>>()?,
        body_html,
        body_text,
        remote_images: remote
            .hosts
            .into_iter()
            .map(|(host, images)| RemoteHost { host, images })
            .collect(),
        web_link: graph.web_link,
        raw: serde_json::to_string_pretty(&json).expect("serializes"),
    })
}
