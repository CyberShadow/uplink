//! WebSocket protocol and tx_log undo, end to end over TLS (SPEC §14 M3
//! "Done when").

mod common;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use common::{connect_tls, gen_cert, start_server, write_config, PASSWORD, USER};
use futures_util::{SinkExt, StreamExt};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, protocol::frame::coding::CloseCode, Message},
    WebSocketStream,
};
use uplink_protocol::{
    ClientRequest, Cursor, ErrorCode, Resume, ServerEvent, ServerResponse, TxLogId, WsClientMsg,
    WsServerMsg,
};
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::{
    sample::{self, SampleInsert},
    Dispatcher, Registry,
};

type Ws = WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

struct Server {
    _dir: tempfile::TempDir,
    data_root: PathBuf,
    addr: SocketAddr,
    dispatcher: Dispatcher,
}

/// A server whose database has the `sample` table and ops.
async fn server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let cert = gen_cert("localhost");
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert_pem).unwrap();
    std::fs::write(&key_path, cert.key_pem).unwrap();
    let config_path = dir.path().join("uplink.yaml");
    write_config(&config_path, &cert_path, &key_path);

    let data_root = dir.path().join("data");
    uplink_storage::open(&data_root)
        .unwrap()
        .execute_batch(sample::SCHEMA)
        .unwrap();

    let mut registry = Registry::new();
    sample::register(&mut registry);
    let (addr, dispatcher) = start_server(&config_path, registry).await;
    Server {
        _dir: dir,
        data_root,
        addr,
        dispatcher,
    }
}

impl Server {
    fn db(&self) -> Connection {
        Connection::open(uplink_storage::db_path(&self.data_root)).unwrap()
    }

    fn tx_log_count(&self) -> i64 {
        self.db()
            .query_row("SELECT count(*) FROM tx_log", [], |r| r.get(0))
            .unwrap()
    }

