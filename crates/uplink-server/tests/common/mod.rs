//! Helpers shared by the server integration tests.
#![allow(dead_code)] // Each test binary uses a different subset.

use std::{net::SocketAddr, path::Path, sync::Arc};

use rustls::pki_types::ServerName;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use uplink_config::Config;
use uplink_server::{open, serve_with_listener, Services};
use uplink_tx_log::Registry;

// ── cert-generation helpers ───────────────────────────────────────────────────

pub struct GenCert {
    pub cert_pem: String,
    pub key_pem: String,
    /// DER bytes for fingerprint computation at generation time.
    pub cert_der: Vec<u8>,
}

pub fn gen_cert(san: &str) -> GenCert {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec![san.to_owned()])
            .expect("rcgen cert generation failed");
    GenCert {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
        cert_der: cert.der().to_vec(),
    }
}

pub fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

// ── TLS client helpers ────────────────────────────────────────────────────────

/// A certificate verifier that accepts any cert and records its DER bytes.
#[derive(Debug)]
pub struct RecordCert {
    der: std::sync::Mutex<Option<Vec<u8>>>,
}

impl RecordCert {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            der: std::sync::Mutex::new(None),
        })
    }

    pub fn take_der(&self) -> Vec<u8> {
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

/// Opens a TLS connection to `addr`, accepting any server certificate.
/// Returns the certificate the server presented and the stream.
pub async fn connect_tls(
    addr: SocketAddr,
) -> (Vec<u8>, tokio_rustls::client::TlsStream<TcpStream>) {
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
    (recorder.take_der(), tls)
}

// ── server setup helpers ──────────────────────────────────────────────────────

/// Basic-auth credentials accepted by servers configured by [`write_config`].
pub const USER: &str = "alice";
pub const PASSWORD: &str = "testpass";

/// Writes a config using the given cert and key, with the data root in a
/// `data` directory next to the config file.
pub fn write_config(config_path: &Path, cert_path: &Path, key_path: &Path) {
    let yaml = format!(
        "server:\n  bind: \"127.0.0.1:0\"\n  tls:\n    cert: {cert}\n    key: {key}\n  basic_auth:\n    user: {USER}\n    password_bcrypt: \"$2b$04$G1zE/xiQeRXdPalTbG/Quu55uU6n5a9ICljrYbTpceT/qklM.0yay\"\ndata_dir: {data}\n",
        cert = cert_path.display(),
        key = key_path.display(),
        data = config_path.parent().unwrap().join("data").display(),
    );
    std::fs::write(config_path, yaml).unwrap();
}

/// Starts a server for the config at `config_path` on a free port.
pub async fn start_server(config_path: &Path, registry: Registry) -> (SocketAddr, Services) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind failed");
    let addr = listener.local_addr().unwrap();

    let config = Config::load(config_path).expect("config load failed");
    let log = open(&config, registry).expect("opening database failed");
    let cp = config_path.to_path_buf();
    let l = log.clone();
    tokio::spawn(async move {
        serve_with_listener(config, cp, listener, l)
            .await
            .expect("server failed");
    });

    (addr, log)
}
