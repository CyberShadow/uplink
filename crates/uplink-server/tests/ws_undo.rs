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
    ClientRequest, ErrorCode, LogEntry, ServerResponse, Source, TxLogId, WsClientMsg, WsServerMsg,
};
use uplink_server::Services;
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::{
    sample::{self, SampleInsert, SampleObserved},
    Registry,
};

type Ws = WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

struct Server {
    dir: tempfile::TempDir,
    data_root: PathBuf,
    addr: SocketAddr,
    log: Services,
}

/// A server whose database has the `sample` table and ops.
async fn server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    setup(&dir);
    start(dir).await
}

/// Writes a config and certificate, and creates the `sample` table.
fn setup(dir: &tempfile::TempDir) {
    let cert = gen_cert("localhost");
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert_pem).unwrap();
    std::fs::write(&key_path, cert.key_pem).unwrap();
    let config_path = dir.path().join("uplink.yaml");
    write_config(&config_path, &cert_path, &key_path);

    uplink_storage::open(&dir.path().join("data"))
        .unwrap()
        .execute_batch(sample::SCHEMA)
        .unwrap();
}

async fn start(dir: tempfile::TempDir) -> Server {
    let mut registry = Registry::new();
    sample::register(&mut registry);
    let (addr, log) = start_server(&dir.path().join("uplink.yaml"), registry).await;
    Server {
        data_root: dir.path().join("data"),
        dir,
        addr,
        log,
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

async fn ws_connect(addr: SocketAddr, subprotocol: Option<&str>, auth: bool) -> Result<Ws, u16> {
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
    let (ws, _) = tokio_tungstenite::client_async(request, tls)
        .await
        .map_err(http_status)?;
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

async fn recv_entry(ws: &mut Ws) -> LogEntry {
    entry(recv(ws).await)
}

fn entry(msg: WsServerMsg) -> LogEntry {
    match msg {
        WsServerMsg::Entry { entry } => entry,
        other => panic!("expected an entry, got {other:?}"),
    }
}

async fn hello(ws: &mut Ws, since: Option<TxLogId>) -> (TxLogId, bool) {
    let version = uplink_protocol::typescript::version().to_owned();
    send(ws, &WsClientMsg::Hello { version, since }).await;
    let welcome = match recv(ws).await {
        WsServerMsg::Welcome { cursor, resumed } => (cursor, resumed),
        other => panic!("expected welcome, got {other:?}"),
    };
    assert!(matches!(recv(ws).await, WsServerMsg::Badges { .. }));
    welcome
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

#[tokio::test]
async fn dispatch_undo_redo_over_websocket() {
    let server = server().await;
    let mut ws = connect(server.addr).await;
    let (base, resumed) = hello(&mut ws, None).await;
    assert_eq!(base, TxLogId(0));
    assert!(!resumed);

    let mut entries = vec![];

    // Dispatch: one entry, echoed to the client.
    let op = server
        .log
        .dispatcher
        .dispatch(SampleInsert { id: 1, x: 42 })
        .await
        .unwrap();
    assert_eq!(server.tx_log_count(), 1);
    assert_eq!(server.samples(), vec![(1, 42)]);
    let e = recv_entry(&mut ws).await;
    assert_eq!((e.id, e.source.clone(), e.undoes), (op, Source::User, None));
    entries.push(e);

    // Undo: the row is gone; the undo is a new entry, which reaches the
    // client before the response does.
    let e = entry(undo(&mut ws, 1, op).await);
    let undo_id = undone(recv(&mut ws).await, 1);
    assert_eq!(server.tx_log_count(), 2);
    assert_eq!(server.samples(), vec![]);
    assert_eq!((e.id, e.undoes), (undo_id, Some(op)));
    entries.push(e);

    // Undo of the undo: the row is back.
    let e = entry(undo(&mut ws, 2, undo_id).await);
    let redo_id = undone(recv(&mut ws).await, 2);
    assert_eq!(server.tx_log_count(), 3);
    assert_eq!(server.samples(), vec![(1, 42)]);
    assert_eq!((e.id, e.undoes), (redo_id, Some(undo_id)));
    entries.push(e);

    // An observation shares the stream but can't be undone.
    let observed = server
        .log
        .dispatcher
        .observe("acct".into(), SampleObserved { id: 2, x: 7 })
        .await
        .unwrap();
    let e = recv_entry(&mut ws).await;
    assert_eq!(
        (e.id, e.source.clone()),
        (observed, Source::Account { id: "acct".into() })
    );
    entries.push(e);
    let failed = |msg: WsServerMsg, id: u64| match msg {
        WsServerMsg::RequestFailed { request_id, error } if request_id == id => error.code,
        other => panic!("expected failure of {id}, got {other:?}"),
    };
    assert_eq!(
        failed(undo(&mut ws, 3, observed).await, 3),
        ErrorCode::NotUndoable
    );

    // The original entry can't be undone twice.
    assert_eq!(
        failed(undo(&mut ws, 4, op).await, 4),
        ErrorCode::AlreadyUndone
    );
    assert_eq!(
        failed(undo(&mut ws, 5, TxLogId(99)).await, 5),
        ErrorCode::NotFound
    );
    assert_eq!(server.tx_log_count(), 4);

    // Entry ids are consecutive.
    let ids: Vec<i64> = entries.iter().map(|e| e.id.0).collect();
    assert_eq!(ids, vec![1, 2, 3, 4]);

    // A reconnecting client is replayed what it missed.
    drop(ws);
    let mut ws = connect(server.addr).await;
    let (head, resumed) = hello(&mut ws, Some(TxLogId(2))).await;
    assert!(resumed);
    assert_eq!(head, TxLogId(4));
    for expected in &entries[2..] {
        assert_eq!(&recv_entry(&mut ws).await, expected);
    }

    // ... also by a fresh server on the same data.
    let Server { dir, .. } = server;
    let server = start(dir).await;
    let mut ws = connect(server.addr).await;
    let (head, resumed) = hello(&mut ws, Some(TxLogId(3))).await;
    assert!(resumed);
    assert_eq!(head, TxLogId(4));
    assert_eq!(&recv_entry(&mut ws).await, &entries[3]);
}

#[tokio::test]
async fn resume_from_unknown_position_requires_reload() {
    let server = server().await;
    let mut ws = connect(server.addr).await;
    let (cursor, resumed) = hello(&mut ws, Some(TxLogId(5))).await;
    assert_eq!(cursor, TxLogId(0));
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
async fn another_protocol_version_closes_connection() {
    let server = server().await;
    let mut ws = connect(server.addr).await;
    let version = "0123456789abcdef".to_owned();
    send(
        &mut ws,
        &WsClientMsg::Hello {
            version,
            since: None,
        },
    )
    .await;
    match next_frame(&mut ws).await {
        Message::Close(Some(frame)) => assert_eq!(frame.code, CloseCode::Library(4002)),
        other => panic!("expected close, got {other:?}"),
    }
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

/// The status of a refused upgrade.
fn http_status(err: tokio_tungstenite::tungstenite::Error) -> u16 {
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP error, got {other:?}"),
    }
}

#[tokio::test]
async fn upgrade_requires_subprotocol() {
    let server = server().await;
    let status = ws_connect(server.addr, None, true).await.err().unwrap();
    assert_eq!(status, 400);
    let status = ws_connect(server.addr, Some("other"), true)
        .await
        .err()
        .unwrap();
    assert_eq!(status, 400);
}

#[tokio::test]
async fn upgrade_requires_auth() {
    let server = server().await;
    let status = ws_connect(server.addr, Some("uplink.v1"), false)
        .await
        .err()
        .unwrap();
    assert_eq!(status, 401);
}
