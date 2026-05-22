use anyhow::{Context, Result};
use axum::{
    extract::Request,
    http::{header, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use hyper::body::Incoming;
use rust_embed::Embed;
use rustls::ServerConfig;
use rustls_pemfile::{certs, private_key};
use std::{io::BufReader, net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tracing::info;
use uplink_config::Config;

#[derive(Embed)]
#[folder = "static/"]
struct StaticFiles;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
}

pub fn run() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_async())
}

async fn run_async() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "uplink=info".into()),
        )
        .init();

    let config_path = parse_config_flag();
    let config = Arc::new(Config::load(&config_path)?);
    let state = AppState {
        config: config.clone(),
    };

    let bind: SocketAddr = config
        .server
        .bind
        .parse()
        .with_context(|| format!("parsing bind address '{}'", config.server.bind))?;

    let tls_acceptor = build_tls_acceptor(&config)?;
    let listener = TcpListener::bind(&bind).await?;
    info!("listening on https://{}", bind);

    let app = build_app(state);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let acceptor = tls_acceptor.clone();
        let tower_service = app.clone();

        tokio::spawn(async move {
            match acceptor.accept(stream).await {
                Ok(tls_stream) => {
                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let hyper_service = hyper::service::service_fn(move |req: Request<Incoming>| {
                        tower_service.clone().call(req)
                    });
                    let result = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(io, hyper_service)
                    .await;
                    if let Err(e) = result {
                        tracing::debug!("connection error from {}: {}", peer_addr, e);
                    }
                }
                Err(e) => tracing::debug!("TLS handshake error from {}: {}", peer_addr, e),
            }
        });
    }
}

fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .fallback(static_file_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            basic_auth_middleware,
        ))
        .with_state(state)
}

async fn index_handler() -> impl IntoResponse {
    if let Some(file) = StaticFiles::get("index.html") {
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            file.data.into_owned(),
        )
            .into_response()
    } else {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain")],
            "hello from uplink",
        )
            .into_response()
    }
}

async fn static_file_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    match StaticFiles::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref().to_owned())],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn basic_auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let Some(auth) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
    else {
        return unauthorized();
    };

    let Ok(decoded_bytes) = BASE64.decode(auth.trim()) else {
        return unauthorized();
    };
    let Ok(decoded) = String::from_utf8(decoded_bytes) else {
        return unauthorized();
    };
    let Some((username, password)) = decoded.split_once(':') else {
        return unauthorized();
    };

    let expected_user = state.config.server.basic_auth.user.as_bytes();
    let user_match =
        subtle::ConstantTimeEq::ct_eq(username.as_bytes(), expected_user).unwrap_u8();

    if user_match != 1 {
        return unauthorized();
    }

    match bcrypt::verify(password, &state.config.server.basic_auth.password_bcrypt) {
        Ok(true) => next.run(req).await,
        _ => unauthorized(),
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, r#"Basic realm="uplink""#)],
        "Unauthorized\n",
    )
        .into_response()
}

fn build_tls_acceptor(config: &Config) -> Result<TlsAcceptor> {
    let cert_bytes = std::fs::read(&config.server.tls.cert)
        .with_context(|| format!("reading TLS cert '{}'", config.server.tls.cert))?;
    let key_bytes = std::fs::read(&config.server.tls.key)
        .with_context(|| format!("reading TLS key '{}'", config.server.tls.key))?;

    let cert_chain: Vec<_> = certs(&mut BufReader::new(cert_bytes.as_slice()))
        .collect::<Result<_, _>>()
        .context("parsing TLS cert PEM")?;

    let key = private_key(&mut BufReader::new(key_bytes.as_slice()))
        .context("parsing TLS key PEM")?
        .context("no private key found in key file")?;

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .context("building TLS ServerConfig")?;

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

fn parse_config_flag() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--config" && i + 1 < args.len() {
            return PathBuf::from(&args[i + 1]);
        }
        if let Some(path) = args[i].strip_prefix("--config=") {
            return PathBuf::from(path);
        }
        i += 1;
    }
    PathBuf::from("uplink.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use uplink_config::{BasicAuthConfig, ServerConfig, TlsConfig};

    // password = "testpass", cost = 4 (min cost for fast tests)
    const TEST_BCRYPT: &str = "$2b$04$G1zE/xiQeRXdPalTbG/Quu55uU6n5a9ICljrYbTpceT/qklM.0yay";
    const TEST_USER: &str = "testuser";
    const TEST_PASS: &str = "testpass";

    fn test_state() -> AppState {
        AppState {
            config: Arc::new(Config {
                server: ServerConfig {
                    bind: "127.0.0.1:0".to_string(),
                    tls: TlsConfig {
                        cert: String::new(),
                        key: String::new(),
                    },
                    basic_auth: BasicAuthConfig {
                        user: TEST_USER.to_string(),
                        password_bcrypt: TEST_BCRYPT.to_string(),
                    },
                },
            }),
        }
    }

    fn basic_auth_header(user: &str, pass: &str) -> String {
        format!("Basic {}", BASE64.encode(format!("{}:{}", user, pass)))
    }

    #[tokio::test]
    async fn unauthenticated_request_returns_401() {
        let app = build_app(test_state());
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticated_root_returns_200() {
        let app = build_app(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::AUTHORIZATION, basic_auth_header(TEST_USER, TEST_PASS))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn static_assets_are_served() {
        // Fetch index.html to find an embedded asset path.
        // This test only runs meaningfully when the SPA is embedded
        // (i.e. in the full nix build); in a plain cargo test with an
        // empty static/ dir the test is a no-op.
        let Some(index_file) = StaticFiles::get("index.html") else {
            return;
        };
        let index_html = String::from_utf8(index_file.data.into_owned()).unwrap();

        // Vite emits <script ... src="/assets/index-HASH.js"> in index.html.
        let Some(asset_path) = index_html
            .split('"')
            .find(|s| s.starts_with("/assets/") && s.ends_with(".js"))
        else {
            return;
        };
        let asset_path = asset_path.to_owned();

        let app = build_app(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(&asset_path)
                    .header(header::AUTHORIZATION, basic_auth_header(TEST_USER, TEST_PASS))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("Content-Type header missing")
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("javascript"),
            "expected javascript content-type, got: {content_type}"
        );

        // Consume body to verify it's non-empty.
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        let app = build_app(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/does-not-exist.txt")
                    .header(header::AUTHORIZATION, basic_auth_header(TEST_USER, TEST_PASS))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
