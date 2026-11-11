//! Whether an account's changes are reaching its provider (SPEC §6.6),
//! as its reconciler records it, of mail and Matrix accounts alike: from
//! the first failure on, until its pushes succeed again; not at each
//! retry.

use crate::{Observation, Observed};
use anyhow::{ensure, Result};
use serde::Serialize;
use uplink_protocol::{
    compose::{Failing, OutboxChanged},
    ServerEvent,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension};

/// `error` says why the account's changes are not reaching its provider;
/// `None` once they are again.
#[derive(Debug, Serialize)]
pub struct PushFailing {
    pub error: Option<String>,
    /// Unix ms.
    pub at: i64,
}

impl Observation for PushFailing {
    const KIND: &'static str = "account.push_failing";

    fn apply(self, account: &str, tx: &uplink_storage::rusqlite::Transaction) -> Result<Observed> {
        let description = match &self.error {
            Some(error) => format!("Changes are not reaching the provider: {error}"),
            None => "Changes are reaching the provider again".to_owned(),
        };
        let updated = tx.execute(
            "UPDATE account SET push_error = ?1,
               push_failing_since = CASE WHEN ?1 IS NULL THEN NULL
                                         ELSE coalesce(push_failing_since, ?2) END
             WHERE id = ?3",
            params![self.error, self.at, account],
        )?;
        ensure!(updated == 1, "account {account} is not recorded");
        Ok(Observed {
            description,
            events: vec![ServerEvent::OutboxChanged(OutboxChanged {
                accounts: vec![account.to_owned()],
            })],
        })
    }
}

/// Whether the account is recorded as failing; `false` if it is not
/// recorded yet (its first sync has not run).
pub fn is_failing(conn: &Connection, account: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT push_error IS NOT NULL FROM account WHERE id = ?1",
            [account],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// The accounts of these kinds that are failing, in id order.
pub fn failing(conn: &Connection, kinds: &[&str]) -> Result<Vec<(String, Failing)>> {
    let mut failing = vec![];
    let mut query = conn.prepare(
        "SELECT id, push_error, push_failing_since FROM account
         WHERE push_error IS NOT NULL AND kind = ?1",
    )?;
    for kind in kinds {
        let rows = query.query_map([kind], |r| {
            Ok((
                r.get(0)?,
                Failing {
                    error: r.get(1)?,
                    since: r.get(2)?,
                },
            ))
        })?;
        for row in rows {
            failing.push(row?);
        }
    }
    failing.sort_by(|a: &(String, Failing), b| a.0.cmp(&b.0));
    Ok(failing)
}
