use crate::hub::Hub;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::{stream::FuturesUnordered, StreamExt};
use std::{convert::Infallible, future::Future, sync::Arc, time::Duration};
use tokio::{
    sync::broadcast::{error::RecvError, Receiver},
    task::JoinHandle,
    time::Instant,
};
use uplink_protocol::{
    ClientRequest, LogEntry, ServerError, ServerResponse, TxLogId, WsClientMsg, WsServerMsg,
};

/// WebSocket subprotocol spoken by [`run_session`] (SPEC §5.1).
pub const PROTOCOL: &str = "uplink.v1";

/// The connection is closed after this long without any client frame. The
/// client pings every 30 s.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(60);

const CLOSE_PROTOCOL_ERROR: u16 = 1002;
const CLOSE_HEARTBEAT_TIMEOUT: u16 = 4000;
/// The session fell further behind the live stream than the hub buffers.
/// The client should reconnect and resume.
const CLOSE_LAGGED: u16 = 4001;
const CLOSE_INTERNAL_ERROR: u16 = 1011;
/// The client speaks another version of the protocol: it is from another
/// build, and should load anew.
const CLOSE_PROTOCOL_VERSION: u16 = 4002;

/// What sessions need from the rest of the server.
pub trait Backend: Send + Sync {
    /// Executes a client request (SPEC §5.6). A session handles its
    /// requests one at a time, in order, so this must not wait on anything
    /// remote: what does is answered [`Answer::later`].
    fn handle(&self, request: ClientRequest) -> impl Future<Output = Answer> + Send;

    /// Reads the log for a client saying hello with `since`.
    fn replay(&self, since: Option<TxLogId>)
        -> impl Future<Output = anyhow::Result<Replay>> + Send;
}

pub type Response = Result<ServerResponse, ServerError>;

/// How a request is answered. Short-lived, one per request: the size
/// of a response held at once is no matter.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Answer {
    /// Of what the server has: its database, its clients' stores.
    Now(Response),
    /// By work that waits on something remote (a homeserver), running on
    /// its own: the session goes on to the next request meanwhile, and
    /// answers when it is done. It is done even if the session ends first,
    /// so that what it stores is not lost.
    Later(JoinHandle<Response>),
}

impl Answer {
    pub fn later(work: impl Future<Output = Response> + Send + 'static) -> Self {
        Answer::Later(tokio::spawn(work))
    }

    /// The response, once there is one.
    pub async fn response(self) -> Response {
        match self {
            Answer::Now(response) => response,
            Answer::Later(work) => work.await.expect("a request's work panicked"),
        }
    }
}

impl From<Response> for Answer {
    fn from(response: Response) -> Self {
        Answer::Now(response)
    }
}

/// A request whose remote part runs later, unless its local part refused
/// it.
impl From<Result<Answer, ServerError>> for Answer {
    fn from(answer: Result<Answer, ServerError>) -> Self {
        answer.unwrap_or_else(|error| Answer::Now(Err(error)))
    }
}

pub struct Replay {
    /// The most recent entry.
    pub head: TxLogId,
    /// The entries after `since`, oldest first, if the client can resume
    /// from it; `None` if it must reload.
    pub entries: Option<Vec<LogEntry>>,
}

/// Speaks the uplink protocol on an upgraded WebSocket until either side
/// ends the connection.
pub async fn run_session(mut socket: WebSocket, hub: &Hub, backend: &impl Backend) {
    let Err(end) = session(&mut socket, hub, backend).await;
    match end {
        End::ClientClosed => tracing::debug!("ws: client closed"),
        End::Transport(e) => tracing::debug!("ws: transport error: {e}"),
        End::Close { code, reason } => {
            tracing::info!("ws: closing ({code}): {reason}");
            let frame = CloseFrame {
                code,
                reason: reason.into(),
            };
            // The connection is being torn down; failure to deliver the
            // close frame changes nothing.
            let _ = socket.send(Message::Close(Some(frame))).await;
        }
    }
}

enum End {
    ClientClosed,
    Transport(axum::Error),
    Close { code: u16, reason: String },
}

impl From<axum::Error> for End {
    fn from(e: axum::Error) -> Self {
        End::Transport(e)
    }
}

fn protocol_error(reason: impl Into<String>) -> End {
    End::Close {
        code: CLOSE_PROTOCOL_ERROR,
        reason: reason.into(),
    }
}

