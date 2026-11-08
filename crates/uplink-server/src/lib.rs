use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::{
    extract::{ws::WebSocketUpgrade, Request, State},
    http::{header, HeaderMap, StatusCode, Uri},
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
use sha2::Digest;
use std::{
    io::BufReader,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tracing::info;
use uplink_config::{
    AccountConfig, BasicAuthConfig, Config, ConfigChanged, ConfigSection, GmailAccountConfig,
    MatrixAccountConfig, Watcher,
};
use uplink_email::{gmail::sync::Sync, query::ConfiguredAccount, service::Mail};
use uplink_matrix::service::Matrix;
use uplink_protocol::{ClientRequest, ErrorCode, ServerError, ServerResponse, TxLogId};
use uplink_realtime::{Backend, Hub, Replay};
use uplink_storage::seal::MasterKey;
use uplink_todos::service::Todos;
use uplink_tx_log::{Dispatcher, LogReader, Registry, UndoError};

/// How far a session may lag the live stream before it is disconnected.
const HUB_CAPACITY: usize = 4096;
/// A reconnecting client further behind than this reloads instead.
const MAX_REPLAY: usize = 4096;
/// The most history entries one request may ask for.
const MAX_HISTORY: u32 = 500;

#[derive(Embed)]
#[folder = "static/"]
struct StaticFiles;

#[derive(Clone)]
struct AppState {
    basic_auth: Arc<ArcSwap<BasicAuthConfig>>,
    services: Services,
}

/// Runs the server for the config at `config_path` until it fails.
pub async fn serve(config_path: PathBuf) -> Result<()> {
    let config = Config::load(&config_path)?;
    let services = open(&config, Registry::new())?;
    start_syncs(&config, &services)?;

    let bind: SocketAddr = config
        .server
        .bind
        .parse()
        .with_context(|| format!("parsing bind address '{}'", config.server.bind))?;

    let listener = TcpListener::bind(&bind).await?;
    info!("listening on https://{}", bind);

    serve_with_listener(config, config_path, listener, services).await
}

/// Starts the sync task of every configured account.
pub fn start_syncs(config: &Config, services: &Services) -> Result<()> {
    let mut gmail: Vec<&GmailAccountConfig> = vec![];
    let mut matrix: Vec<&MatrixAccountConfig> = vec![];
    for account in &config.accounts {
        match account {
            AccountConfig::Gmail(account) => gmail.push(account),
            AccountConfig::M365(m365) => {
                tracing::warn!(account = m365.id, "M365 accounts are not supported yet")
            }
            AccountConfig::Matrix(account) => matrix.push(account),
        }
    }
    // The master key is needed only to unseal accounts' secrets.
    if gmail.is_empty() && matrix.is_empty() {
        return Ok(());
    }
    let key =
        Arc::new(MasterKey::load(config.master_key_file.as_ref().context(
            "master_key_file is required with Gmail and Matrix accounts",
        )?)?);
    let http = reqwest::Client::new();
    for account in gmail {
        let sync = Sync::new(
            account,
            key.clone(),
            http.clone(),
            services.dispatcher.clone(),
            &config.data.root,
        )?;
        info!(account = account.id, "starting Gmail sync");
        tokio::spawn(sync.run());
    }
    for account in matrix {
        let sync = uplink_matrix::sync::Sync::new(
            account,
            key.clone(),
            services.dispatcher.clone(),
            &config.data.root,
        );
        info!(account = account.id, "starting Matrix sync");
        tokio::spawn(sync.run());
    }
    Ok(())
}

/// What answers clients: the database's only writer, a reader for replay,
/// and the domains.
#[derive(Clone)]
pub struct Services {
    pub dispatcher: Dispatcher,
    pub reader: Arc<LogReader>,
    pub mail: Mail,
    pub matrix: Matrix,
    pub todos: Todos,
}

/// Opens (and migrates) the database under the configured data root, with
/// a dispatcher for the domains' op kinds plus those in `registry`.
pub fn open(config: &Config, mut registry: Registry) -> Result<Services> {
    uplink_email::mailbox::register(&mut registry);
    uplink_email::compose::register(&mut registry);
    uplink_matrix::register(&mut registry);
    uplink_todos::register(&mut registry);
    let root = &config.data.root;
    let dispatcher = Dispatcher::new(
        uplink_storage::open(root)?,
        registry,
        Arc::new(Hub::new(HUB_CAPACITY)),
    );
    let mail_accounts = config
        .accounts
        .iter()
        .filter_map(|account| match account {
            AccountConfig::Gmail(gmail) => Some(ConfiguredAccount {
                id: gmail.id.clone(),
                address: gmail.address.clone(),
            }),
            AccountConfig::M365(_) | AccountConfig::Matrix(_) => None,
        })
        .collect();
    let matrix_accounts = config
        .accounts
        .iter()
        .filter_map(|account| match account {
            AccountConfig::Matrix(matrix) => Some(uplink_matrix::service::ConfiguredAccount {
                id: matrix.id.clone(),
                mxid: matrix.mxid.clone(),
            }),
            AccountConfig::Gmail(_) | AccountConfig::M365(_) => None,
        })
        .collect();
    Ok(Services {
        todos: Todos::new(dispatcher.clone(), uplink_storage::open_reader(root)?),
        matrix: Matrix::new(
            dispatcher.clone(),
            uplink_storage::open_reader(root)?,
            matrix_accounts,
        ),
        mail: Mail::new(
            dispatcher.clone(),
            uplink_storage::open_reader(root)?,
            root,
            mail_accounts,
            Duration::from_secs(config.send_delay_secs.into()),
        ),
        reader: Arc::new(LogReader::new(
            uplink_storage::open_reader(root)?,
            MAX_REPLAY,
        )),
        dispatcher,
    })
}

/// Start accepting connections on `listener` using `config`.
/// The `config_path` is watched for changes; cert/key files named in the
/// initial TLS config are also watched.
pub async fn serve_with_listener(
    config: Config,
    config_path: PathBuf,
    listener: TcpListener,
    services: Services,
) -> Result<()> {
    let tls_server_config =
        build_tls_server_config_from_paths(&config.server.tls.cert, &config.server.tls.key)?;

    let tls_holder: Arc<ArcSwap<ServerConfig>> = Arc::new(ArcSwap::from(tls_server_config));

    let basic_auth: Arc<ArcSwap<BasicAuthConfig>> =
        Arc::new(ArcSwap::from(Arc::new(config.server.basic_auth.clone())));

    let state = AppState {
        basic_auth: basic_auth.clone(),
        services,
    };

    let (tx, rx) = tokio::sync::broadcast::channel::<ConfigChanged>(16);

    let _watcher = Watcher::new(config_path.clone(), tx).context("starting config watcher")?;

    tokio::spawn(config_reload_task(
        rx,
        config_path,
        tls_holder.clone(),
        basic_auth.clone(),
    ));

    let app = build_app(state);

    loop {
        let (stream, peer_addr) = listener.accept().await?;

        // Load the current TLS config for this connection; in-flight
        // connections keep their own Arc<ServerConfig> reference.
        let current_sc = tls_holder.load_full();
        let acceptor = TlsAcceptor::from(current_sc);
        let tower_service = app.clone();

        tokio::spawn(async move {
            match acceptor.accept(stream).await {
                Ok(tls_stream) => {
                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let hyper_service =
                        hyper::service::service_fn(move |req: Request<Incoming>| {
                            tower_service.clone().call(req)
                        });
                    let result = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection_with_upgrades(io, hyper_service)
                    .await;
                    if let Err(e) = result {
                        tracing::debug!("connection error from {}: {}", peer_addr, e);
                    }
                }
                Err(e) => {
                    tracing::debug!("TLS handshake error from {}: {}", peer_addr, e)
                }
            }
        });
    }
}

async fn config_reload_task(
    mut rx: tokio::sync::broadcast::Receiver<ConfigChanged>,
    config_path: PathBuf,
    tls_holder: Arc<ArcSwap<ServerConfig>>,
    basic_auth: Arc<ArcSwap<BasicAuthConfig>>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                handle_config_changed(event, &config_path, &tls_holder, &basic_auth).await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("config: dropped {n} reload events (lagged)");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn handle_config_changed(
    event: ConfigChanged,
    config_path: &Path,
    tls_holder: &Arc<ArcSwap<ServerConfig>>,
    basic_auth: &Arc<ArcSwap<BasicAuthConfig>>,
) {
    match event.section {
        ConfigSection::Tls => match Config::load(config_path) {
            Ok(cfg) => {
                let cert_pem = match std::fs::read(&cfg.server.tls.cert) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!("config: tls reload failed reading cert: {e:#}");
                        return;
                    }
                };
                let key_pem = match std::fs::read(&cfg.server.tls.key) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!("config: tls reload failed reading key: {e:#}");
                        return;
                    }
                };
                let fp = pem_fingerprint(&cert_pem);
                match build_tls_server_config(&cert_pem, &key_pem) {
                    Ok(sc) => {
                        tls_holder.store(sc);
                        info!("config: tls cert reloaded ({fp})");
                    }
                    Err(e) => {
                        tracing::error!("config: tls reload failed, keeping old cert: {e:#}");
                    }
                }
            }
            Err(e) => {
                tracing::error!("config: could not read config for tls reload: {e:#}");
            }
        },
        ConfigSection::BasicAuth => match Config::load(config_path) {
            Ok(cfg) => {
                basic_auth.store(Arc::new(cfg.server.basic_auth));
                info!("config: basic_auth reloaded");
            }
            Err(e) => {
                tracing::error!("config: basic_auth reload failed, keeping old config: {e:#}");
            }
        },
        ConfigSection::Accounts => {
            tracing::warn!("config: accounts changed — restart required (ignored)");
        }
        ConfigSection::Bind => {
            tracing::warn!("config: bind changed — restart required (ignored)");
        }
        ConfigSection::Data => {
            tracing::warn!("config: data changed — restart required (ignored)");
        }
        ConfigSection::SendDelay => {
            tracing::warn!("config: send_delay_secs changed — restart required (ignored)");
        }
    }
}

