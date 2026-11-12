//! When the nav's places were last seen (SPEC §12.15): desired state
//! uplink alone keeps, set by ops so that it can be undone as any.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::Place;
use uplink_storage::rusqlite::{params, OptionalExtension, Transaction};
use uplink_tx_log::{Applied, Conflict, Inverse, Op, Registry};

pub fn register(registry: &mut Registry) {
    registry.register::<MarkSeen>();
    registry.register::<RestoreSeen>();
}

/// Of each kind, the latest id stored when a place was seen: what came
/// since is unseen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watermark {
    pub mail: i64,
    pub room_event: i64,
    pub todo: i64,
    pub event: i64,
}

/// A row of `seen`: a place's key, and its watermark, if it was seen.
type Row = (String, Option<Watermark>);

/// Sees `place`, as of now.
#[derive(Debug, Serialize, Deserialize)]
pub struct MarkSeen {
    pub place: Place,
}

/// Sets places' watermarks back, as they were: `expected` is what they
/// are now, as the op undone left them. `seeing`: whether it sees them
/// again (a redo) rather than unseeing them (an undo).
#[derive(Debug, Serialize, Deserialize)]
pub struct RestoreSeen {
    name: String,
    seeing: bool,
    rows: Vec<Row>,
    expected: Vec<Row>,
}

impl Op for MarkSeen {
    const KIND: &'static str = "seen.mark";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let (name, keys) = resolve(tx, &self.place)?;
        let now = Some(latest(tx)?);
        let previous = keys
            .iter()
            .map(|key| Ok((key.clone(), watermark(tx, key)?)))
            .collect::<Result<Vec<_>>>()?;
        let rows: Vec<Row> = keys.into_iter().map(|key| (key, now)).collect();
        set(tx, &rows)?;
        Ok(Applied {
            description: format!("Seen: {name}"),
            inverse: Inverse::of(&RestoreSeen {
                name,
                seeing: false,
                rows: previous,
                expected: rows,
            }),
            events: vec![],
        })
    }

    fn shown(&self) -> bool {
        true
    }
}

impl Op for RestoreSeen {
    const KIND: &'static str = "seen.restore";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        for (key, expected) in &self.expected {
            if watermark(tx, key)? != *expected {
                bail!(Conflict(format!("{} was seen again since", self.name)));
            }
        }
        set(tx, &self.rows)?;
        Ok(Applied {
            description: match self.seeing {
                true => format!("Seen: {}", self.name),
                false => format!("Not seen: {}", self.name),
            },
            inverse: Inverse::of(&RestoreSeen {
                name: self.name,
                seeing: !self.seeing,
                rows: self.expected,
                expected: self.rows,
            }),
            events: vec![],
        })
    }
}

/// The place's name, for the log, and the keys of its rows: All inboxes
/// is seen as each Inbox.
fn resolve(tx: &Transaction, place: &Place) -> Result<(String, Vec<String>)> {
    Ok(match place {
        Place::Home => ("Home".into(), vec![key(place)]),
        Place::Todos => ("To-dos".into(), vec![key(place)]),
        Place::Calendar => ("Calendar".into(), vec![key(place)]),
        Place::Inboxes => {
            let labels = tx
                .prepare("SELECT id FROM label WHERE system AND external_id = 'INBOX' ORDER BY id")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<Vec<i64>, _>>()?;
            let keys = labels
                .into_iter()
                .map(|label| {
                    key(&Place::Label {
                        label: uplink_protocol::mail::LabelId(label),
                    })
                })
                .collect();
            ("All inboxes".into(), keys)
        }
        Place::Label { label } => {
            let name: String = tx
                .query_row("SELECT name FROM label WHERE id = ?1", [label.0], |r| {
                    r.get(0)
                })
                .optional()?
                .with_context(|| format!("no label {}", label.0))?;
            (name, vec![key(place)])
        }
        Place::Account { account } => {
            tx.query_row("SELECT 1 FROM account WHERE id = ?1", [account], |_| Ok(()))
                .optional()?
                .with_context(|| format!("no account {account}"))?;
            (format!("All Mail of {account}"), vec![key(place)])
        }
        Place::Room { room } => {
            let name: String = tx
                .query_row(
                    "SELECT name FROM matrix_room WHERE id = ?1",
                    [room.0],
                    |r| r.get(0),
                )
                .optional()?
                .with_context(|| format!("no room {}", room.0))?;
            (name, vec![key(place)])
        }
    })
}

/// The place's key in `seen`; not of All inboxes, which has none.
pub fn key(place: &Place) -> String {
    match place {
        Place::Home => "home".into(),
        Place::Todos => "todos".into(),
        Place::Calendar => "calendar".into(),
        Place::Inboxes => unreachable!("All inboxes is seen as its Inboxes"),
        Place::Label { label } => format!("label:{}", label.0),
        Place::Account { account } => format!("account:{account}"),
        Place::Room { room } => format!("room:{}", room.0),
    }
}

fn latest(tx: &Transaction) -> Result<Watermark> {
    Ok(tx.query_row(
        "SELECT (SELECT coalesce(max(id), 0) FROM mail_entry),
                (SELECT coalesce(max(id), 0) FROM matrix_event),
                (SELECT coalesce(max(id), 0) FROM todo),
                (SELECT coalesce(max(id), 0) FROM calendar_event)",
        [],
        |r| {
            Ok(Watermark {
                mail: r.get(0)?,
                room_event: r.get(1)?,
                todo: r.get(2)?,
                event: r.get(3)?,
            })
        },
    )?)
}

/// Every place's watermark, by its key.
pub fn all(tx: &Transaction) -> Result<Vec<(String, Watermark)>> {
    Ok(tx
        .prepare("SELECT place, mail, room_event, todo, event FROM seen")?
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                Watermark {
                    mail: r.get(1)?,
                    room_event: r.get(2)?,
                    todo: r.get(3)?,
                    event: r.get(4)?,
                },
            ))
        })?
        .collect::<Result<_, _>>()?)
}

fn watermark(tx: &Transaction, key: &str) -> Result<Option<Watermark>> {
    Ok(tx
        .query_row(
            "SELECT mail, room_event, todo, event FROM seen WHERE place = ?1",
            [key],
            |r| {
                Ok(Watermark {
                    mail: r.get(0)?,
                    room_event: r.get(1)?,
                    todo: r.get(2)?,
                    event: r.get(3)?,
                })
            },
        )
        .optional()?)
}

fn set(tx: &Transaction, rows: &[Row]) -> Result<()> {
    for (key, watermark) in rows {
        match watermark {
            Some(w) => tx.execute(
                "INSERT INTO seen (place, mail, room_event, todo, event) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (place) DO UPDATE SET
                   mail = excluded.mail, room_event = excluded.room_event,
                   todo = excluded.todo, event = excluded.event",
                params![key, w.mail, w.room_event, w.todo, w.event],
            )?,
            None => tx.execute("DELETE FROM seen WHERE place = ?1", [key])?,
        };
    }
    Ok(())
}
