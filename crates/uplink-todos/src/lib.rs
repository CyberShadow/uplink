//! To-dos (SPEC §11): a local list, changed only through ops, so every
//! change can be undone. Flagging a message for follow-up is creating a
//! to-do linked to it (§11.2): the message is flagged while the to-do is
//! open, so flagging and its undo are one op each.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    mail::MessageId,
    todos::{Todo, TodoEdit, TodoId, TodosChanged},
    ServerEvent,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use uplink_tx_log::{Applied, Conflict, Inverse, Op, Registry};

pub mod service;

#[cfg(test)]
mod tests;

pub fn register(registry: &mut Registry) {
    registry.register::<CreateTodo>();
    registry.register::<PutTodo>();
    registry.register::<DeleteTodo>();
    registry.register::<EditTodo>();
}

const COLUMNS: &str = "id, title, notes, due, done_at, message, created_at";

fn todo_row(r: &Row) -> uplink_storage::rusqlite::Result<Todo> {
    Ok(Todo {
        id: TodoId(r.get(0)?),
        title: r.get(1)?,
        notes: r.get(2)?,
        due: r.get(3)?,
        done_at: r.get(4)?,
        message: r.get::<_, Option<i64>>(5)?.map(MessageId),
        created_at: r.get(6)?,
    })
}

/// Open to-dos first, by due date (undated last), then done ones, most
/// recently done first.
pub fn todos(conn: &Connection) -> Result<Vec<Todo>> {
    Ok(conn
        .prepare(&format!(
            "SELECT {COLUMNS} FROM todo
             ORDER BY done_at IS NOT NULL, done_at DESC, due IS NULL, due, created_at, id"
        ))?
        .query_map([], todo_row)?
        .collect::<Result<_, _>>()?)
}

fn todo(tx: &Transaction, id: TodoId) -> Result<Todo> {
    tx.query_row(
        &format!("SELECT {COLUMNS} FROM todo WHERE id = ?1"),
        [id.0],
        todo_row,
    )
    .optional()?
    .ok_or_else(|| Conflict("the to-do no longer exists".into()).into())
}

fn changed(id: TodoId) -> Vec<ServerEvent> {
    vec![ServerEvent::TodosChanged(TodosChanged { todos: vec![id] })]
}

/// Whether `due` is a date, `YYYY-MM-DD`.
pub fn is_date(due: &str) -> bool {
    let parts: Vec<&str> = due.split('-').collect();
    let number = |part: &str, digits: usize, range: std::ops::RangeInclusive<u32>| {
        part.len() == digits
            && part.bytes().all(|b| b.is_ascii_digit())
            && range.contains(&part.parse().expect("digits parse"))
    };
    matches!(parts.as_slice(), [y, m, d]
        if number(y, 4, 1..=9999) && number(m, 2, 1..=12) && number(d, 2, 1..=31))
}

/// Adds a to-do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTodo {
    pub title: String,
    pub notes: String,
    pub due: Option<String>,
    pub message: Option<MessageId>,
}

impl Op for CreateTodo {
    const KIND: &'static str = "todo.create";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let now = now_ms();
        let id = TodoId(tx.query_row(
            "INSERT INTO todo (title, notes, due, message, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5) RETURNING id",
            params![
                self.title,
                self.notes,
                self.due,
                self.message.map(|m| m.0),
                now
            ],
            |r| r.get(0),
        )?);
        Ok(Applied {
            description: match self.message {
                Some(_) => format!("Flagged '{}'", self.title),
                None => format!("Added '{}'", self.title),
            },
            inverse: Inverse::of(&DeleteTodo { id }),
            events: changed(id),
        })
    }
}

/// Restores a deleted to-do, as it was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutTodo {
    pub todo: Todo,
}

impl Op for PutTodo {
    const KIND: &'static str = "todo.put";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let t = &self.todo;
        let inserted = tx.execute(
            &format!(
                "INSERT INTO todo ({COLUMNS}, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (id) DO NOTHING"
            ),
            params![
                t.id.0,
                t.title,
                t.notes,
                t.due,
                t.done_at,
                t.message.map(|m| m.0),
                t.created_at,
                now_ms()
            ],
        )?;
        if inserted == 0 {
            bail!(Conflict(format!("to-do {} exists already", t.id.0)));
        }
        Ok(Applied {
            description: format!("Restored '{}'", t.title),
            inverse: Inverse::of(&DeleteTodo { id: t.id }),
            events: changed(t.id),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteTodo {
    pub id: TodoId,
}

impl Op for DeleteTodo {
    const KIND: &'static str = "todo.delete";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let todo = todo(tx, self.id)?;
        tx.execute("DELETE FROM todo WHERE id = ?1", [self.id.0])?;
        Ok(Applied {
            description: format!("Deleted '{}'", todo.title),
            inverse: Inverse::of(&PutTodo { todo }),
            events: changed(self.id),
        })
    }
}

/// Changes one field. The user's edits apply to whatever the field is;
/// inverses only if it is still what the edit they undo made it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditTodo {
    pub id: TodoId,
    pub edit: TodoEdit,
    pub expected: Option<TodoEdit>,
}

/// The field `edit` changes, as it is in `todo`.
fn current(todo: &Todo, edit: &TodoEdit) -> TodoEdit {
    match edit {
        TodoEdit::Title { .. } => TodoEdit::Title {
            title: todo.title.clone(),
        },
        TodoEdit::Notes { .. } => TodoEdit::Notes {
            notes: todo.notes.clone(),
        },
        TodoEdit::Due { .. } => TodoEdit::Due {
            due: todo.due.clone(),
        },
        TodoEdit::Done { .. } => TodoEdit::Done {
            done: todo.done_at.is_some(),
        },
    }
}

impl Op for EditTodo {
    const KIND: &'static str = "todo.edit";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let todo = todo(tx, self.id)?;
        let previous = current(&todo, &self.edit);
        if let Some(expected) = &self.expected {
            if *expected != previous {
                bail!(Conflict("the to-do was changed since".into()));
            }
        }
        let now = now_ms();
        let set = |column: &str, value: &dyn uplink_storage::rusqlite::ToSql| {
            tx.execute(
                &format!("UPDATE todo SET {column} = ?1, updated_at = ?2 WHERE id = ?3"),
                params![value, now, self.id.0],
            )
        };
        let title = &todo.title;
        let description = match &self.edit {
            TodoEdit::Title { title: new } => {
                set("title", new)?;
                format!("Renamed '{title}'")
            }
            TodoEdit::Notes { notes } => {
                set("notes", notes)?;
                format!("Edited the notes of '{title}'")
            }
            TodoEdit::Due { due } => {
                set("due", due)?;
                match due {
                    Some(due) => format!("Set '{title}' due {due}"),
                    None => format!("Removed the due date of '{title}'"),
                }
            }
            TodoEdit::Done { done } => {
                set("done_at", &done.then_some(now))?;
                match done {
                    true => format!("Done: '{title}'"),
                    false => format!("Reopened '{title}'"),
                }
            }
        };
        Ok(Applied {
            description,
            inverse: Inverse::of(&EditTodo {
                id: self.id,
                expected: Some(self.edit.clone()),
                edit: previous,
            }),
            events: changed(self.id),
        })
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis()
        .try_into()
        .expect("timestamp fits in i64")
}
