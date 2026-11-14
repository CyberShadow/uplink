//! A session is sent the badges at once, and again as they change, but
//! not when they are set as they were (SPEC §12.14).

use axum::{extract::WebSocketUpgrade, routing::get, Router};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;
use uplink_protocol::{
    Badges, ClientRequest, Mark, Marks, ServerError, ServerResponse, TxLogId, UnknownNames,
    WsClientMsg, WsServerMsg,
};
use uplink_realtime::{run_session, Answer, Backend, Hub, Listing, Replay};

struct Idle;

impl Backend for Idle {
    async fn handle(&self, _request: ClientRequest) -> Answer {
        Answer::Now(Ok(ServerResponse::Done))
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

/// Of `count` items in a `count` bucket.
fn badges(count: u32) -> Badges {
    Badges {
        marks: Marks {
            home: Mark { count, dot: false },
            ..Marks::default()
        },
    }
}

#[tokio::test]
async fn badges_are_sent_at_once_and_as_they_change() {
    let hub = Arc::new(Hub::new(16));
    hub.set_badges(badges(1));
    let serving = hub.clone();
    let app = Router::new().route(
        "/ws",
        get(move |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(move |socket| async move { run_session(socket, &serving, &Idle).await })
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    let hello = WsClientMsg::Hello {
        version: uplink_protocol::typescript::version().to_owned(),
        since: None,
    };
    ws.send(Message::text(serde_json::to_string(&hello).unwrap()))
        .await
        .unwrap();
    let mut recv = async || match ws.next().await.unwrap().unwrap() {
        Message::Text(text) => serde_json::from_str::<WsServerMsg>(&text).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    };
    assert!(matches!(recv().await, WsServerMsg::Welcome { .. }));
    let sent = |count| WsServerMsg::Badges {
        badges: badges(count),
    };
    assert_eq!(recv().await, sent(1));
    // As they were: not sent again.
    hub.set_badges(badges(1));
    hub.set_badges(badges(2));
    assert_eq!(recv().await, sent(2));
}
