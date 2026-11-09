//! Without `server.tls`, uplink serves plain HTTP, for a reverse proxy in
//! front that terminates TLS (SPEC §13.6).

mod common;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use common::{start_server, PASSWORD, USER};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uplink_tx_log::Registry;

/// The status line of a GET of `/`, with `credentials` (`user:password`)
/// if given.
async fn get(addr: std::net::SocketAddr, credentials: Option<&str>) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let authorization = credentials.map_or(String::new(), |c| {
        format!("Authorization: Basic {}\r\n", BASE64.encode(c))
    });
    stream
        .write_all(
            format!(
                "GET / HTTP/1.1\r\nHost: localhost\r\n{authorization}Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response.lines().next().unwrap().to_owned()
}

#[tokio::test]
async fn serves_plain_http_without_tls() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("uplink.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  bind: \"127.0.0.1:0\"\n  basic_auth:\n    user: {USER}\n    password_bcrypt: \"$2b$04$G1zE/xiQeRXdPalTbG/Quu55uU6n5a9ICljrYbTpceT/qklM.0yay\"\ndata_dir: data\n"
        ),
    )
    .unwrap();
    let (addr, _) = start_server(&config_path, Registry::new()).await;
    assert_eq!(get(addr, None).await, "HTTP/1.1 401 Unauthorized");
    assert_eq!(
        get(addr, Some(&format!("{USER}:{PASSWORD}"))).await,
        "HTTP/1.1 200 OK"
    );
    // The data directory is the config file's.
    assert!(dir.path().join("data/db").is_dir());
}
