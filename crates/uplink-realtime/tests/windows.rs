//! A subscription's window is kept current with what changed of it,
//! after the entries that changed it (SPEC §5.7).

use axum::{extract::WebSocketUpgrade, routing::get, Router};
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use uplink_protocol::{
    compose::{DraftId, DraftsChanged},
    mail::ListScope,
    matrix::{Invitation, InvitationId, RoomId},
    todos::{Todo, TodoId, TodosChanged},
    Changed, ClientRequest, ErrorCode, Item, LogEntry, Of, Place, ServerError, ServerEvent,
    ServerResponse, Source, SubscriptionId, TxLogId, UnknownNames, Unseen, WindowChange,
    WsClientMsg, WsServerMsg,
};
use uplink_realtime::{
    keep_windows, run_session, Answer, Backend, Held, Hub, Key, Listing, Replay,
};

/// What every query matches, the most recently active first, as of the
/// last entry published.
#[derive(Default)]
struct State {
    head: i64,
    items: Vec<Item>,
    /// What is unseen while nothing is attended.
    unseen: Vec<Unseen>,
    /// How many times windows of items were listed.
    listings: u32,
}

struct Lists(Arc<Mutex<State>>, Arc<Hub>);

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
        _held: Held,
        _changed: Changed,
    ) -> Result<Listing<Item>, ServerError> {
        let mut state = self.0.lock().unwrap();
        state.listings += 1;
        Ok(Listing {
            as_of: TxLogId(state.head),
            listed: state.items.iter().take(limit as usize).cloned().collect(),
            next: state.items.get(limit as usize).cloned(),
            scope: ListScope::Except { labels: vec![] },
            partial_rooms: vec![],
        })
    }

    /// Of none while anything is attended: it is all in Home.
    async fn unseen(
        &self,
        _query: String,
        _unknown: UnknownNames,
        limit: u32,
    ) -> Result<Listing<Unseen>, ServerError> {
        let state = self.0.lock().unwrap();
        let attended = !self.1.attended().borrow().is_empty();
        let unseen: Vec<Unseen> = match attended {
            true => vec![],
            false => state.unseen.clone(),
        };
        Ok(Listing {
            as_of: TxLogId(state.head),
            next: unseen.get(limit as usize).cloned(),
            listed: unseen.into_iter().take(limit as usize).collect(),
            scope: (),
            partial_rooms: vec![],
        })
    }

    async fn attend(
        &self,
        session: u64,
        _from: Option<Place>,
        to: Option<Place>,
    ) -> Result<(), ServerError> {
        self.1.attend(session, to);
        Ok(())
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
            room_event: None,
            created_at: 0,
            updated_at,
        },
    }
}

