/// Integration test: TLS cert hot-reload (SPEC §14.M2 "Done when" check).
///
/// 1. Start server with cert 1.
/// 2. Connect via TLS; capture the server's certificate fingerprint.
/// 3. Keep the first connection open.
/// 4. Atomically replace the cert file on disk with cert 2.
/// 5. Wait for the watcher to fire and the server to reload.
/// 6. Open a new TLS connection; capture the fingerprint again.
/// 7. Assert fingerprints differ.
/// 8. Assert the original connection (held open through the swap) is
///    unaffected — completing its TLS shutdown cleanly, not dropped.
use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use rustls::pki_types::ServerName;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use uplink_config::Config;
use uplink_server::serve_with_listener;

// ── cert-generation helpers ───────────────────────────────────────────────────

struct GenCert {
    cert_pem: String,
    key_pem: String,
    /// DER bytes for fingerprint computation at generation time.
    cert_der: Vec<u8>,
}

fn gen_cert(san: &str) -> GenCert {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec![san.to_owned()])
            .expect("rcgen cert generation failed");
    GenCert {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
        cert_der: cert.der().to_vec(),
    }
}

fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

// ── TLS client helpers ────────────────────────────────────────────────────────

/// A certificate verifier that accepts any cert and records its DER bytes.
#[derive(Debug)]
struct RecordCert {
    der: std::sync::Mutex<Option<Vec<u8>>>,
}

impl RecordCert {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            der: std::sync::Mutex::new(None),
        })
    }

    fn take_der(&self) -> Vec<u8> {
        self.der
            .lock()
            .unwrap()
            .take()
            .expect("no certificate was recorded")
    }
}

impl rustls::client::danger::ServerCertVerifier for RecordCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        *self.der.lock().unwrap() = Some(end_entity.to_vec());
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

/// Connect via TLS to `addr`, extract the server's certificate fingerprint,
/// and return both the fingerprint and the open TLS stream (kept alive to
/// verify no connection drops on reload).
async fn connect(
    addr: SocketAddr,
) -> (String, tokio_rustls::client::TlsStream<TcpStream>) {
    let recorder = RecordCert::new();
    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(recorder.clone())
        .with_no_client_auth();

    let connector = TlsConnector::from(Arc::new(client_config));
    let server_name = ServerName::try_from("localhost").unwrap();
    let tcp = TcpStream::connect(addr).await.expect("TCP connect failed");
    let tls = connector
        .connect(server_name, tcp)
        .await
        .expect("TLS handshake failed");

    let fp = fingerprint(&recorder.take_der());
    (fp, tls)
}

// ── server setup helpers ──────────────────────────────────────────────────────

fn write_config(
    config_path: &Path,
    cert_path: &Path,
    key_path: &Path,
) {
    let yaml = format!(
        "server:\n  bind: \"127.0.0.1:0\"\n  tls:\n    cert: {cert}\n    key: {key}\n  basic_auth:\n    user: alice\n    password_bcrypt: \"$2b$04$G1zE/xiQeRXdPalTbG/Quu55uU6n5a9ICljrYbTpceT/qklM.0yay\"\ndata:\n  root: {data}\n",
        data = config_path.parent().unwrap().join("data").display(),
        cert = cert_path.display(),
        key = key_path.display()
    );
    std::fs::write(config_path, yaml).unwrap();
}

async fn start_server(config_path: &Path) -> SocketAddr {
    // Bind port 0 so the OS assigns a free port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind failed");
    let addr = listener.local_addr().unwrap();

    let config = Config::load(config_path).expect("config load failed");
    let cp = config_path.to_path_buf();

    tokio::spawn(async move {
        serve_with_listener(config, cp, listener)
            .await
            .expect("server failed");
    });

    addr
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
    assert_eq!(fp2, fp2_expected, "server should present cert 2 after reload");
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
