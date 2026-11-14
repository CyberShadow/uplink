//! A subscription's window is kept current with what changed of it,
//! after the entries that changed it (SPEC §5.7).

use axum::{extract::WebSocketUpgrade, routing::get, Router};
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use uplink_protocol::{
    compose::{DraftId, DraftsChanged},
    mail::ListScope,
    todos::{Todo, TodoId, TodosChanged},
    ClientRequest, ErrorCode, Item, LogEntry, ServerError, ServerEvent, ServerResponse, Source,
    SubscriptionId, TxLogId, UnknownNames, WindowChange, WsClientMsg, WsServerMsg,
};
use uplink_realtime::{run_session, Answer, Backend, Hub, Listing, Replay};

/// What every query matches, the most recently active first, as of the
/// last entry published.
#[derive(Default)]
struct State {
    head: i64,
    items: Vec<Item>,
}

struct Lists(Arc<Mutex<State>>);

impl Backend for Lists {
    async fn handle(&self, _request: ClientRequest) -> Answer {
        Answer::Now(Ok(ServerResponse::Done))
    }

    async fn replay(&self, _since: Option<TxLogId>) -> anyhow::Result<Replay> {
        Ok(Replay {
            head: TxLogId(self.0.lock().unwrap().head),
            entries: Some(vec![]),
        })
    }

    async fn window(
        &self,
        _query: String,
        _unknown: UnknownNames,
        limit: u32,
    ) -> Result<Listing, ServerError> {
        let state = self.0.lock().unwrap();
        Ok(Listing {
            as_of: TxLogId(state.head),
            items: state.items.iter().take(limit as usize).cloned().collect(),
            more: state.items.len() > limit as usize,
            scope: ListScope::Except { labels: vec![] },
            partial_rooms: vec![],
        })
    }
}

fn todo(id: i64, updated_at: i64) -> Item {
    Item::Todo {
        todo: Todo {
            id: TodoId(id),
            title: format!("to-do {id}"),
            notes: String::new(),
            due: None,
            done_at: None,
            message: None,
            created_at: 0,
            updated_at,
        },
    }
}

fn entry(id: i64, event: ServerEvent) -> LogEntry {
    LogEntry {
        id: TxLogId(id),
        ts: 0,
        source: Source::User,
        description: String::new(),
        op_type: String::new(),
        undoes: None,
        events: vec![event],
        shown: false,
    }
}

fn keys(change: &WindowChange) -> (Vec<&str>, Vec<&str>) {
    (
        change.upserted.iter().map(|l| l.key.0.as_str()).collect(),
        change.removed.iter().map(|k| k.0.as_str()).collect(),
    )
}

