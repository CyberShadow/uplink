use crate::{is_date, register, service::Todos};
use std::sync::Arc;
use uplink_protocol::{
    mail::MessageId,
    matrix::{RoomEventId, RoomId},
    todos::{Todo, TodoEdit, TodoId, TodoRoomEvent},
    ErrorCode, ServerResponse, TxLogId,
};
use uplink_realtime::Hub;
use uplink_tx_log::{Dispatcher, Registry, UndoError};

struct Db {
    _dir: tempfile::TempDir,
    dispatcher: Dispatcher,
    todos: Todos,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(64)),
        );
        let todos = Todos::new(
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
        );
        Self {
            _dir: dir,
            dispatcher,
            todos,
        }
    }

    async fn list(&self) -> Vec<Todo> {
        match self.todos.list().await.unwrap() {
            ServerResponse::Todos { todos, .. } => todos,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn titles(&self) -> Vec<String> {
        self.list().await.into_iter().map(|t| t.title).collect()
    }

    async fn create(&self, title: &str, due: Option<&str>) -> TxLogId {
        dispatched(
            self.todos
                .create(title.into(), String::new(), due.map(Into::into), None, None)
                .await
                .unwrap(),
        )
    }

    async fn edit(&self, todo: TodoId, edit: TodoEdit) -> TxLogId {
        dispatched(self.todos.edit(todo, edit).await.unwrap())
    }

    async fn id(&self, title: &str) -> TodoId {
        self.list()
            .await
            .into_iter()
            .find(|t| t.title == title)
            .unwrap()
            .id
    }
}

fn dispatched(response: ServerResponse) -> TxLogId {
    match response {
        ServerResponse::Dispatched { tx_log_id } => tx_log_id,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn due_dates_are_dates() {
    assert!(is_date("2026-11-08"));
    for not in [
        "2026-11-8",
        "2026-13-01",
        "2026-11-08T10:00",
        "tomorrow",
        "",
        "2026-00-10",
    ] {
        assert!(!is_date(not), "{not}");
    }
}

#[tokio::test]
async fn open_todos_come_first_by_due_date() {
    let db = Db::new();
    db.create("Undated", None).await;
    db.create("Later", Some("2026-12-01")).await;
    db.create("Sooner", Some("2026-11-10")).await;
    db.create("Finished", Some("2026-11-01")).await;
    db.edit(db.id("Finished").await, TodoEdit::Done { done: true })
        .await;
    assert_eq!(
        db.titles().await,
        ["Sooner", "Later", "Undated", "Finished"]
    );
}

#[tokio::test]
async fn creating_and_deleting_are_undone() {
    let db = Db::new();
    let created = db.create("Call Bob", Some("2026-11-10")).await;
    let todo = db.list().await.remove(0);
    let deleted = dispatched(db.todos.delete(todo.id).await.unwrap());
    assert!(db.list().await.is_empty());
    // Restored as it was, under its id.
    db.dispatcher.undo(deleted).await.unwrap();
    assert_eq!(db.list().await, [todo]);
    db.dispatcher.undo(created).await.unwrap();
    assert!(db.list().await.is_empty());
}

#[tokio::test]
async fn edits_are_undone_unless_changed_since() {
    let db = Db::new();
    db.create("Call Bob", None).await;
    let id = db.id("Call Bob").await;
    let done = db.edit(id, TodoEdit::Done { done: true }).await;
    assert!(db.list().await[0].done_at.is_some());
    db.dispatcher.undo(done).await.unwrap();
    assert_eq!(db.list().await[0].done_at, None);

    let renamed = db
        .edit(
            id,
            TodoEdit::Title {
                title: "Call Carol".into(),
            },
        )
        .await;
    db.edit(
        id,
        TodoEdit::Title {
            title: "Call Dave".into(),
        },
    )
    .await;
    let refused = db.dispatcher.undo(renamed).await.unwrap_err();
    assert!(matches!(refused, UndoError::Conflict { .. }), "{refused}");
    assert_eq!(db.titles().await, ["Call Dave"]);
}

#[tokio::test]
async fn requests_are_checked() {
    let db = Db::new();
    let refused = |r: ServerResponse| panic!("accepted: {r:?}");
    let code =
        |r: Result<ServerResponse, uplink_protocol::ServerError>| r.map(refused).unwrap_err().code;
    assert_eq!(
        code(
            db.todos
                .create(" ".into(), String::new(), None, None, None)
                .await
        ),
        ErrorCode::Invalid
    );
    assert_eq!(
        code(
            db.todos
                .create("x".into(), String::new(), Some("soon".into()), None, None)
                .await
        ),
        ErrorCode::Invalid
    );
    assert_eq!(
        code(
            db.todos
                .create("x".into(), String::new(), None, Some(MessageId(7)), None)
                .await
        ),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(
            db.todos
                .create("x".into(), String::new(), None, None, Some(RoomEventId(7)))
                .await
        ),
        ErrorCode::NotFound
    );
    assert_eq!(code(db.todos.delete(TodoId(7)).await), ErrorCode::Conflict);
}

#[tokio::test]
async fn a_room_message_is_flagged_by_a_todo_which_undo_removes_and_redo_restores() {
    let db = Db::new();
    // An event, of a room there is no need of here.
    let conn = uplink_storage::rusqlite::Connection::open(uplink_storage::db_path(db._dir.path()))
        .unwrap();
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         INSERT INTO matrix_event (id, room, event_id, sender, ts, type, raw_json, encryption, pos)
         VALUES (5, 3, '$e', '@bob:example.org', 0, 'm.room.message', '{}', 'none', 1);",
    )
    .unwrap();
    let flagged = Some(TodoRoomEvent {
        room: RoomId(3),
        event: RoomEventId(5),
    });
    let created = dispatched(
        db.todos
            .create(
                "Answer Bob".into(),
                String::new(),
                None,
                None,
                Some(RoomEventId(5)),
            )
            .await
            .unwrap(),
    );
    assert_eq!(db.list().await[0].room_event, flagged);
    // Deleted and restored, it is of the message still.
    let id = db.id("Answer Bob").await;
    let deleted = dispatched(db.todos.delete(id).await.unwrap());
    db.dispatcher.undo(deleted).await.unwrap();
    assert_eq!(db.list().await[0].room_event, flagged);
    db.dispatcher.undo(created).await.unwrap();
    assert!(db.list().await.is_empty());
    // Of one message, not two.
    assert!(db
        .todos
        .create(
            "x".into(),
            String::new(),
            None,
            Some(MessageId(1)),
            Some(RoomEventId(5))
        )
        .await
        .is_err());
}

#[test]
fn ops_logged_before_room_messages_were_flagged_still_read() {
    let create: crate::CreateTodo =
        serde_json::from_str(r#"{"title":"t","notes":"","due":null,"message":null}"#).unwrap();
    assert_eq!(create.room_event, None);
    let put: crate::PutTodo = serde_json::from_str(
        r#"{"todo":{"id":1,"title":"t","notes":"","due":null,"done_at":null,"message":null,"created_at":0,"updated_at":0}}"#,
    )
    .unwrap();
    assert_eq!(put.todo.room_event, None);
}

#[tokio::test]
async fn queries_match_todos_by_text_due_dates_and_done() {
    let db = Db::new();
    db.create("Pay rent", Some("2026-11-10")).await;
    db.create("Call the dentist", Some("2026-11-11")).await;
    db.create("Book flights", Some("2026-11-12")).await;
    db.create("Read a book", None).await;
    let done = db
        .list()
        .await
        .into_iter()
        .find(|t| t.title == "Read a book")
        .unwrap();
    db.edit(done.id, TodoEdit::Done { done: true }).await;
    let today = uplink_query::days("2026-11-11").unwrap();
    let matching = |query: &str| -> Vec<String> {
        let conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        let query = uplink_query::parse(query, 0).unwrap();
        crate::matching(&conn, &query, today)
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect()
    };
    assert_eq!(matching("is:overdue"), ["Pay rent"]);
    assert_eq!(matching("due:today"), ["Call the dentist"]);
    assert_eq!(
        matching("due:tomorrow OR due:2026-11-10"),
        ["Pay rent", "Book flights"]
    );
    assert_eq!(matching("book"), ["Book flights"], "done ones are left out");
    assert_eq!(matching("book is:done"), ["Read a book"]);
    assert_eq!(matching("is:todo -has:due"), Vec::<String>::new());
    assert_eq!(
        matching("is:todo"),
        ["Pay rent", "Call the dentist", "Book flights"]
    );
    // Terms of other kinds are false of to-dos.
    assert!(matching("is:unread OR from:me OR is:room").is_empty());
    assert_eq!(matching("-is:unread").len(), 3);
}
