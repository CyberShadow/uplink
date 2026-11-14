//! Answering meeting requests (SPEC §10.3): accepting, tentatively
//! accepting, declining. The answer is desired state (§6.6), a field of
//! each event: the op records the answer asked for, shown at once, and
//! the account's calendar sync gives it to the provider, which tells the
//! organizer, and reads it back. An answer given cannot be taken back at
//! the provider, only changed: undoing one already given needs a
//! decision.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    calendar::{CalendarChanged, EventId, EventResponse, PendingResponse},
    compose::PendingOp,
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{
    reconcile::{Ended, Field},
    this_entry, Applied, Conflict, Inverse, Observation, Observed, Op,
};

/// Why an answer asked for cannot be given: a meeting request, once
/// answered, has no unanswered state to go back to.
pub const CANNOT_TAKE_BACK: &str = "an answer given can be changed, not taken back";

/// The answer as a verb on the meeting, as a description has it.
fn verb(response: EventResponse) -> &'static str {
    match response {
        EventResponse::Accepted => "Accepted",
        EventResponse::TentativelyAccepted => "Tentatively accepted",
        EventResponse::Declined => "Declined",
        EventResponse::NotResponded | EventResponse::None => "Took back the answer to",
        EventResponse::Organizer => unreachable!("the organizer answers nothing"),
    }
}

/// Graph's action giving the answer, if it can be given.
pub fn action(response: EventResponse) -> Option<&'static str> {
    match response {
        EventResponse::Accepted => Some("accept"),
        EventResponse::TentativelyAccepted => Some("tentativelyAccept"),
        EventResponse::Declined => Some("decline"),
        EventResponse::NotResponded | EventResponse::None | EventResponse::Organizer => None,
    }
}

/// Whether the user can answer an event the provider has answered
/// `observed`: a meeting request, rather than an appointment or one the
/// user organizes.
pub fn answerable(observed: EventResponse) -> bool {
    !matches!(observed, EventResponse::Organizer | EventResponse::None)
}

/// An event's answer (SPEC §6.6): as the provider has it (`observed`) or
/// as asked for (`desired`).
pub type Answer = Field<EventResponse>;

fn parse_field(json: &str) -> Result<Answer> {
    serde_json::from_str(json).with_context(|| format!("an event's answer field: {json}"))
}

/// The JSON of the field of an event first seen answered `response` by a
/// request sent after `as_of`.
pub(crate) fn new_field(response: EventResponse, as_of: TxLogId) -> String {
    serde_json::to_string(&Field::new(response, as_of)).expect("a field serializes")
}

/// The event's answer; `None` if the event is gone.
fn load(conn: &Connection, event: EventId) -> Result<Option<Answer>> {
    conn.query_row(
        "SELECT response_field FROM calendar_event WHERE id = ?1",
        [event.0],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|json| parse_field(&json))
    .transpose()
}

fn store(tx: &Transaction, event: EventId, field: &Answer) -> Result<()> {
    tx.execute(
        "UPDATE calendar_event SET response_field = ?2 WHERE id = ?1",
        params![event.0, serde_json::to_string(field)?],
    )?;
    Ok(())
}

/// Records the event's answer as observed by a request sent after
/// `as_of` (SPEC §6.6.2).
pub(crate) fn observe(
    tx: &Transaction,
    event: EventId,
    response: EventResponse,
    as_of: TxLogId,
) -> Result<()> {
    let mut field = load(tx, event)?.with_context(|| format!("no event {}", event.0))?;
    field.observe(response, as_of, this_entry(tx)?)?;
    store(tx, event, &field)
}

