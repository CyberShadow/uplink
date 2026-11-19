//! Answers the to-do requests of the WebSocket protocol (SPEC §5.4).

use crate::{is_date, CreateTodo, DeleteTodo, EditTodo};
use uplink_protocol::{
    mail::MessageId,
    matrix::RoomEventId,
    todos::{TodoEdit, TodoId},
    ErrorCode, ServerError, ServerResponse,
};
use uplink_storage::rusqlite::{Connection, OptionalExtension};
use uplink_tx_log::{
    answer::{error, internal, Reader, Response},
    Dispatcher,
};

#[derive(Clone)]
pub struct Todos {
    dispatcher: Dispatcher,
    reader: Reader,
}

impl Todos {
    /// `reader` is a read-only connection to the database `dispatcher`
    /// writes.
    pub fn new(dispatcher: Dispatcher, reader: Connection) -> Self {
        Self {
            dispatcher,
            reader: Reader::new(reader),
        }
    }

    pub async fn list(&self) -> Response {
        self.reader
            .read(|conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let todos = crate::todos(&tx).map_err(internal)?;
                Ok(ServerResponse::Todos { as_of, todos })
            })
            .await
    }

    pub async fn create(
        &self,
        title: String,
        notes: String,
        due: Option<String>,
        message: Option<MessageId>,
        room_event: Option<RoomEventId>,
    ) -> Response {
        check_title(&title)?;
        if let Some(due) = &due {
            check_due(due)?;
        }
        if message.is_some() && room_event.is_some() {
            return Err(error(
                ErrorCode::Invalid,
                "a to-do is flagged from one message".into(),
            ));
        }
        let linked = message
            .map(|m| ("message", m.0))
            .or(room_event.map(|e| ("matrix_event", e.0)));
        if let Some((table, id)) = linked {
            self.reader
                .read(move |conn| {
                    conn.query_row(
                        &format!("SELECT 1 FROM {table} WHERE id = ?1"),
                        [id],
                        |_| Ok(()),
                    )
                    .optional()
                    .map_err(|e| internal(e.into()))?
                    .ok_or_else(|| error(ErrorCode::NotFound, format!("no message {id}")))
                })
                .await?;
        }
        self.dispatcher
            .answer(CreateTodo {
                title,
                notes,
                due,
                message,
                room_event,
            })
            .await
    }

    pub async fn edit(&self, todo: TodoId, edit: TodoEdit) -> Response {
        match &edit {
            TodoEdit::Title { title } => check_title(title)?,
            TodoEdit::Due { due: Some(due) } => check_due(due)?,
            TodoEdit::Notes { .. } | TodoEdit::Due { due: None } | TodoEdit::Done { .. } => {}
        }
        self.dispatcher
            .answer(EditTodo {
                id: todo,
                edit,
                expected: None,
            })
            .await
    }

    pub async fn delete(&self, todo: TodoId) -> Response {
        self.dispatcher.answer(DeleteTodo { id: todo }).await
    }
}

fn check_title(title: &str) -> Result<(), ServerError> {
    if title.trim().is_empty() {
        return Err(error(ErrorCode::Invalid, "the title is empty".into()));
    }
    Ok(())
}

fn check_due(due: &str) -> Result<(), ServerError> {
    if !is_date(due) {
        return Err(error(
            ErrorCode::Invalid,
            format!("due date {due} is not YYYY-MM-DD"),
        ));
    }
    Ok(())
}