/// Serves sessions of `state`, at the address returned, their windows
/// kept.
async fn serve(state: Arc<Mutex<State>>, hub: Arc<Hub>) -> std::net::SocketAddr {
    tokio::spawn(keep_windows(hub.clone(), Lists(state.clone(), hub.clone())));
    let backend = Arc::new(Lists(state, hub.clone()));
    let app = Router::new().route(
        "/ws",
        get(move |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(move |socket| async move { run_session(socket, &hub, &*backend).await })
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

type Connection = (
    futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
);

/// A session said hello to, welcomed: how to send to it, and receive.
async fn connect(
    addr: std::net::SocketAddr,
) -> (
    impl AsyncFnMut(WsClientMsg),
    impl AsyncFnMut() -> WsServerMsg,
) {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    let (mut tx, mut rx): Connection = ws.split();
    let mut send = async move |msg: WsClientMsg| {
        tx.send(Message::text(serde_json::to_string(&msg).unwrap()))
            .await
            .unwrap();
    };
    let mut recv = async move || match rx.next().await.unwrap().unwrap() {
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
    (send, recv)
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

fn keys<T>(change: &WindowChange<T>) -> (Vec<&str>, Vec<&str>) {
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
        unseen: vec![],
        listings: 0,
    }));
    let hub = Arc::new(Hub::new(16));
    let addr = serve(state.clone(), hub.clone()).await;
    let (mut send, mut recv) = connect(addr).await;

    let one = SubscriptionId(1);
    send(WsClientMsg::Request {
        request_id: 1,
        body: ClientRequest::Subscribe {
            subscription: one,
            query: "q".into(),
            unknown: UnknownNames::Invalid,
            limit: 2,
            of: Of::Items,
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
            of: Of::Items,
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

/// What another session attends is seen: a window of the unseen is
/// listed again as attention begins, and as it ends with its session.
#[tokio::test]
async fn windows_of_the_unseen_follow_what_is_attended() {
    let invite = Unseen::Invite {
        room: RoomId(1),
        name: "Party".into(),
        invitation: Invitation {
            id: InvitationId(1),
            inviter: "@bob:x".into(),
            inviter_name: None,
            stored_at: 10,
        },
    };
    let state = Arc::new(Mutex::new(State {
        head: 0,
        items: vec![],
        unseen: vec![invite],
        listings: 0,
    }));
    let hub = Arc::new(Hub::new(16));
    let addr = serve(state, hub).await;
    let (mut send, mut recv) = connect(addr).await;
    send(WsClientMsg::Request {
        request_id: 1,
        body: ClientRequest::Subscribe {
            subscription: SubscriptionId(1),
            query: "q".into(),
            unknown: UnknownNames::False,
            limit: 10,
            of: Of::Unseen,
        },
    })
    .await;
    let WsServerMsg::Response {
        request_id: 1,
        body: ServerResponse::UnseenWindow { change, .. },
    } = recv().await
    else {
        panic!("expected the window")
    };
    assert_eq!(keys(&change).0, ["invite:1"]);

    let (mut other, mut answers) = connect(addr).await;
    other(WsClientMsg::Request {
        request_id: 1,
        body: ClientRequest::Attend {
            place: Some(Place::Home),
        },
    })
    .await;
    assert_eq!(
        answers().await,
        WsServerMsg::Response {
            request_id: 1,
            body: ServerResponse::Done
        }
    );
    let WsServerMsg::UnseenChanged { change, .. } = recv().await else {
        panic!("expected what is seen gone")
    };
    assert_eq!(
        change.removed,
        [uplink_protocol::ItemKey("invite:1".into())]
    );

    // Its session ends, and so its attention.
    drop((other, answers));
    let WsServerMsg::UnseenChanged { change, .. } = recv().await else {
        panic!("expected what is unseen again")
    };
    assert_eq!(keys(&change).0, ["invite:1"]);
}

/// Subscribe to `query`'s items, of `limit`: the window.
fn subscribe(request_id: u64, subscription: u64, query: &str, limit: u32) -> WsClientMsg {
    WsClientMsg::Request {
        request_id,
        body: ClientRequest::Subscribe {
            subscription: SubscriptionId(subscription),
            query: query.into(),
            unknown: UnknownNames::False,
            limit,
            of: Of::Items,
        },
    }
}

/// Sessions subscribed to the same query share its listing: it is listed
/// once for all, as they subscribe and as it changes; and, pinned, it is
/// listed before any subscribes, and kept while none is.
#[tokio::test]
async fn windows_of_a_query_share_its_listing() {
    let state = Arc::new(Mutex::new(State {
        head: 0,
        items: vec![todo(1, 30), todo(2, 20), todo(3, 10)],
        unseen: vec![],
        listings: 0,
    }));
    let hub = Arc::new(Hub::new(16));
    hub.windows().pin(vec![(
        Key {
            of: Of::Items,
            query: "pinned".into(),
            unknown: UnknownNames::False,
        },
        2,
    )]);
    let addr = serve(state.clone(), hub.clone()).await;
    let (mut send, mut recv) = connect(addr).await;
    let (mut other, mut answers) = connect(addr).await;
    let window = |msg: WsServerMsg| match msg {
        WsServerMsg::Response {
            body: ServerResponse::Window { change, .. },
            ..
        } => change,
        other => panic!("expected a window, got {other:?}"),
    };

    // The pinned is listed before it is subscribed to, of as many as
    // pinned.
    send(subscribe(1, 1, "pinned", 1)).await;
    assert_eq!(keys(&window(recv().await)), (vec!["todo:1"], vec![]));
    other(subscribe(1, 1, "pinned", 2)).await;
    assert_eq!(
        keys(&window(answers().await)),
        (vec!["todo:1", "todo:2"], vec![])
    );
    assert_eq!(state.lock().unwrap().listings, 1);

    // Another query is listed as it is subscribed to, once for both.
    send(subscribe(2, 2, "q", 2)).await;
    window(recv().await);
    other(subscribe(2, 2, "q", 1)).await;
    assert_eq!(keys(&window(answers().await)), (vec!["todo:1"], vec![]));
    assert_eq!(state.lock().unwrap().listings, 2);

    // A change is listed once of each query, and sent to each window as
    // it changed of it.
    {
        let mut state = state.lock().unwrap();
        state.head = 1;
        state.items = vec![todo(2, 20), todo(3, 10)];
    }
    hub.publish(entry(
        1,
        ServerEvent::TodosChanged(TodosChanged {
            todos: vec![TodoId(1)],
        }),
    ));
    let mut changes = vec![];
    for _ in 0..3 {
        match recv().await {
            WsServerMsg::Entry { entry } => assert_eq!(entry.id, TxLogId(1)),
            WsServerMsg::WindowChanged {
                subscription,
                change,
            } => changes.push((
                subscription.0,
                keys(&change).0.join(","),
                keys(&change).1.join(","),
            )),
            other => panic!("unexpected {other:?}"),
        }
    }
    changes.sort();
    assert_eq!(
        changes,
        [
            (1, "todo:2".to_owned(), "todo:1".to_owned()),
            (2, "todo:3".to_owned(), "todo:1".to_owned()),
        ]
    );
    assert_eq!(state.lock().unwrap().listings, 4);

    // Unsubscribed by both, a query is listed no more; the pinned is.
    drop((other, answers));
    send(WsClientMsg::Request {
        request_id: 3,
        body: ClientRequest::Unsubscribe {
            subscription: SubscriptionId(2),
        },
    })
    .await;
    assert!(matches!(
        recv().await,
        WsServerMsg::Response { request_id: 3, .. }
    ));
    send(WsClientMsg::Request {
        request_id: 4,
        body: ClientRequest::Unsubscribe {
            subscription: SubscriptionId(1),
        },
    })
    .await;
    assert!(matches!(
        recv().await,
        WsServerMsg::Response { request_id: 4, .. }
    ));
    {
        let mut state = state.lock().unwrap();
        state.head = 2;
        state.items = vec![todo(3, 10)];
    }
    hub.publish(entry(
        2,
        ServerEvent::TodosChanged(TodosChanged {
            todos: vec![TodoId(2)],
        }),
    ));
    assert!(matches!(recv().await, WsServerMsg::Entry { entry } if entry.id == TxLogId(2)));
    // Kept, the pinned is listed again, and subscribed to as listed.
    while state.lock().unwrap().listings < 5 {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    send(subscribe(5, 3, "pinned", 2)).await;
    assert_eq!(keys(&window(recv().await)), (vec!["todo:3"], vec![]));
    assert_eq!(state.lock().unwrap().listings, 5);
}
