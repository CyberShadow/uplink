//! Answering meeting requests (SPEC §10.3): accepting, tentatively
//! accepting, declining. The answer is desired state (§6.6), as mail's
//! labels are: the op records the answer asked for, shown at once, and
//! the account's calendar sync gives it to the provider, which tells the
//! organizer. An answer given cannot be taken back at the provider, only
//! changed: undoing one already given needs a decision.

use crate::{name, named};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    calendar::{CalendarChanged, EventId, EventResponse, PendingResponse},
    compose::{AccountOutbox, OutboxChanged, Pending, PendingOp},
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{this_entry, Applied, Conflict, Inverse, Observation, Observed, Op};

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

/// The account's answers being given, as the outbox compares them.
fn pending_of(tx: &Transaction, account: &str) -> Result<Vec<(i64, String, Option<String>)>> {
    Ok(tx
        .prepare(
            "SELECT id, desired_response, response_refused FROM calendar_event
             WHERE account_id = ?1 AND desired_response IS NOT NULL ORDER BY id",
        )?
        .query_map([account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?)
}

/// What a change to the account's events did to its answers being
/// given: run `change`, then tell the outbox if they changed.
pub(crate) fn telling_outbox<T>(
    tx: &Transaction,
    account: &str,
    events: &mut Vec<ServerEvent>,
    change: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let before = pending_of(tx, account)?;
    let result = change()?;
    if pending_of(tx, account)? != before {
        events.push(ServerEvent::OutboxChanged(OutboxChanged {
            accounts: vec![account.to_owned()],
        }));
    }
    Ok(result)
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
        let this = this_entry(tx)?.0;
        let (account, subject, observed, desired): (String, String, String, Option<String>) = tx
            .query_row(
                "SELECT account_id, subject, response, desired_response
                 FROM calendar_event WHERE id = ?1",
                [self.event.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| Conflict("the event is no longer on the calendar".into()))?;
        let observed: EventResponse = named(observed)?;
        if !answerable(observed) || self.response == EventResponse::Organizer {
            bail!("event {} is not a meeting request to answer", self.event.0);
        }
        let shown = desired.map(named).transpose()?.unwrap_or(observed);
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
        telling_outbox(tx, &account, &mut events, || {
            let wanted = (self.response != observed).then(|| name(self.response));
            tx.execute(
                "UPDATE calendar_event SET desired_response = ?2, response_by = ?3,
                   response_refused = NULL
                 WHERE id = ?1",
                params![self.event.0, wanted, wanted.is_some().then_some(this)],
            )?;
            Ok(())
        })?;
        Ok(Applied {
            description: format!("{} '{subject}'", verb(self.response)),
            inverse,
            events,
        })
    }
}

/// An answer for the account's calendar sync to give.
#[derive(Debug, Clone, PartialEq)]
pub struct Wanted {
    pub event: EventId,
    pub external_id: String,
    pub response: EventResponse,
}

/// The account's answers still to give, but those refused.
pub fn wanted(conn: &Connection, account: &str) -> Result<Vec<Wanted>> {
    let rows: Vec<(i64, String, String)> = conn
        .prepare(
            "SELECT id, external_id, desired_response FROM calendar_event
             WHERE account_id = ?1 AND desired_response IS NOT NULL
               AND response_refused IS NULL
             ORDER BY id",
        )?
        .query_map([account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(event, external_id, response)| {
            Ok(Wanted {
                event: EventId(event),
                external_id,
                response: named(response)?,
            })
        })
        .collect()
}

/// The provider has the user's answer to the event as `response`, as
/// asked; or refused it, saying why.
#[derive(Debug, Serialize)]
pub struct ResponseGiven {
    pub event: EventId,
    pub response: EventResponse,
    pub refused: Option<String>,
}

impl Observation for ResponseGiven {
    const KIND: &'static str = "calendar.response_given";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let subject: Option<String> = tx
            .query_row(
                "SELECT subject FROM calendar_event WHERE id = ?1",
                [self.event.0],
                |r| r.get(0),
            )
            .optional()?;
        // Gone since, by a sync: its answer went with it.
        let subject = subject.unwrap_or_default();
        let mut events = vec![];
        let response = name(self.response);
        telling_outbox(tx, account, &mut events, || {
            match &self.refused {
                // Unless asked otherwise meanwhile (that is the sync's
                // next step), refused until the user decides.
                Some(reason) => tx.execute(
                    "UPDATE calendar_event SET response_refused = ?3
                     WHERE id = ?1 AND desired_response = ?2",
                    params![self.event.0, response, reason],
                )?,
                // As given; no longer pending, unless asked otherwise.
                None => tx.execute(
                    "UPDATE calendar_event SET response = ?2,
                       response_by = CASE WHEN desired_response = ?2 THEN NULL ELSE response_by END,
                       desired_response = CASE WHEN desired_response = ?2 THEN NULL ELSE desired_response END
                     WHERE id = ?1",
                    params![self.event.0, response],
                )?,
            };
            Ok(())
        })?;
        let description = match &self.refused {
            None => format!("{} '{subject}' at the provider", verb(self.response)),
            Some(reason) => format!(
                "The provider refused to answer '{subject}' ({}): {reason}",
                verb(self.response).to_lowercase()
            ),
        };
        Ok(Observed {
            description,
            events,
        })
    }
}

/// Each account with answers being given, in id order.
pub fn outbox(tx: &Transaction) -> Result<Vec<AccountOutbox>> {
    type Row = (String, i64, String, String, Option<String>, PendingOp);
    let rows: Vec<Row> = tx
        .prepare(
            "SELECT e.account_id, e.id, e.subject, e.desired_response, e.response_refused,
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
                r.get(4)?,
                PendingOp {
                    id: TxLogId(r.get(5)?),
                    description: r.get(6)?,
                    ts: r.get(7)?,
                },
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut accounts: Vec<AccountOutbox> = vec![];
    for (account, event, subject, response, refused, op) in rows {
        if accounts.last().is_none_or(|a| a.account != account) {
            accounts.push(AccountOutbox {
                account,
                failing: None,
                pending: Pending::Calendar { responses: vec![] },
            });
        }
        let Pending::Calendar { responses } = &mut accounts.last_mut().expect("pushed").pending
        else {
            unreachable!("pushed as of the calendar")
        };
        responses.push(PendingResponse {
            event: EventId(event),
            subject,
            response: named(response)?,
            op,
            refused,
        });
    }
    Ok(accounts)
}