/// Runs the protocol; only ever returns how the session ended.
async fn session(
    socket: &mut WebSocket,
    hub: &Hub,
    backend: &impl Backend,
) -> Result<Infallible, End> {
    let mut deadline = Instant::now() + SILENCE_TIMEOUT;

    let since = match recv(socket, &mut deadline).await? {
        WsClientMsg::Hello { version, since }
            if version == uplink_protocol::typescript::version() =>
        {
            since
        }
        WsClientMsg::Hello { version, .. } => {
            return Err(End::Close {
                code: CLOSE_PROTOCOL_VERSION,
                reason: format!(
                    "protocol version {version}; the server's is {}",
                    uplink_protocol::typescript::version()
                ),
            })
        }
        other => return Err(protocol_error(format!("expected hello, got {other:?}"))),
    };

    // Subscribe before reading the log: an entry committed meanwhile is
    // then seen by both, and the live copy is skipped below, rather than by
    // neither.
    let mut live = hub.subscribe();
    let mut notices = hub.notices();
    let replay = backend.replay(since).await.map_err(|e| End::Close {
        code: CLOSE_INTERNAL_ERROR,
        reason: format!("reading log: {e:#}"),
    })?;
    send(
        socket,
        &WsServerMsg::Welcome {
            cursor: replay.head,
            resumed: replay.entries.is_some(),
        },
    )
    .await?;
    for entry in replay.entries.into_iter().flatten() {
        send(socket, &WsServerMsg::Entry { entry }).await?;
    }
    let mut cursor = replay.head;
    // Requests answered later, by their ids.
    let mut answers = FuturesUnordered::new();

    loop {
        tokio::select! {
            msg = recv(socket, &mut deadline) => match msg? {
                WsClientMsg::Ping { seq } => send(socket, &WsServerMsg::Pong { seq }).await?,
                WsClientMsg::Hello { .. } => return Err(protocol_error("duplicate hello")),
                WsClientMsg::Request { request_id, body } => match backend.handle(body).await {
                    Answer::Now(response) => {
                        respond(socket, &mut live, &mut cursor, request_id, response).await?;
                    }
                    later @ Answer::Later(_) => answers.push(async move {
                        (request_id, later.response().await)
                    }),
                },
            },
            Some((request_id, response)) = answers.next(), if !answers.is_empty() => {
                respond(socket, &mut live, &mut cursor, request_id, response).await?;
            }
            published = live.recv() => forward(socket, published, &mut cursor).await?,
            notice = notices.recv() => match notice {
                Ok(notice) => send(socket, &WsServerMsg::Notice { notice: (*notice).clone() }).await?,
                // Of the moment: those missed are past.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => unreachable!("hub outlives its sessions"),
            },
        }
    }
}

/// Answers the request `request_id`. The entries a response reflects go
/// out before it, so every entry after it is news to the client (SPEC
/// §5.4).
async fn respond(
    socket: &mut WebSocket,
    live: &mut Receiver<Arc<LogEntry>>,
    cursor: &mut TxLogId,
    request_id: u64,
    response: Response,
) -> Result<(), End> {
    let reply = match response {
        Ok(body) => {
            if let Some(position) = body.position() {
                while *cursor < position {
                    forward(socket, live.recv().await, cursor).await?;
                }
            }
            WsServerMsg::Response { request_id, body }
        }
        Err(error) => WsServerMsg::RequestFailed { request_id, error },
    };
    send(socket, &reply).await
}

/// Sends a live entry the client has not seen yet, advancing `cursor`.
async fn forward(
    socket: &mut WebSocket,
    published: Result<Arc<LogEntry>, RecvError>,
    cursor: &mut TxLogId,
) -> Result<(), End> {
    match published {
        Ok(entry) if entry.id <= *cursor => Ok(()),
        Ok(entry) => {
            *cursor = entry.id;
            send(
                socket,
                &WsServerMsg::Entry {
                    entry: (*entry).clone(),
                },
            )
            .await
        }
        Err(RecvError::Lagged(n)) => Err(End::Close {
            code: CLOSE_LAGGED,
            reason: format!("lagged {n} entries behind"),
        }),
        Err(RecvError::Closed) => unreachable!("hub outlives its sessions"),
    }
}

/// Receives the next protocol message. Every client frame postpones
/// `deadline`; reaching it ends the session.
async fn recv(socket: &mut WebSocket, deadline: &mut Instant) -> Result<WsClientMsg, End> {
    loop {
        let frame = tokio::time::timeout_at(*deadline, socket.recv())
            .await
            .map_err(|_| End::Close {
                code: CLOSE_HEARTBEAT_TIMEOUT,
                reason: "heartbeat timeout".into(),
            })?;
        *deadline = Instant::now() + SILENCE_TIMEOUT;
        match frame {
            None | Some(Ok(Message::Close(_))) => return Err(End::ClientClosed),
            Some(Err(e)) => return Err(End::Transport(e)),
            // Answered by the WebSocket layer itself.
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Binary(_))) => return Err(protocol_error("binary frame")),
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text)
                    .map_err(|e| protocol_error(format!("malformed message: {e}")));
            }
        }
    }
}

async fn send(socket: &mut WebSocket, msg: &WsServerMsg) -> Result<(), End> {
    let text = serde_json::to_string(msg).expect("server messages always serialize");
    socket.send(Message::Text(text)).await?;
    Ok(())
}
