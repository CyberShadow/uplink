//! A request answered later holds up none after it (SPEC §5.6).

use axum::{extract::WebSocketUpgrade, routing::get, Router};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use uplink_protocol::{
    ClientRequest, ServerError, ServerResponse, TxLogId, UnknownNames, WsClientMsg, WsServerMsg,
};
use uplink_realtime::{run_session, Answer, Backend, Hub, Listing, Replay};

/// Answers `list_rooms` later, once `release` is notified; anything else
/// at once.
struct Slow {
    release: Arc<Notify>,
}

impl Backend for Slow {
    async fn handle(&self, request: ClientRequest) -> Answer {
        match request {
            ClientRequest::ListRooms => {
                let release = self.release.clone();
                Answer::later(async move {
                    release.notified().await;
                    Ok(ServerResponse::Done)
                })
            }
            _ => Answer::Now(Ok(ServerResponse::Done)),
        }
    }

    async fn replay(&self, _since: Option<TxLogId>) -> anyhow::Result<Replay> {
        Ok(Replay {
            head: TxLogId(0),
            entries: Some(vec![]),
        })
    }

    async fn window(
        &self,
        _query: String,
        _unknown: UnknownNames,
        _limit: u32,
    ) -> Result<Listing, ServerError> {
        unreachable!("nothing is subscribed to")
    }
}

#[tokio::test]
async fn a_request_answered_later_holds_up_none_after_it() {
    let release = Arc::new(Notify::new());
    let backend = Arc::new(Slow {
        release: release.clone(),
    });
    let hub = Arc::new(Hub::new(16));
    let app = Router::new().route(
        "/ws",
        get(move |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(move |socket| async move { run_session(socket, &hub, &*backend).await })
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    let mut send = async |msg: WsClientMsg| {
        ws.send(Message::text(serde_json::to_string(&msg).unwrap()))
            .await
            .unwrap();
    };
    send(WsClientMsg::Hello {
        version: uplink_protocol::typescript::version().to_owned(),
        since: None,
    })
    .await;
    send(WsClientMsg::Request {
        request_id: 1,
        body: ClientRequest::ListRooms,
    })
    .await;
    send(WsClientMsg::Request {
        request_id: 2,
        body: ClientRequest::ListAccounts,
    })
    .await;
    send(WsClientMsg::Ping { seq: 7 }).await;

    let mut recv = async || match ws.next().await.unwrap().unwrap() {
        Message::Text(text) => serde_json::from_str::<WsServerMsg>(&text).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    };
    assert!(matches!(recv().await, WsServerMsg::Welcome { .. }));
    assert!(matches!(recv().await, WsServerMsg::Badges { .. }));
    let answered = |request_id| WsServerMsg::Response {
        request_id,
        body: ServerResponse::Done,
    };
    assert_eq!(recv().await, answered(2));
    assert_eq!(recv().await, WsServerMsg::Pong { seq: 7 });
    release.notify_one();
    assert_eq!(recv().await, answered(1));
}