/// The account's answers being given, as the outbox compares them.
pub(crate) fn pending_of(
    tx: &Transaction,
    account: &str,
) -> Result<Vec<(i64, String, Option<String>)>> {
    Ok(tx
        .prepare(
            "SELECT id, desired_response, response_decision FROM calendar_event
             WHERE account_id = ?1 AND response_by IS NOT NULL ORDER BY id",
        )?
        .query_map([account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?)
}

/// Answers a meeting request, as `response`; or, as an inverse, takes an
/// answer back to the one before. The answer is shown at once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Respond {
    pub event: EventId,
    pub response: EventResponse,
}

impl Op for Respond {
    const KIND: &'static str = "calendar.respond";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let (account, subject): (String, String) = tx
            .query_row(
                "SELECT account_id, subject FROM calendar_event WHERE id = ?1",
                [self.event.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| Conflict("the event is no longer on the calendar".into()))?;
        let mut field = load(tx, self.event)?.expect("the event is there");
        if !answerable(field.observed) || self.response == EventResponse::Organizer {
            bail!("event {} is not a meeting request to answer", self.event.0);
        }
        let shown = field.desired;
        let inverse = Inverse::of(&Respond {
            event: self.event,
            response: shown,
        });
        if shown == self.response {
            return Ok(Applied {
                description: "Changed nothing".to_owned(),
                inverse,
                events: vec![],
            });
        }
        let mut events = vec![ServerEvent::CalendarChanged(CalendarChanged {
            account: account.clone(),
        })];
        crate::telling_outbox(tx, &account, &mut events, || {
            field.want(self.response, this_entry(tx)?);
            store(tx, self.event, &field)
        })?;
        Ok(Applied {
            description: format!("{} '{subject}'", verb(self.response)),
            inverse,
            events,
        })
    }
}

/// An event's answer, as stored, of an account.
#[derive(Debug, Clone)]
pub struct Stored {
    pub event: EventId,
    pub external_id: String,
    pub field: Answer,
}

/// The account's answers uplink has an opinion of, or is to read back.
pub fn reconciling(conn: &Connection, account: &str) -> Result<Vec<Stored>> {
    conn.prepare(
        "SELECT id, external_id, response_field FROM calendar_event
         WHERE account_id = ?1
           AND (response_by IS NOT NULL
                OR json_extract(response_field, '$.check') IS NOT NULL)
         ORDER BY id",
    )?
    .query_map([account], |r| {
        Ok((EventId(r.get(0)?), r.get(1)?, r.get::<_, String>(2)?))
    })?
    .map(|row| {
        let (event, external_id, json) = row?;
        Ok(Stored {
            event,
            external_id,
            field: parse_field(&json)?,
        })
    })
    .collect()
}

/// The event's subject, for the log; empty if the event is gone, by a
/// sync, its answer with it.
fn subject(tx: &Transaction, event: EventId) -> Result<String> {
    Ok(tx
        .query_row(
            "SELECT subject FROM calendar_event WHERE id = ?1",
            [event.0],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default())
}

/// The answers whose pushes the calendar sync claims, in one entry (SPEC
/// §6.6.3): those still wanted of `events`.
#[derive(Debug, Serialize)]
pub struct AnswersClaimed {
    pub events: Vec<EventId>,
}

impl Observation for AnswersClaimed {
    const KIND: &'static str = "calendar.answers_claimed";

    fn apply(self, _account: &str, tx: &Transaction) -> Result<Observed> {
        let mut claimed = 0;
        for &event in &self.events {
            // Unless gone, or settled by an op, meanwhile.
            let Some(mut field) = load(tx, event)? else {
                continue;
            };
            if field.wanted().is_some() {
                field.claim()?;
                store(tx, event, &field)?;
                claimed += 1;
            }
        }
        Ok(Observed {
            description: format!("Giving {claimed} answers to the provider"),
            events: vec![],
        })
    }
}

/// How giving the user's answer to the event, `response`, ended.
#[derive(Debug, Serialize)]
pub struct ResponseGiven {
    pub event: EventId,
    pub response: EventResponse,
    pub ended: Ended,
}

impl Observation for ResponseGiven {
    const KIND: &'static str = "calendar.response_given";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let subject = subject(tx, self.event)?;
        let mut events = vec![];
        crate::telling_outbox(tx, account, &mut events, || {
            // Gone since, by a sync: its answer went with it.
            let Some(mut field) = load(tx, self.event)? else {
                return Ok(());
            };
            field.ended(self.ended.clone(), this_entry(tx)?)?;
            store(tx, self.event, &field)
        })?;
        let verb = verb(self.response);
        let description = match &self.ended {
            Ended::Applied => format!("{verb} '{subject}' at the provider"),
            Ended::NotApplied {
                refused: Some(reason),
            } => format!(
                "The provider refused to answer '{subject}' ({}): {reason}",
                verb.to_lowercase()
            ),
            Ended::NotApplied { refused: None } => format!(
                "Could not answer '{subject}' ({}) at the provider",
                verb.to_lowercase()
            ),
            Ended::Unknown { .. } => format!(
                "Whether the provider has the answer to '{subject}' ({}) is unknown",
                verb.to_lowercase()
            ),
        };
        Ok(Observed {
            description,
            events,
        })
    }
}

/// The event's answer, read back from the provider by a request sent
/// after `as_of` (SPEC §6.6.2); `None`: the event is gone, and its answer
/// with it.
#[derive(Debug, Serialize)]
pub struct ResponseRead {
    pub event: EventId,
    pub response: Option<EventResponse>,
    pub as_of: TxLogId,
}

impl Observation for ResponseRead {
    const KIND: &'static str = "calendar.response_read";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let subject = subject(tx, self.event)?;
        let mut events = vec![ServerEvent::CalendarChanged(CalendarChanged {
            account: account.to_owned(),
        })];
        crate::telling_outbox(tx, account, &mut events, || match self.response {
            Some(response) => observe(tx, self.event, response, self.as_of),
            None => {
                tx.execute("DELETE FROM calendar_event WHERE id = ?1", [self.event.0])?;
                Ok(())
            }
        })?;
        Ok(Observed {
            description: match self.response {
                Some(_) => format!("Read the answer to '{subject}' from the provider"),
                None => format!("'{subject}' is no longer on the calendar"),
            },
            events,
        })
    }
}