#[tokio::test]
async fn windows_are_sent_what_changed_of_them() {
    let state = Arc::new(Mutex::new(State {
        head: 0,
        items: vec![todo(1, 30), todo(2, 20), todo(3, 10)],
    }));
    let backend = Arc::new(Lists(state.clone()));
    let hub = Arc::new(Hub::new(16));
    let session_hub = hub.clone();
    let app = Router::new().route(
        "/ws",
        get(move |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(move |socket| async move {
                run_session(socket, &session_hub, &*backend).await
            })
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    let (mut tx, mut rx) = ws.split();
    let mut send = async |msg: WsClientMsg| {
        tx.send(Message::text(serde_json::to_string(&msg).unwrap()))
            .await
            .unwrap();
    };
    let mut recv = async || match rx.next().await.unwrap().unwrap() {
        Message::Text(text) => serde_json::from_str::<WsServerMsg>(&text).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    };

    send(WsClientMsg::Hello {
        version: uplink_protocol::typescript::version().to_owned(),
        since: None,
    })
    .await;
    assert!(matches!(recv().await, WsServerMsg::Welcome { .. }));
    assert!(matches!(recv().await, WsServerMsg::Badges { .. }));

    let one = SubscriptionId(1);
    send(WsClientMsg::Request {
        request_id: 1,
        body: ClientRequest::Subscribe {
            subscription: one,
            query: "q".into(),
            unknown: UnknownNames::Invalid,
            limit: 2,
        },
    })
    .await;
    let WsServerMsg::Response {
        request_id: 1,
        body: ServerResponse::Window { change, .. },
    } = recv().await
    else {
        panic!("expected the window")
    };
    assert_eq!(keys(&change), (vec!["todo:1", "todo:2"], vec![]));
    assert!(change.more);

    // 1 is done with, 2 changes: 3 comes into the window, which is all.
    {
        let mut state = state.lock().unwrap();
        state.head = 1;
        state.items = vec![todo(2, 40), todo(3, 10)];
    }
    hub.publish(entry(
        1,
        ServerEvent::TodosChanged(TodosChanged {
            todos: vec![TodoId(1), TodoId(2)],
        }),
    ));
    assert!(matches!(recv().await, WsServerMsg::Entry { entry } if entry.id == TxLogId(1)));
    let WsServerMsg::WindowChanged {
        subscription,
        change,
    } = recv().await
    else {
        panic!("expected what changed of the window")
    };
    assert_eq!(subscription, one);
    assert_eq!(keys(&change), (vec!["todo:2", "todo:3"], vec!["todo:1"]));
    assert_eq!(change.upserted[0].position.at, 40);
    assert!(!change.more);

    // Of an entry no list shows, nothing; nor of one that changes none.
    state.lock().unwrap().head = 3;
    hub.publish(entry(
        2,
        ServerEvent::DraftsChanged(DraftsChanged {
            drafts: vec![DraftId("d".into())],
        }),
    ));
    hub.publish(entry(
        3,
        ServerEvent::TodosChanged(TodosChanged {
            todos: vec![TodoId(9)],
        }),
    ));
    assert!(matches!(recv().await, WsServerMsg::Entry { entry } if entry.id == TxLogId(2)));
    assert!(matches!(recv().await, WsServerMsg::Entry { entry } if entry.id == TxLogId(3)));
    send(WsClientMsg::Ping { seq: 1 }).await;
    assert_eq!(recv().await, WsServerMsg::Pong { seq: 1 });

    // Widened, it lists what is past it; there is no more.
    state.lock().unwrap().items.push(todo(4, 5));
    send(WsClientMsg::Request {
        request_id: 2,
        body: ClientRequest::Extend {
            subscription: one,
            limit: 5,
        },
    })
    .await;
    let WsServerMsg::Response {
        request_id: 2,
        body: ServerResponse::Window { change, .. },
    } = recv().await
    else {
        panic!("expected the window widened")
    };
    assert_eq!(keys(&change), (vec!["todo:4"], vec![]));
    assert!(!change.more);

    // A name is the client's once.
    send(WsClientMsg::Request {
        request_id: 3,
        body: ClientRequest::Subscribe {
            subscription: one,
            query: "q".into(),
            unknown: UnknownNames::Invalid,
            limit: 2,
        },
    })
    .await;
    assert!(matches!(
        recv().await,
        WsServerMsg::RequestFailed { request_id: 3, error } if error.code == ErrorCode::Invalid
    ));

    send(WsClientMsg::Request {
        request_id: 4,
        body: ClientRequest::Unsubscribe { subscription: one },
    })
    .await;
    assert_eq!(
        recv().await,
        WsServerMsg::Response {
            request_id: 4,
            body: ServerResponse::Done
        }
    );
    // Then it is of nothing changing.
    {
        let mut state = state.lock().unwrap();
        state.head = 4;
        state.items.clear();
    }
    hub.publish(entry(
        4,
        ServerEvent::TodosChanged(TodosChanged {
            todos: vec![TodoId(2)],
        }),
    ));
    assert!(matches!(recv().await, WsServerMsg::Entry { entry } if entry.id == TxLogId(4)));
    send(WsClientMsg::Request {
        request_id: 5,
        body: ClientRequest::Extend {
            subscription: one,
            limit: 1,
        },
    })
    .await;
    assert!(matches!(
        recv().await,
        WsServerMsg::RequestFailed { request_id: 5, error } if error.code == ErrorCode::NotFound
    ));
}
