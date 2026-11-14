//! What an event opened shows (SPEC §10.2): all the calendar has of it,
//! read from its provider's JSON.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use uplink_email::remote::Remote;
use uplink_protocol::{
    calendar::{
        Attendee, AttendeeKind, CalendarEvent, EventDetails, EventField, EventFields,
        EventResponse, Person,
    },
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

/// The body of an event, as clients show it: sanitized HTML (and the
/// remote images it refers to), or text.
fn body(body: Option<Body>) -> (Option<String>, Option<String>, Remote) {
    match body {
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
        }) => (None, Some(content), Remote::default()),
        None => (None, None, Remote::default()),
    }
}

/// The details of `event`: of its provider's JSON `raw` (`None` while
/// being created), under the `edited` fields of `shown` still to be given;
/// and, of the user's own, what the edit form starts from (of it, and of
/// its series).
pub fn details(
    event: CalendarEvent,
    raw: Option<&str>,
    shown: Option<(EventFields, BTreeSet<EventField>)>,
    editable: Option<(EventFields, Option<EventFields>)>,
) -> Result<EventDetails> {
    let json: Option<serde_json::Value> = raw
        .map(serde_json::from_str)
        .transpose()
        .context("an event's JSON")?;
    let graph: Option<Event> = json
        .clone()
        .map(serde_json::from_value)
        .transpose()
        .context("an event's details")?;
    let (organizer, observed_attendees, observed_body, web_link) = match graph {
        Some(graph) => (graph.organizer, graph.attendees, graph.body, graph.web_link),
        None => (None, vec![], None, None),
    };
    let mut attendees = observed_attendees
        .into_iter()
        .map(|a| {
            Ok(Attendee {
                person: person(a.email_address)?,
                kind: a.kind,
                response: a.status.response,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut body_of = observed_body;
    if let Some((fields, edited)) = &shown {
        if edited.contains(&EventField::Attendees) {
            // As asked, with the answers given so far; rooms as they are.
            let mut asked: Vec<Attendee> = attendees
                .iter()
                .filter(|a| a.kind == AttendeeKind::Resource)
                .cloned()
                .collect();
            for (list, kind) in [
                (&fields.required, AttendeeKind::Required),
                (&fields.optional, AttendeeKind::Optional),
            ] {
                for (name, address) in uplink_email::compose::parse_addresses(list)? {
                    let given = attendees.iter().find(|a| {
                        a.person
                            .address
                            .as_deref()
                            .is_some_and(|a| a.eq_ignore_ascii_case(&address))
                    });
                    asked.push(Attendee {
                        person: Person {
                            name: name.unwrap_or_else(|| address.clone()),
                            address: Some(address),
                        },
                        kind,
                        response: given.map_or(EventResponse::None, |a| a.response),
                    });
                }
            }
            attendees = asked;
        }
        if edited.contains(&EventField::Body) {
            body_of = Some(Body {
                content_type: ContentType::Html,
                content: fields.body.clone(),
            });
        }
    }
    let (body_html, body_text, remote) = body(body_of);
    // The form edits HTML: a body as clients show it.
    let as_shown = |mut fields: EventFields| {
        fields.body = body(Some(Body {
            content_type: ContentType::Html,
            content: fields.body,
        }))
        .0
        .expect("HTML");
        fields
    };
    let (editable, series) = match editable {
        Some((own, series)) => (Some(as_shown(own)), series.map(as_shown)),
        None => (None, None),
    };
    Ok(EventDetails {
        event,
        organizer: organizer.map(|o| person(o.email_address)).transpose()?,
        attendees,
        body_html,
        body_text,
        remote_images: remote
            .hosts
            .into_iter()
            .map(|(host, images)| RemoteHost { host, images })
            .collect(),
        web_link,
        editable,
        series,
        raw: json.map(|json| serde_json::to_string_pretty(&json).expect("serializes")),
    })
}