/// Answers whose outcome became unknown, as of `now_ms`: those in flight,
/// as the process stopped (`interrupted`), else those so long enough to
/// expire (SPEC §6.6.3).
#[derive(Debug, Serialize)]
pub struct AnswersSettled {
    pub now_ms: i64,
    pub interrupted: bool,
}

impl Observation for AnswersSettled {
    const KIND: &'static str = "calendar.answers_settled";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let at = this_entry(tx)?;
        let mut settled = 0;
        for mut stored in reconciling(tx, account)? {
            let field = &mut stored.field;
            let before = field.clone();
            if self.interrupted {
                field.interrupted(self.now_ms);
            } else if field.expiring(self.now_ms) {
                field.expire(self.now_ms, at)?;
            }
            if *field != before {
                store(tx, stored.event, field)?;
                settled += 1;
            }
        }
        Ok(Observed {
            description: if self.interrupted {
                format!("Whether the provider has {settled} answers is unknown")
            } else {
                format!("Took {settled} answers given or not for good")
            },
            events: vec![],
        })
    }
}

/// Each account's answers being given, newest first.
pub(crate) fn outbox(tx: &Transaction) -> Result<Vec<(String, PendingResponse)>> {
    type Row = (String, i64, String, String, PendingOp);
    let rows: Vec<Row> = tx
        .prepare(
            "SELECT e.account_id, e.id, e.subject, e.response_field,
                    t.id, t.description, t.ts
             FROM calendar_event e JOIN tx_log t ON t.id = e.response_by
             ORDER BY e.account_id, t.id DESC, e.id",
        )?
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                PendingOp {
                    id: TxLogId(r.get(4)?),
                    description: r.get(5)?,
                    ts: r.get(6)?,
                },
            ))
        })?
        .collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(account, event, subject, field, op)| {
            let field = parse_field(&field)?;
            Ok((
                account,
                PendingResponse {
                    event: EventId(event),
                    subject,
                    response: field.desired,
                    op,
                    refused: field.decision.map(|d| d.reason().to_owned()),
                },
            ))
        })
        .collect()
}