/// Compute a short hex SHA-256 fingerprint of the first DER cert in a PEM blob.
fn pem_fingerprint(cert_pem: &[u8]) -> String {
    let mut reader = BufReader::new(cert_pem);
    let first = certs(&mut reader).next();
    match first {
        Some(Ok(der)) => {
            let hash = sha2::Sha256::digest(&*der);
            hash.iter().map(|b| format!("{b:02x}")).collect()
        }
        _ => "unknown".to_string(),
    }
}

fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(ws_handler))
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

async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let offered = headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| p.trim() == uplink_realtime::PROTOCOL);
    if !offered {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "WebSocket subprotocol {} required\n",
                uplink_realtime::PROTOCOL
            ),
        )
            .into_response();
    }
    ws.protocols([uplink_realtime::PROTOCOL])
        .on_upgrade(move |socket| async move {
            uplink_realtime::run_session(socket, state.services.dispatcher.hub(), &state.services)
                .await
        })
}

impl Backend for Services {
    async fn handle(&self, request: ClientRequest) -> Result<ServerResponse, ServerError> {
        match request {
            ClientRequest::Undo { tx_log_id } => self
                .dispatcher
                .undo(tx_log_id)
                .await
                .map(|id| ServerResponse::Undone { tx_log_id: id })
                .map_err(undo_error),
            ClientRequest::ListHistory { before, limit } => {
                if !(1..=MAX_HISTORY).contains(&limit) {
                    return Err(ServerError {
                        code: ErrorCode::Invalid,
                        message: format!("limit must be between 1 and {MAX_HISTORY}"),
                    });
                }
                let (as_of, entries, more) = self
                    .reader
                    .history(before, limit)
                    .await
                    .map_err(uplink_tx_log::answer::internal)?;
                Ok(ServerResponse::History {
                    as_of,
                    entries,
                    more,
                })
            }
            ClientRequest::ListAccounts => self.mail.list_accounts().await,
            ClientRequest::ListEntries { list, after, limit } => {
                self.mail.list_entries(list, after, limit).await
            }
            ClientRequest::Search { query, limit } => self.mail.search(query, limit).await,
            ClientRequest::ListMessageEntries { message } => {
                self.mail.list_message_entries(message).await
            }
            ClientRequest::LoadMessage { message } => self.mail.load_message(message).await,
            ClientRequest::ModifyLabels {
                entries,
                add,
                remove,
            } => self.mail.modify_labels(entries, add, remove).await,
            ClientRequest::ListDrafts => self.mail.list_drafts().await,
            ClientRequest::CreateDraft {
                id,
                account,
                content,
                in_reply_to,
            } => {
                self.mail
                    .create_draft(id, account, content, in_reply_to)
                    .await
            }
            ClientRequest::EditDraft {
                draft,
                revision,
                splice,
            } => self.mail.edit_draft(draft, revision, splice).await,
            ClientRequest::DeleteDraft { draft } => self.mail.delete_draft(draft).await,
            ClientRequest::SendDraft { draft } => self.mail.send_draft(draft).await,
            ClientRequest::CancelSend { item } => self.mail.cancel_send(item).await,
            ClientRequest::ListOutbox => self.mail.list_outbox().await,
            ClientRequest::ListRooms => self.matrix.list_rooms().await,
            ClientRequest::ListTimeline { room, page, limit } => {
                self.matrix.list_timeline(room, page, limit).await
            }
            ClientRequest::ListMatrixOutbox => self.matrix.list_outbox().await,
            ClientRequest::SendMatrixMessage { room, body } => {
                self.matrix.send_message(room, body).await
            }
            ClientRequest::CancelMatrixSend { item } => self.matrix.cancel_send(item).await,
            ClientRequest::MarkRoomRead { room, event } => self.matrix.mark_read(room, event).await,
            ClientRequest::ChangeMembership { room, change } => {
                self.matrix.change_membership(room, change).await
            }
            ClientRequest::ListTodos => self.todos.list().await,
            ClientRequest::CreateTodo {
                title,
                notes,
                due,
                message,
            } => self.todos.create(title, notes, due, message).await,
            ClientRequest::EditTodo { todo, edit } => self.todos.edit(todo, edit).await,
            ClientRequest::DeleteTodo { todo } => self.todos.delete(todo).await,
        }
    }

