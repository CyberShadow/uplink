//! Minimal ops over a `sample` table, for exercising the dispatcher in
//! tests (SPEC §14 M3). The table is not part of the production schema;
//! tests create it with [`SCHEMA`].

use crate::{Applied, Conflict, Inverse, Observation, Op, Registry};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use uplink_storage::rusqlite::OptionalExtension;
use uplink_storage::rusqlite::Transaction;

pub const SCHEMA: &str = "CREATE TABLE sample (id INTEGER PRIMARY KEY, x INTEGER NOT NULL)";

pub fn register(registry: &mut Registry) {
    registry.register::<SampleInsert>();
    registry.register::<SampleDelete>();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SampleInsert {
    pub id: i64,
    pub x: i64,
}

/// A sample row appearing externally.
#[derive(Debug, Serialize, Deserialize)]
pub struct SampleObserved {
    pub id: i64,
    pub x: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SampleDelete {
    pub id: i64,
}

impl Op for SampleInsert {
    const KIND: &'static str = "sample.insert";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        tx.execute(
            "INSERT INTO sample (id, x) VALUES (?1, ?2)",
            [self.id, self.x],
        )?;
        Ok(Applied {
            inverse: Inverse::of(&SampleDelete { id: self.id }),
            events: vec![],
        })
    }

    fn describe(&self) -> String {
        format!("insert sample {} = {}", self.id, self.x)
    }
}

impl Op for SampleDelete {
    const KIND: &'static str = "sample.delete";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let x: i64 = tx
            .query_row("SELECT x FROM sample WHERE id = ?1", [self.id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| Conflict(format!("sample {} does not exist", self.id)))?;
        tx.execute("DELETE FROM sample WHERE id = ?1", [self.id])?;
        Ok(Applied {
            inverse: Inverse::of(&SampleInsert { id: self.id, x }),
            events: vec![],
        })
    }

    fn describe(&self) -> String {
        format!("delete sample {}", self.id)
    }
}

impl Observation for SampleObserved {
    const KIND: &'static str = "sample.observed";

    fn apply(self, tx: &Transaction) -> Result<Vec<uplink_protocol::ServerEvent>> {
        tx.execute(
            "INSERT INTO sample (id, x) VALUES (?1, ?2)",
            [self.id, self.x],
        )?;
        Ok(vec![])
    }

    fn describe(&self) -> String {
        format!("observed sample {} = {}", self.id, self.x)
    }
}