    fn samples(&self) -> Vec<(i64, i64)> {
        self.db()
            .prepare("SELECT id, x FROM sample ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
}

async fn ws_connect(
    addr: SocketAddr,
    subprotocol: Option<&str>,
    auth: bool,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut request = "wss://localhost/ws".into_client_request().unwrap();
    let headers = request.headers_mut();
    if let Some(p) = subprotocol {
        headers.insert("sec-websocket-protocol", p.parse().unwrap());
    }
    if auth {
        let credentials = BASE64.encode(format!("{USER}:{PASSWORD}"));
        headers.insert(
            "authorization",
            format!("Basic {credentials}").parse().unwrap(),
        );
    }
    let (_, tls) = connect_tls(addr).await;
    let (ws, _) = tokio_tungstenite::client_async(request, tls).await?;
    Ok(ws)
}

async fn connect(addr: SocketAddr) -> Ws {
    ws_connect(addr, Some("uplink.v1"), true).await.unwrap()
}

async fn send(ws: &mut Ws, msg: &WsClientMsg) {
    ws.send(Message::text(serde_json::to_string(msg).unwrap()))
        .await
        .unwrap();
}

async fn next_frame(ws: &mut Ws) -> Message {
    tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for server message")
        .expect("connection ended")
        .expect("websocket error")
}

async fn recv(ws: &mut Ws) -> WsServerMsg {
    match next_frame(ws).await {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

async fn recv_event(ws: &mut Ws) -> (Cursor, ServerEvent) {
    match recv(ws).await {
        WsServerMsg::Event { cursor, body } => (cursor, body),
        other => panic!("expected an event, got {other:?}"),
    }
}

async fn hello(ws: &mut Ws, resume: Option<Resume>) -> (u64, Cursor, bool) {
    send(ws, &WsClientMsg::Hello { resume }).await;
    match recv(ws).await {
        WsServerMsg::Welcome {
            epoch,
            cursor,
            resumed,
        } => (epoch, cursor, resumed),
        other => panic!("expected welcome, got {other:?}"),
    }
}

async fn undo(ws: &mut Ws, request_id: u64, id: TxLogId) -> WsServerMsg {
    send(
        ws,
        &WsClientMsg::Request {
            request_id,
            body: ClientRequest::Undo { tx_log_id: id },
        },
    )
    .await;
    recv(ws).await
}

fn undone(msg: WsServerMsg, request_id: u64) -> TxLogId {
    match msg {
        WsServerMsg::Response {
            request_id: r,
            body: ServerResponse::Undone { tx_log_id },
        } if r == request_id => tx_log_id,
        other => panic!("expected undone response to {request_id}, got {other:?}"),
    }
}

fn added(event: &ServerEvent) -> (TxLogId, Option<TxLogId>) {
    match event {
        ServerEvent::TxLogEntryAdded { entry } => (entry.id, entry.undoes),
        other => panic!("expected tx_log_entry_added, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_undo_redo_over_websocket() {
    let server = server().await;
    let mut ws = connect(server.addr).await;
    let (epoch, base, resumed) = hello(&mut ws, None).await;
    assert!(!resumed);

    let mut events = vec![];

    // Dispatch: one entry, echoed to the client.
    let op = server
        .dispatcher
        .dispatch(SampleInsert { id: 1, x: 42 })
        .await
        .unwrap();
    assert_eq!(server.tx_log_count(), 1);
    assert_eq!(server.samples(), vec![(1, 42)]);
    let e = recv_event(&mut ws).await;
    assert_eq!(added(&e.1), (op, None));
    events.push(e);

    // Undo: the row is gone; the undo is a new entry.
    let undo_id = undone(undo(&mut ws, 1, op).await, 1);
    assert_eq!(server.tx_log_count(), 2);
    assert_eq!(server.samples(), vec![]);
    let e = recv_event(&mut ws).await;
    assert_eq!(added(&e.1), (undo_id, Some(op)));
    events.push(e);
    let e = recv_event(&mut ws).await;
    assert_eq!(
        e.1,
        ServerEvent::TxLogEntryUndone {
            id: op,
            undone_by: undo_id
        }
    );
    events.push(e);

    // Undo of the undo: the row is back.
    let redo_id = undone(undo(&mut ws, 2, undo_id).await, 2);
    assert_eq!(server.tx_log_count(), 3);
    assert_eq!(server.samples(), vec![(1, 42)]);
    let e = recv_event(&mut ws).await;
    assert_eq!(added(&e.1), (redo_id, Some(undo_id)));
    events.push(e);
    let e = recv_event(&mut ws).await;
    assert_eq!(
        e.1,
        ServerEvent::TxLogEntryUndone {
            id: undo_id,
            undone_by: redo_id
        }
    );
    events.push(e);

    // The original entry can't be undone twice.
    match undo(&mut ws, 3, op).await {
        WsServerMsg::RequestFailed { request_id, error } => {
            assert_eq!(request_id, 3);
            assert_eq!(error.code, ErrorCode::AlreadyUndone);
        }
        other => panic!("expected failure, got {other:?}"),
    }
    match undo(&mut ws, 4, TxLogId(99)).await {
        WsServerMsg::RequestFailed { error, .. } => assert_eq!(error.code, ErrorCode::NotFound),
        other => panic!("expected failure, got {other:?}"),
    }
    assert_eq!(server.tx_log_count(), 3);

    // Cursors are consecutive.
    let cursors: Vec<Cursor> = events.iter().map(|e| e.0).collect();
    assert_eq!(cursors, (base + 1..=base + 5).collect::<Vec<_>>());

    // A reconnecting client is replayed what it missed.
    drop(ws);
    let mut ws = connect(server.addr).await;
    let (_, head, resumed) = hello(
        &mut ws,
        Some(Resume {
            epoch,
            cursor: base + 2,
        }),
    )
    .await;
    assert!(resumed);
    assert_eq!(head, base + 5);
    for expected in &events[2..] {
        assert_eq!(&recv_event(&mut ws).await, expected);
    }
}

#[tokio::test]
async fn resume_from_another_epoch_requires_resync() {
    let server = server().await;
    let mut ws = connect(server.addr).await;
    let (epoch, cursor, _) = hello(&mut ws, None).await;
    let mut ws = connect(server.addr).await;
    let (_, _, resumed) = hello(
        &mut ws,
        Some(Resume {
            epoch: epoch + 1,
            cursor,
        }),
    )
    .await;
    assert!(!resumed);
}

#[tokio::test]
async fn ping_is_answered() {
    let server = server().await;
    let mut ws = connect(server.addr).await;
    hello(&mut ws, None).await;
    send(&mut ws, &WsClientMsg::Ping { seq: 17 }).await;
    assert_eq!(recv(&mut ws).await, WsServerMsg::Pong { seq: 17 });
}

#[tokio::test]
async fn request_before_hello_closes_connection() {
    let server = server().await;
    let mut ws = connect(server.addr).await;
    send(&mut ws, &WsClientMsg::Ping { seq: 1 }).await;
    match next_frame(&mut ws).await {
        Message::Close(Some(frame)) => assert_eq!(frame.code, CloseCode::Protocol),
        other => panic!("expected close, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_message_closes_connection() {
    let server = server().await;
    let mut ws = connect(server.addr).await;
    hello(&mut ws, None).await;
    ws.send(Message::text("{\"type\":\"nope\"}")).await.unwrap();
    match next_frame(&mut ws).await {
        Message::Close(Some(frame)) => assert_eq!(frame.code, CloseCode::Protocol),
        other => panic!("expected close, got {other:?}"),
    }
}

fn http_status(err: tokio_tungstenite::tungstenite::Error) -> u16 {
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP error, got {other:?}"),
    }
}

#[tokio::test]
async fn upgrade_requires_subprotocol() {
    let server = server().await;
    let err = ws_connect(server.addr, None, true).await.err().unwrap();
    assert_eq!(http_status(err), 400);
    let err = ws_connect(server.addr, Some("other"), true)
        .await
        .err()
        .unwrap();
    assert_eq!(http_status(err), 400);
}

#[tokio::test]
async fn upgrade_requires_auth() {
    let server = server().await;
    let err = ws_connect(server.addr, Some("uplink.v1"), false)
        .await
        .err()
        .unwrap();
    assert_eq!(http_status(err), 401);
}
