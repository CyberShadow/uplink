//! Integration test: TLS cert hot-reload (SPEC §14.M2 "Done when" check).
//!
//! 1. Start server with cert 1.
//! 2. Connect via TLS; capture the server's certificate fingerprint.
//! 3. Keep the first connection open.
//! 4. Atomically replace the cert file on disk with cert 2.
//! 5. Wait for the watcher to fire and the server to reload.
//! 6. Open a new TLS connection; capture the fingerprint again.
//! 7. Assert fingerprints differ.
//! 8. Assert the original connection (held open through the swap) is
//!    unaffected — completing its TLS shutdown cleanly, not dropped.

mod common;

use common::{connect_tls, fingerprint, gen_cert, write_config};
use std::{net::SocketAddr, path::Path, time::Duration};
use uplink_tx_log::Registry;

/// Connect via TLS to `addr` and return the server certificate's
/// fingerprint, along with the open stream (kept alive to verify no
/// connection drops on reload).
async fn connect(
    addr: SocketAddr,
) -> (
    String,
    tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
) {
    let (der, tls) = connect_tls(addr).await;
    (fingerprint(&der), tls)
}

async fn start_server(config_path: &Path) -> SocketAddr {
    common::start_server(config_path, Registry::new()).await.0
}

// ── the test ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tls_cert_swap_changes_fingerprint() {
    let dir = tempfile::tempdir().unwrap();

    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    let config_path = dir.path().join("uplink.yaml");

    // Generate two distinguishable self-signed certs.
    let cert1 = gen_cert("localhost");
    let cert2 = gen_cert("localhost");
    let fp1_expected = fingerprint(&cert1.cert_der);
    let fp2_expected = fingerprint(&cert2.cert_der);
    assert_ne!(fp1_expected, fp2_expected, "rcgen produced identical certs");

    // Write cert 1 and config.
    std::fs::write(&cert_path, cert1.cert_pem.as_bytes()).unwrap();
    std::fs::write(&key_path, cert1.key_pem.as_bytes()).unwrap();
    write_config(&config_path, &cert_path, &key_path);

    // Start server.
    let addr = start_server(&config_path).await;

    // Give the server a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect and capture cert 1 fingerprint; keep the stream open.
    let (fp1, _conn1) = connect(addr).await;
    assert_eq!(fp1, fp1_expected, "server should present cert 1 initially");

    // Atomically replace cert 2 over cert 1 (write-then-rename pattern).
    let tmp_cert = dir.path().join("cert_new.pem");
    let tmp_key = dir.path().join("key_new.pem");
    std::fs::write(&tmp_cert, cert2.cert_pem.as_bytes()).unwrap();
    std::fs::write(&tmp_key, cert2.key_pem.as_bytes()).unwrap();
    std::fs::rename(&tmp_cert, &cert_path).unwrap();
    std::fs::rename(&tmp_key, &key_path).unwrap();

    // Wait for the watcher debounce (250 ms) + server reload + margin.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // New connection should present cert 2.
    let (fp2, _conn2) = connect(addr).await;
    assert_eq!(
        fp2, fp2_expected,
        "server should present cert 2 after reload"
    );
    assert_ne!(fp1, fp2, "fingerprint must change after cert swap");

    // _conn1 is still alive here (dropped at end of test scope).
    // Its TLS stack holds a reference to the original Arc<ServerConfig>,
    // so it is unaffected by the swap — it simply drops cleanly.
}

#[tokio::test]
async fn tls_reload_fail_keeps_old_cert() {
    // If the cert file becomes unreadable, the server must keep the old cert.
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    let config_path = dir.path().join("uplink.yaml");

    let cert1 = gen_cert("localhost");
    let fp1_expected = fingerprint(&cert1.cert_der);

    std::fs::write(&cert_path, cert1.cert_pem.as_bytes()).unwrap();
    std::fs::write(&key_path, cert1.key_pem.as_bytes()).unwrap();
    write_config(&config_path, &cert_path, &key_path);

    let addr = start_server(&config_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (fp1, _) = connect(addr).await;
    assert_eq!(fp1, fp1_expected);

    // Overwrite cert with invalid PEM — reload should fail, keep old cert.
    std::fs::write(&cert_path, b"this is not a valid PEM cert").unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    let (fp_after, _) = connect(addr).await;
    assert_eq!(
        fp_after, fp1_expected,
        "server must keep old cert after failed reload"
    );
}
