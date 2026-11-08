use crate::hub::Hub;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use std::{convert::Infallible, future::Future, time::Duration};
use tokio::{sync::broadcast::error::RecvError, time::Instant};
use uplink_protocol::{ClientRequest, ServerError, ServerResponse, WsClientMsg, WsServerMsg};

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

/// Executes client requests on behalf of sessions.
pub trait RequestHandler: Send + Sync {
    fn handle(
        &self,
        request: ClientRequest,
    ) -> impl Future<Output = Result<ServerResponse, ServerError>> + Send;
}

/// Speaks the uplink protocol on an upgraded WebSocket until either side
/// ends the connection.
pub async fn run_session(mut socket: WebSocket, hub: &Hub, handler: &impl RequestHandler) {
    let Err(end) = session(&mut socket, hub, handler).await;
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
    handler: &impl RequestHandler,
) -> Result<Infallible, End> {
    let mut deadline = Instant::now() + SILENCE_TIMEOUT;

    let resume = match recv(socket, &mut deadline).await? {
        WsClientMsg::Hello { resume } => resume,
        other => return Err(protocol_error(format!("expected hello, got {other:?}"))),
    };

    let mut sub = hub.subscribe(resume);
    send(
        socket,
        &WsServerMsg::Welcome {
            epoch: hub.epoch(),
            cursor: sub.cursor,
            resumed: sub.replay.is_some(),
        },
    )
    .await?;
    for p in sub.replay.take().into_iter().flatten() {
        send(
            socket,
            &WsServerMsg::Event {
                cursor: p.cursor,
                body: p.event.clone(),
            },
        )
        .await?;
    }

    loop {
        tokio::select! {
            msg = recv(socket, &mut deadline) => match msg? {
                WsClientMsg::Ping { seq } => send(socket, &WsServerMsg::Pong { seq }).await?,
                WsClientMsg::Hello { .. } => return Err(protocol_error("duplicate hello")),
                WsClientMsg::Request { request_id, body } => {
                    let reply = match handler.handle(body).await {
                        Ok(body) => WsServerMsg::Response { request_id, body },
                        Err(error) => WsServerMsg::RequestFailed { request_id, error },
                    };
                    send(socket, &reply).await?;
                }
            },
            published = sub.live.recv() => match published {
                Ok(p) => send(socket, &WsServerMsg::Event {
                    cursor: p.cursor,
                    body: p.event.clone(),
                }).await?,
                Err(RecvError::Lagged(n)) => return Err(End::Close {
                    code: CLOSE_LAGGED,
                    reason: format!("lagged {n} events behind"),
                }),
                Err(RecvError::Closed) => unreachable!("hub outlives its sessions"),
            },
        }
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