    async fn replay(&self, since: Option<TxLogId>) -> Result<Replay> {
        self.reader.replay(since).await
    }
}

fn undo_error(e: UndoError) -> ServerError {
    let code = match &e {
        UndoError::NotFound(_) => ErrorCode::NotFound,
        UndoError::NotUndoable(_) => ErrorCode::NotUndoable,
        UndoError::AlreadyUndone { .. } => ErrorCode::AlreadyUndone,
        UndoError::Conflict { .. } => ErrorCode::Conflict,
        UndoError::Failed(e) => {
            tracing::error!("undo failed: {e:#}");
            ErrorCode::Internal
        }
    };
    ServerError {
        code,
        message: format!("{e:#}"),
    }
}

/// Serves the SPA's files; any other path outside `/assets/` and `/api/`
/// is one of the SPA's routes, so it gets `index.html`.
async fn static_file_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if let Some(file) = StaticFiles::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            [(header::CONTENT_TYPE, mime.as_ref().to_owned())],
            file.data.into_owned(),
        )
            .into_response();
    }
    if path.starts_with("assets/") || path.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    index_handler().await.into_response()
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

    let ba = state.basic_auth.load();
    let expected_user = ba.user.as_bytes();
    let user_match = subtle::ConstantTimeEq::ct_eq(username.as_bytes(), expected_user).unwrap_u8();

    if user_match != 1 {
        return unauthorized();
    }

    match bcrypt::verify(password, &ba.password_bcrypt) {
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

/// Load cert+key PEM files and build a `rustls::ServerConfig`.
pub fn build_tls_server_config_from_paths(
    cert_path: &str,
    key_path: &str,
) -> Result<Arc<ServerConfig>> {
    let cert_bytes =
        std::fs::read(cert_path).with_context(|| format!("reading TLS cert '{cert_path}'"))?;
    let key_bytes =
        std::fs::read(key_path).with_context(|| format!("reading TLS key '{key_path}'"))?;

    build_tls_server_config(&cert_bytes, &key_bytes)
}

/// Build a `rustls::ServerConfig` from PEM bytes.
pub fn build_tls_server_config(cert_pem: &[u8], key_pem: &[u8]) -> Result<Arc<ServerConfig>> {
    let cert_chain: Vec<_> = certs(&mut BufReader::new(cert_pem))
        .collect::<Result<_, _>>()
        .context("parsing TLS cert PEM")?;

    let key = private_key(&mut BufReader::new(key_pem))
        .context("parsing TLS key PEM")?
        .context("no private key found in key file")?;

    let sc = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .context("building TLS ServerConfig")?;

    Ok(Arc::new(sc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    // password = "testpass", cost = 4 (min cost for fast tests)
    const TEST_BCRYPT: &str = "$2b$04$G1zE/xiQeRXdPalTbG/Quu55uU6n5a9ICljrYbTpceT/qklM.0yay";
    const TEST_USER: &str = "testuser";
    const TEST_PASS: &str = "testpass";

    /// These tests don't touch the database; they share one.
    static SERVICES: std::sync::LazyLock<(tempfile::TempDir, Services)> =
        std::sync::LazyLock::new(|| {
            let dir = tempfile::tempdir().unwrap();
            let conn = uplink_storage::open(dir.path()).unwrap();
            let reader = || uplink_storage::open_reader(dir.path()).unwrap();
            let dispatcher = Dispatcher::new(conn, Registry::new(), Arc::new(Hub::new(16)));
            let services = Services {
                mail: Mail::new(
                    dispatcher.clone(),
                    reader(),
                    dir.path(),
                    vec![],
                    Duration::from_secs(10),
                ),
                matrix: Matrix::new(dispatcher.clone(), reader(), vec![]),
                todos: Todos::new(dispatcher.clone(), reader()),
                reader: Arc::new(LogReader::new(reader(), 16)),
                dispatcher,
            };
            (dir, services)
        });

    fn test_state() -> AppState {
        AppState {
            basic_auth: Arc::new(ArcSwap::from(Arc::new(BasicAuthConfig {
                user: TEST_USER.to_string(),
                password_bcrypt: TEST_BCRYPT.to_string(),
            }))),
            services: SERVICES.1.clone(),
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
                    .header(
                        header::AUTHORIZATION,
                        basic_auth_header(TEST_USER, TEST_PASS),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn static_assets_are_served() {
        let Some(index_file) = StaticFiles::get("index.html") else {
            return;
        };
        let index_html = String::from_utf8(index_file.data.into_owned()).unwrap();

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
                    .header(
                        header::AUTHORIZATION,
                        basic_auth_header(TEST_USER, TEST_PASS),
                    )
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

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!body.is_empty());
    }

    async fn get_status(path: &str) -> StatusCode {
        build_app(test_state())
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(
                        header::AUTHORIZATION,
                        basic_auth_header(TEST_USER, TEST_PASS),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn spa_routes_load_the_app() {
        assert_eq!(get_status("/mail/work/INBOX").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_assets_and_api_paths_return_404() {
        assert_eq!(
            get_status("/assets/missing.js").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(get_status("/api/missing").await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn basic_auth_hot_reload() {
        // Verify that swapping the ArcSwap<BasicAuthConfig> takes effect
        // immediately on the next request.
        let state = test_state();
        let app = build_app(state.clone());

        // Old credentials accepted.
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(
                        header::AUTHORIZATION,
                        basic_auth_header(TEST_USER, TEST_PASS),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // Swap to new credentials.
        const NEW_USER: &str = "bob";
        const NEW_PASS: &str = "newpass";
        let new_bcrypt = bcrypt::hash(NEW_PASS, 4).unwrap();
        state.basic_auth.store(Arc::new(BasicAuthConfig {
            user: NEW_USER.into(),
            password_bcrypt: new_bcrypt,
        }));

        // Old credentials rejected.
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(
                        header::AUTHORIZATION,
                        basic_auth_header(TEST_USER, TEST_PASS),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        // New credentials accepted.
        let r = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::AUTHORIZATION, basic_auth_header(NEW_USER, NEW_PASS))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
}
