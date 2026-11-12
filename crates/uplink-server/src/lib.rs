use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::{
    body::Bytes,
    extract::{self, ws::WebSocketUpgrade, DefaultBodyLimit, Request, State},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use hyper::body::Incoming;
use outbox::Outbox;
use rust_embed::Embed;
use rustls::ServerConfig;
use rustls_pemfile::{certs, private_key};
use sha2::Digest;
use std::{
    io::BufReader,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tracing::info;
use uplink_calendar::service::Calendar;
use uplink_config::{
    AccountConfig, BasicAuthConfig, Config, ConfigChanged, ConfigSection, Watcher,
};
use uplink_email::{gmail, imap, m365, pop3, query::ConfiguredAccount, service::Mail};
use uplink_matrix::service::Matrix;
use uplink_protocol::{
    mail::{ConversationAction, ConversationSet, MailProvider},
    ClientRequest, ErrorCode, Notice, ServerError, ServerResponse, TxLogId,
};
use uplink_realtime::{Answer, Backend, Hub, Replay};
use uplink_storage::seal::MasterKey;
use uplink_todos::service::Todos;
use uplink_tx_log::{Dispatcher, LogReader, Registry, UndoError};

mod badges;
mod items;
mod outbox;
mod remote_image;
mod seen;
use remote_image::RemoteImages;

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
    gate: Gate,
    services: Services,
    remote_images: RemoteImages,
}

/// Basic auth's check. bcrypt takes about 200 ms by design, and a browser
/// sends the password with every request: it is checked by bcrypt once,
/// then by the digest of the password it accepted, which a change of the
/// configured password voids.
#[derive(Clone)]
struct Gate {
    config: Arc<ArcSwap<BasicAuthConfig>>,
    accepted: Arc<Mutex<Option<Accepted>>>,
}

struct Accepted {
    /// The configured hash it was checked against.
    password_bcrypt: String,
    digest: [u8; 32],
}

impl Gate {
    fn new(config: Arc<ArcSwap<BasicAuthConfig>>) -> Gate {
        Gate {
            config,
            accepted: Arc::new(Mutex::new(None)),
        }
    }

    async fn admits(&self, user: &str, password: &str) -> bool {
        let config = self.config.load_full();
        if subtle::ConstantTimeEq::ct_eq(user.as_bytes(), config.user.as_bytes()).unwrap_u8() != 1 {
            return false;
        }
        let digest: [u8; 32] = sha2::Sha256::digest(password.as_bytes()).into();
        let known = self.accepted.lock().unwrap().as_ref().is_some_and(|a| {
            a.password_bcrypt == config.password_bcrypt
                && bool::from(subtle::ConstantTimeEq::ct_eq(&a.digest[..], &digest[..]))
        });
        if known {
            return true;
        }
        let (password, hash) = (password.to_owned(), config.password_bcrypt.clone());
        // Off the async threads, which it would hold up.
        let verified = tokio::task::spawn_blocking(move || bcrypt::verify(password, &hash))
            .await
            .expect("bcrypt does not panic");
        if !matches!(verified, Ok(true)) {
            return false;
        }
        *self.accepted.lock().unwrap() = Some(Accepted {
            password_bcrypt: config.password_bcrypt.clone(),
            digest,
        });
        true
    }
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
    let scheme = if config.server.tls.is_some() {
        "https"
    } else {
        "http"
    };
    info!("listening on {scheme}://{bind}");

    serve_with_listener(config, config_path, listener, services).await
}

/// Starts the sync task of every configured account.
pub fn start_syncs(config: &Config, services: &Services) -> Result<()> {
    // The master key is needed only to unseal accounts' secrets.
    if config.accounts.is_empty() {
        return Ok(());
    }
    let key = Arc::new(MasterKey::open(&config.master_key_file())?);
    let http = reqwest::Client::new();
    let dispatcher = &services.dispatcher;
    let root = &config.data_dir;
    for account in &config.accounts {
        match account {
            AccountConfig::Gmail(account) => {
                let sync = gmail::sync::task(
                    account,
                    key.clone(),
                    http.clone(),
                    dispatcher.clone(),
                    root,
                )?;
                info!(account = account.id, "starting Gmail sync");
                tokio::spawn(sync.run());
            }
            AccountConfig::M365(account) => {
                // One Graph, so that both syncs refresh one token.
                let graph = m365::api::Graph::of(account, key.clone(), http.clone());
                let calendar =
                    uplink_calendar::sync::task(account, graph.clone(), dispatcher.clone(), root)?;
                let sync = m365::sync::task(account, graph, dispatcher.clone(), root)?;
                info!(account = account.id, "starting Microsoft 365 sync");
                tokio::spawn(sync.run());
                tokio::spawn(calendar.run());
            }
            AccountConfig::Imap(account) => {
                let sync =
                    imap::sync::task(account, key.clone(), http.clone(), dispatcher.clone(), root)?;
                info!(account = account.id, "starting IMAP sync");
                tokio::spawn(sync.run());
            }
            AccountConfig::Pop3(account) => {
                let sync =
                    pop3::sync::task(account, key.clone(), http.clone(), dispatcher.clone(), root)?;
                info!(account = account.id, "starting POP3 sync");
                tokio::spawn(sync.run());
            }
            AccountConfig::Matrix(account) => {
                let sync = uplink_matrix::sync::Sync::new(
                    account,
                    services.matrix.clients(),
                    key.clone(),
                    dispatcher.clone(),
                    root,
                );
                info!(account = account.id, "starting Matrix sync");
                tokio::spawn(sync.run());
            }
        }
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
    pub outbox: Outbox,
    pub items: items::Items,
    pub todos: Todos,
    pub calendar: Calendar,
}

/// Opens (and migrates) the database under the configured data root, with
/// a dispatcher for the domains' op kinds plus those in `registry`.
pub fn open(config: &Config, mut registry: Registry) -> Result<Services> {
    uplink_email::mailbox::register(&mut registry);
    uplink_email::compose::register(&mut registry);
    uplink_email::image_rules::register(&mut registry);
    uplink_matrix::register(&mut registry);
    uplink_todos::register(&mut registry);
    seen::register(&mut registry);
    let root = &config.data_dir;
    let mut writer = uplink_storage::open(root)?;
    uplink_email::mailbox::index_attachments(
        &mut writer,
        &uplink_storage::store::Store::mail(root),
    )?;
    let dispatcher = Dispatcher::new(writer, registry, Arc::new(Hub::new(HUB_CAPACITY)));
    let mail_accounts: Vec<ConfiguredAccount> = config
        .accounts
        .iter()
        .filter_map(|account| match account {
            AccountConfig::Gmail(gmail) => Some(ConfiguredAccount {
                id: gmail.id.clone(),
                address: gmail.address.clone(),
                provider: MailProvider::Gmail,
            }),
            AccountConfig::M365(m365) => Some(ConfiguredAccount {
                id: m365.id.clone(),
                address: m365.address.clone(),
                provider: MailProvider::M365,
            }),
            AccountConfig::Imap(imap) => Some(ConfiguredAccount {
                id: imap.id.clone(),
                address: imap.address.clone(),
                provider: MailProvider::Imap,
            }),
            AccountConfig::Pop3(pop3) => Some(ConfiguredAccount {
                id: pop3.id.clone(),
                address: pop3.address.clone(),
                provider: MailProvider::Pop3,
            }),
            AccountConfig::Matrix(_) => None,
        })
        .collect();
    let matrix_accounts: Vec<uplink_matrix::service::ConfiguredAccount> = config
        .accounts
        .iter()
        .filter_map(|account| match account {
            AccountConfig::Matrix(matrix) => Some(uplink_matrix::service::ConfiguredAccount {
                id: matrix.id.clone(),
                mxid: matrix.mxid.clone(),
            }),
            AccountConfig::Gmail(_)
            | AccountConfig::M365(_)
            | AccountConfig::Imap(_)
            | AccountConfig::Pop3(_) => None,
        })
        .collect();
    Ok(Services {
        outbox: Outbox::new(uplink_storage::open_reader(root)?),
        items: items::Items::new(
            uplink_storage::open_reader(root)?,
            mail_accounts.clone(),
            matrix_accounts.clone(),
            config.home.clone(),
        ),
        todos: Todos::new(dispatcher.clone(), uplink_storage::open_reader(root)?),
        calendar: Calendar::new(uplink_storage::open_reader(root)?),
        matrix: Matrix::new(
            dispatcher.clone(),
            uplink_storage::open_reader(root)?,
            matrix_accounts,
            uplink_storage::store::Store::blobs(root),
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

/// Start accepting connections on `listener` using `config`: over TLS if
/// it has a certificate, else plain HTTP. The `config_path` is watched for
/// changes; cert/key files named in the initial TLS config are also
/// watched.
pub async fn serve_with_listener(
    config: Config,
    config_path: PathBuf,
    listener: TcpListener,
    services: Services,
) -> Result<()> {
    let tls_holder: Option<Arc<ArcSwap<ServerConfig>>> = config
        .server
        .tls
        .as_ref()
        .map(|tls| build_tls_server_config_from_paths(&tls.cert, &tls.key))
        .transpose()?
        .map(|tls| Arc::new(ArcSwap::from(tls)));

    let basic_auth: Arc<ArcSwap<BasicAuthConfig>> =
        Arc::new(ArcSwap::from(Arc::new(config.server.basic_auth.clone())));

    let state = AppState {
        gate: Gate::new(basic_auth.clone()),
        services,
        remote_images: RemoteImages::new(remote_image::public),
    };

    let (tx, rx) = tokio::sync::broadcast::channel::<ConfigChanged>(16);

    let _watcher = Watcher::new(config_path.clone(), tx).context("starting config watcher")?;

    tokio::spawn(config_reload_task(
        rx,
        config_path,
        tls_holder.clone(),
        basic_auth.clone(),
        state.services.clone(),
    ));

    // Set before any session is served, which is sent them at once.
    let hub = state.services.dispatcher.hub().clone();
    let badges = state.services.items.badges().await;
    hub.set_badges(badges.map_err(|e| anyhow::anyhow!("reading the badges: {}", e.message))?);
    tokio::spawn(badges::keep(state.services.items.clone(), hub));

    let app = build_app(state);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let app = app.clone();
        let Some(tls_holder) = &tls_holder else {
            tokio::spawn(serve_connection(stream, app, peer_addr));
            continue;
        };
        // Load the current TLS config for this connection; in-flight
        // connections keep their own Arc<ServerConfig> reference.
        let acceptor = TlsAcceptor::from(tls_holder.load_full());
        tokio::spawn(async move {
            match acceptor.accept(stream).await {
                Ok(tls_stream) => serve_connection(tls_stream, app, peer_addr).await,
                Err(e) => tracing::debug!("TLS handshake error from {}: {}", peer_addr, e),
            }
        });
    }
}

/// Serves HTTP on one connection, until it closes.
async fn serve_connection<S>(stream: S, app: Router, peer_addr: SocketAddr)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = hyper_util::rt::TokioIo::new(stream);
    let hyper_service =
        hyper::service::service_fn(move |req: Request<Incoming>| app.clone().call(req));
    let result = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection_with_upgrades(io, hyper_service)
        .await;
    if let Err(e) = result {
        tracing::debug!("connection error from {}: {}", peer_addr, e);
    }
}

async fn config_reload_task(
    mut rx: tokio::sync::broadcast::Receiver<ConfigChanged>,
    config_path: PathBuf,
    tls_holder: Option<Arc<ArcSwap<ServerConfig>>>,
    basic_auth: Arc<ArcSwap<BasicAuthConfig>>,
    services: Services,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                handle_config_changed(
                    event,
                    &config_path,
                    tls_holder.as_ref(),
                    &basic_auth,
                    &services,
                )
                .await;
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
    tls_holder: Option<&Arc<ArcSwap<ServerConfig>>>,
    basic_auth: &Arc<ArcSwap<BasicAuthConfig>>,
    services: &Services,
) {
    match event.section {
        ConfigSection::Tls => match Config::load(config_path) {
            Ok(cfg) => {
                // A certificate is swapped live; turning TLS on or off is
                // not.
                let (Some(tls_holder), Some(tls)) = (tls_holder, &cfg.server.tls) else {
                    tracing::warn!("config: tls turned on or off — restart required (ignored)");
                    return;
                };
                let cert_pem = match std::fs::read(&tls.cert) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!("config: tls reload failed reading cert: {e:#}");
                        return;
                    }
                };
                let key_pem = match std::fs::read(&tls.key) {
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
        ConfigSection::Home => match Config::load(config_path) {
            Ok(cfg) => {
                services.items.configure_home(cfg.home);
                services.dispatcher.hub().notify(Notice::HomeChanged);
                info!("config: home reloaded");
            }
            Err(e) => {
                tracing::error!("config: home reload failed, keeping old config: {e:#}");
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
        .route(
            "/api/messages/:message/attachments/:index",
            get(attachment_handler),
        )
        .route(
            "/api/messages/:message/attachments/:index/blob",
            post(copy_attachment_handler),
        )
        .route("/api/entries/:entry/raw", get(raw_entry_handler))
        .route(
            "/api/rooms/:room/avatar/:server/:media",
            get(room_avatar_handler),
        )
        .route(
            "/api/rooms/:room/events/:event/media/:server/:media",
            get(|state, path| event_media_handler(state, path, false)),
        )
        .route(
            "/api/rooms/:room/events/:event/thumbnail/:server/:media",
            get(|state, path| event_media_handler(state, path, true)),
        )
        .route(
            "/api/rooms/:room/members/:user/avatar",
            get(member_avatar_handler),
        )
        .route(
            "/api/rooms/:room/events/:event/preview/:server/:media",
            get(preview_image_handler),
        )
        .route("/api/matrix/:account/room-keys", post(room_keys_handler))
        .route(
            &format!("{}/:host", uplink_email::remote::REMOTE_IMAGE_PATH),
            get(remote_image_handler),
        )
        .route(
            "/api/blobs",
            post(blob_handler).layer(DefaultBodyLimit::max(
                uplink_email::service::MAX_ATTACHMENTS
                    .try_into()
                    .expect("fits in usize"),
            )),
        )
        .fallback(static_file_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            basic_auth_middleware,
        ))
        .layer(middleware::from_fn(cross_site_middleware))
        .with_state(state)
}

/// Requests of other sites' pages, refused, before basic auth: the
/// browser would send the credentials it has for uplink's origin, so a
/// page elsewhere could otherwise open the WebSocket and act as the user,
/// post to the API, or probe it with `<img>` (SPEC §13.1). Going to
/// uplink, by a link to the app or a download, stays. As the browser
/// tells (`Sec-Fetch-*`, and a WebSocket's `Origin`); a request without
/// it is no browser's, or a browser's that predates it (Safari before
/// 16.4).
async fn cross_site_middleware(req: Request, next: Next) -> Response {
    if admitted(&req) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "Cross-site request refused\n").into_response()
    }
}

fn admitted(req: &Request) -> bool {
    let get = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok());
    if get("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket")) {
        // Chromium sends no `Sec-Fetch-*` with a WebSocket's handshake,
        // but every browser its `Origin`: the page's, which must be at
        // the authority asked for (as a proxy in front is to keep it).
        return match get("origin") {
            None => true,
            Some(origin) => origin.split_once("://").map(|(_, authority)| authority) == get("host"),
        };
    }
    match get("sec-fetch-site") {
        None | Some("same-origin" | "none") => true,
        Some(_) => {
            req.method() == Method::GET
                && get("sec-fetch-mode") == Some("navigate")
                && get("sec-fetch-dest") == Some("document")
        }
    }
}

/// The app's: its own scripts, styles and images only (and `data:` ones,
/// and inline styles, which mail has, its frame inheriting this), and
/// never framed by another page. Matrix HTML is in the app's own
/// document: were its sanitizing to miss, this would still refuse its
/// scripts.
const APP_CSP: &str = "default-src 'self'; img-src 'self' data:; \
    style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'";

/// A message's attachment, as a download: never shown by the browser in
/// uplink's origin, where an HTML one could act as the page.
async fn attachment_handler(
    State(state): State<AppState>,
    extract::Path((message, index)): extract::Path<(i64, u32)>,
) -> Response {
    match state
        .services
        .mail
        .attachment(uplink_protocol::mail::MessageId(message), index)
        .await
    {
        Ok((name, mime, content)) => {
            let name = name.unwrap_or_else(|| format!("attachment-{index}"));
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename*=UTF-8''{}", percent_encode(&name)),
                    ),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                    (header::CONTENT_SECURITY_POLICY, "sandbox".to_owned()),
                ],
                content,
            )
                .into_response()
        }
        Err(e) if e.code == ErrorCode::NotFound => {
            (StatusCode::NOT_FOUND, e.message).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.message).into_response(),
    }
}

/// A thumbnail of a room's avatar `mxc://server/media`. The path names the
/// avatar, so it is one image for good.
async fn room_avatar_handler(
    State(state): State<AppState>,
    extract::Path((room, server, media)): extract::Path<(i64, String, String)>,
) -> Response {
    match state
        .services
        .matrix
        .avatar(
            uplink_protocol::matrix::RoomId(room),
            format!("mxc://{server}/{media}"),
        )
        .await
    {
        Ok((mime, image)) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CONTENT_SECURITY_POLICY, "sandbox"),
                (
                    header::CACHE_CONTROL,
                    "private, max-age=31536000, immutable",
                ),
            ],
            image,
        )
            .into_response(),
        Err(e) if e.code == ErrorCode::NotFound => {
            (StatusCode::NOT_FOUND, e.message).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, e.message).into_response(),
    }
}

/// The media `mxc://server/media` that a room's event names, or a
/// thumbnail of it (SPEC §9.8). Media never change, so they are cached
/// for good. Sandboxed, and only images, video and audio are shown by
/// the browser; anything else is downloaded, as mail attachments are.
async fn event_media_handler(
    State(state): State<AppState>,
    extract::Path((room, event, server, media)): extract::Path<(i64, i64, String, String)>,
    thumbnail: bool,
) -> Response {
    let fetched = state
        .services
        .matrix
        .media(
            uplink_protocol::matrix::RoomId(room),
            uplink_protocol::matrix::RoomEventId(event),
            format!("mxc://{server}/{media}"),
            thumbnail,
        )
        .await;
    match fetched {
        Ok(media) => {
            let disposition = if media.inline { "inline" } else { "attachment" };
            let disposition = match &media.name {
                Some(name) => format!("{disposition}; filename*=UTF-8''{}", percent_encode(name)),
                None => disposition.to_owned(),
            };
            (
                [
                    (header::CONTENT_TYPE, media.mime),
                    (header::CONTENT_DISPOSITION, disposition),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                    (header::CONTENT_SECURITY_POLICY, "sandbox".to_owned()),
                    (
                        header::CACHE_CONTROL,
                        "private, max-age=31536000, immutable".to_owned(),
                    ),
                ],
                media.data,
            )
                .into_response()
        }
        Err(e) if e.code == ErrorCode::NotFound => {
            (StatusCode::NOT_FOUND, e.message).into_response()
        }
        Err(e) if e.code == ErrorCode::Invalid => {
            (StatusCode::BAD_REQUEST, e.message).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, e.message).into_response(),
    }
}

/// A thumbnail of a room member's avatar. Members change their avatars,
/// so it is cached for an hour only. A member without one is no error,
/// which browsers would log for each: it is No Content.
async fn member_avatar_handler(
    State(state): State<AppState>,
    extract::Path((room, user)): extract::Path<(i64, String)>,
) -> Response {
    match state
        .services
        .matrix
        .member_avatar(uplink_protocol::matrix::RoomId(room), user)
        .await
    {
        Ok(None) => (
            StatusCode::NO_CONTENT,
            [(header::CACHE_CONTROL, "private, max-age=3600")],
        )
            .into_response(),
        Ok(Some((mime, image))) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CONTENT_SECURITY_POLICY, "sandbox"),
                (header::CACHE_CONTROL, "private, max-age=3600"),
            ],
            image,
        )
            .into_response(),
        Err(e) if e.code == ErrorCode::NotFound => {
            (StatusCode::NOT_FOUND, e.message).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, e.message).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PreviewedUrl {
    /// As the message has it: it is looked for there.
    url: String,
}

/// A thumbnail of the image `mxc://server/media` of the preview of the
/// page `url` that a room's message links (SPEC §9.8).
async fn preview_image_handler(
    State(state): State<AppState>,
    extract::Path((room, event, server, media)): extract::Path<(i64, i64, String, String)>,
    extract::Query(PreviewedUrl { url }): extract::Query<PreviewedUrl>,
) -> Response {
    match state
        .services
        .matrix
        .preview_image(
            uplink_protocol::matrix::RoomId(room),
            uplink_protocol::matrix::RoomEventId(event),
            url,
            format!("mxc://{server}/{media}"),
        )
        .await
    {
        Ok((mime, image)) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CONTENT_SECURITY_POLICY, "sandbox"),
                (
                    header::CACHE_CONTROL,
                    "private, max-age=31536000, immutable",
                ),
            ],
            image,
        )
            .into_response(),
        Err(e) if e.code == ErrorCode::NotFound => {
            (StatusCode::NOT_FOUND, e.message).into_response()
        }
        Err(e) if e.code == ErrorCode::Invalid => {
            (StatusCode::BAD_REQUEST, e.message).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, e.message).into_response(),
    }
}

/// The account's room keys, exported encrypted with the passphrase the
/// body is (SPEC §9.4), as a download. A POST: it reads a secret.
async fn room_keys_handler(
    State(state): State<AppState>,
    extract::Path(account): extract::Path<String>,
    passphrase: String,
) -> Response {
    if passphrase.is_empty() {
        return (StatusCode::BAD_REQUEST, "the passphrase is empty").into_response();
    }
    match state
        .services
        .matrix
        .export_room_keys(&account, &passphrase)
        .await
    {
        Ok(export) => (
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"element-keys-{account}.txt\""),
                ),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                (header::CACHE_CONTROL, "no-store".to_owned()),
            ],
            export,
        )
            .into_response(),
        Err(e) if e.code == ErrorCode::NotFound => {
            (StatusCode::NOT_FOUND, e.message).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.message).into_response(),
    }
}

/// A message as its account has it, raw, for "Show original": text, which
/// the browser shows as is; sandboxed, as an attachment is.
async fn raw_entry_handler(
    State(state): State<AppState>,
    extract::Path(entry): extract::Path<i64>,
) -> Response {
    match state
        .services
        .mail
        .raw_entry(uplink_protocol::mail::EntryId(entry))
        .await
    {
        Ok(raw) => (
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CONTENT_SECURITY_POLICY, "sandbox"),
            ],
            raw,
        )
            .into_response(),
        Err(e) if e.code == ErrorCode::NotFound => {
            (StatusCode::NOT_FOUND, e.message).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.message).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct RemoteImage {
    url: reqwest::Url,
}

/// A remote image of a mail's, fetched by the server (SPEC §8.9): only
/// at the host its path names, which the frame's CSP lets through as
/// the user has them shown.
async fn remote_image_handler(
    State(state): State<AppState>,
    extract::Path(host): extract::Path<String>,
    extract::Query(RemoteImage { url }): extract::Query<RemoteImage>,
) -> Response {
    if uplink_email::remote::remote(url.as_str()).map(|(of, _)| of) != Some(host) {
        return (StatusCode::BAD_REQUEST, "Not an image of that host\n").into_response();
    }
    state.remote_images.get(url).await
}

/// Stores an uploaded file, to be attached to drafts (SPEC §7.4).
async fn blob_handler(State(state): State<AppState>, content: Bytes) -> Response {
    match state.services.mail.put_blob(content.into()).await {
        Ok(uploaded) => Json(uploaded).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.message).into_response(),
    }
}

/// Stores a message's attachment as a blob, for a draft forwarding it.
async fn copy_attachment_handler(
    State(state): State<AppState>,
    extract::Path((message, index)): extract::Path<(i64, u32)>,
) -> Response {
    match state
        .services
        .mail
        .copy_attachment(uplink_protocol::mail::MessageId(message), index)
        .await
    {
        Ok(uploaded) => Json(uploaded).into_response(),
        Err(e) if e.code == ErrorCode::NotFound => {
            (StatusCode::NOT_FOUND, e.message).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.message).into_response(),
    }
}

/// RFC 5987 encoding, for `filename*`: all but unreserved characters.
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn index_handler() -> impl IntoResponse {
    if let Some(file) = StaticFiles::get("index.html") {
        (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CONTENT_SECURITY_POLICY, APP_CSP),
            ],
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

impl Services {
    async fn history(
        &self,
        before: Option<TxLogId>,
        limit: u32,
    ) -> uplink_tx_log::answer::Response {
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
}

impl Services {
    /// A query's conversations are those its search lists.
    async fn act_on_conversations(
        &self,
        conversations: ConversationSet,
        action: ConversationAction,
        shown: bool,
    ) -> uplink_tx_log::answer::Response {
        let conversations = match conversations {
            ConversationSet::Listed { conversations } => conversations,
            ConversationSet::Matching { query } => self.items.matching(query).await?,
        };
        self.mail
            .act_on_conversations(conversations, action, shown)
            .await
    }
}

impl Backend for Services {
    async fn handle(&self, request: ClientRequest) -> Answer {
        // Those that wait on a homeserver return how they are answered:
        // their remote part, later (SPEC §5.6).
        let response = match request {
            ClientRequest::Undo { tx_log_id } => self
                .dispatcher
                .undo(tx_log_id)
                .await
                .map(|id| ServerResponse::Undone { tx_log_id: id })
                .map_err(undo_error),
            ClientRequest::ListHistory { before, limit } => self.history(before, limit).await,
            ClientRequest::ListAccounts => self.mail.list_accounts().await,
            ClientRequest::ListHome { limit } => self.items.home(limit).await,
            ClientRequest::ListItems {
                query,
                after,
                limit,
            } => self.items.list(query, after, limit).await,
            ClientRequest::ListEntries {
                query,
                after,
                limit,
            } => self.mail.list_entries(query, after, limit).await,
            ClientRequest::ListMessageEntries { message } => {
                self.mail.list_message_entries(message).await
            }
            ClientRequest::ListConversation { entry } => self.mail.list_conversation(entry).await,
            ClientRequest::LoadMessage { message } => self.mail.load_message(message).await,
            ClientRequest::AlwaysShowImages { rule, always } => {
                self.mail.always_show_images(rule, always).await
            }
            ClientRequest::ModifyLabels {
                entries,
                add,
                remove,
            } => self.mail.modify_labels(entries, add, remove).await,
            ClientRequest::ActOnConversations {
                conversations,
                action,
                shown,
            } => {
                self.act_on_conversations(conversations, action, shown)
                    .await
            }
            ClientRequest::CountConversations { query } => self.items.count(query).await,
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
            ClientRequest::AttachToDraft {
                draft,
                blob,
                name,
                mime,
            } => self.mail.attach_to_draft(draft, blob, name, mime).await,
            ClientRequest::DetachFromDraft { draft, attachment } => {
                self.mail.detach_from_draft(draft, attachment).await
            }
            ClientRequest::SendDraft { draft } => self.mail.send_draft(draft).await,
            ClientRequest::CancelSend { item } => self.mail.cancel_send(item).await,
            ClientRequest::KeepSent { item } => self.mail.keep_sent(item).await,
            ClientRequest::ListOutbox => self.outbox.list().await,
            ClientRequest::KeepObserved { account, op } => {
                self.mail.keep_observed(account, op).await
            }
            ClientRequest::ListRooms => self.matrix.list_rooms().await,
            ClientRequest::ListTimeline { room, page, limit } => {
                self.matrix.list_timeline(room, page, limit).await
            }
            ClientRequest::ListRoomEvents { room, events } => {
                self.matrix.list_room_events(room, events).await
            }
            ClientRequest::ListEdits { room, event } => self.matrix.list_edits(room, event).await,
            ClientRequest::ListThread { room, root } => self.matrix.list_thread(room, root).await,
            ClientRequest::ListPinned { room } => self.matrix.list_pinned(room).await,
            ClientRequest::SearchRoom {
                room,
                query,
                before,
                limit,
            } => self.matrix.search_room(room, query, before, limit).await,
            ClientRequest::PreviewUrl { room, event, url } => {
                return self.matrix.preview_url(room, event, url).await.into()
            }
            ClientRequest::SendMatrixMessage { room, content } => {
                self.matrix.send_message(room, content).await
            }
            ClientRequest::CancelMatrixSend { item } => self.matrix.cancel_send(item).await,
            ClientRequest::KeepMatrixSent { item } => self.matrix.keep_sent(item).await,
            ClientRequest::MarkSeen { place } => {
                self.dispatcher.answer(seen::MarkSeen { place }).await
            }
            ClientRequest::MarkRoomRead {
                room,
                event,
                shown,
                fully,
            } => self.matrix.mark_read(room, event, shown, fully).await,
            ClientRequest::MarkRoomFullyRead { room, event } => {
                self.matrix.mark_fully_read(room, event).await
            }
            ClientRequest::SetTyping { room, typing } => self.matrix.set_typing(room, typing).await,
            ClientRequest::ListReceipts { room } => self.matrix.list_receipts(room).await,
            ClientRequest::ListMembers { room } => {
                return self.matrix.list_members(room).await.into()
            }
            ClientRequest::CreateMatrixRoom { account, room } => {
                return self.matrix.create_room(&account, room).await.into()
            }
            ClientRequest::StartDirectChat { account, user } => {
                return self.matrix.start_direct_chat(&account, &user).await.into()
            }
            ClientRequest::JoinMatrixRoom { account, room } => {
                return self.matrix.join_room(&account, &room).await.into()
            }
            ClientRequest::ResolveRoomAlias { account, alias } => {
                return self.matrix.resolve_alias(&account, &alias).await.into()
            }
            ClientRequest::FillGap { room, gap } => {
                return self.matrix.fill_gap(room, gap).await.into()
            }
            ClientRequest::GetMatrixSecurity { account } => {
                return self.matrix.get_security(&account).await.into()
            }
            ClientRequest::VerifyMatrixDevice { account, device } => {
                return self
                    .matrix
                    .verify_device(&account, device.as_deref())
                    .await
                    .into()
            }
            ClientRequest::AnswerVerification {
                account,
                flow_id,
                answer,
            } => {
                return self
                    .matrix
                    .answer_verification(&account, &flow_id, answer)
                    .await
                    .into()
            }
            ClientRequest::RecoverMatrix { account, recovery } => {
                return self.matrix.recover(&account, &recovery).await.into()
            }
            ClientRequest::ImportRoomKeys {
                account,
                blob,
                passphrase,
            } => {
                return self
                    .matrix
                    .import_room_keys(&account, &blob, &passphrase)
                    .await
                    .into()
            }
            ClientRequest::ChangeMembership { room, change } => {
                self.matrix.change_membership(room, change).await
            }
            ClientRequest::OrderSpaces { changes } => self.matrix.order_spaces(changes).await,
            ClientRequest::ModifyRoomTags { rooms, add, remove } => {
                self.matrix.modify_room_tags(rooms, add, remove).await
            }
            ClientRequest::ListTodos => self.todos.list().await,
            ClientRequest::ListEvents { from, to } => self.calendar.list(from, to).await,
            ClientRequest::CreateTodo {
                title,
                notes,
                due,
                message,
            } => self.todos.create(title, notes, due, message).await,
            ClientRequest::EditTodo { todo, edit } => self.todos.edit(todo, edit).await,
            ClientRequest::DeleteTodo { todo } => self.todos.delete(todo).await,
        };
        Answer::Now(response)
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

    if state.gate.admits(username, password).await {
        next.run(req).await
    } else {
        unauthorized()
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
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<ServerConfig>> {
    let cert_bytes = std::fs::read(cert_path)
        .with_context(|| format!("reading TLS cert {}", cert_path.display()))?;
    let key_bytes = std::fs::read(key_path)
        .with_context(|| format!("reading TLS key {}", key_path.display()))?;

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
                matrix: Matrix::new(
                    dispatcher.clone(),
                    reader(),
                    vec![],
                    uplink_storage::store::Store::blobs(dir.path()),
                ),
                outbox: Outbox::new(reader()),
                items: items::Items::new(reader(), vec![], vec![], vec![]),
                todos: Todos::new(dispatcher.clone(), reader()),
                calendar: Calendar::new(reader()),
                reader: Arc::new(LogReader::new(reader(), 16)),
                dispatcher,
            };
            (dir, services)
        });

    fn test_state() -> AppState {
        AppState {
            gate: Gate::new(Arc::new(ArcSwap::from(Arc::new(BasicAuthConfig {
                user: TEST_USER.to_string(),
                password_bcrypt: TEST_BCRYPT.to_string(),
            })))),
            services: SERVICES.1.clone(),
            remote_images: RemoteImages::new(remote_image::public),
        }
    }

    fn basic_auth_header(user: &str, pass: &str) -> String {
        format!("Basic {}", BASE64.encode(format!("{}:{}", user, pass)))
    }

    /// A password accepted once is remembered, until the configured one
    /// changes: then it is checked against the new one.
    #[tokio::test]
    async fn a_changed_password_voids_the_one_accepted() {
        let state = test_state();
        let gate = &state.gate;
        assert!(gate.admits(TEST_USER, TEST_PASS).await);
        assert!(gate.admits(TEST_USER, TEST_PASS).await);
        assert!(!gate.admits(TEST_USER, "wrong").await);
        assert!(!gate.admits("mallory", TEST_PASS).await);
        gate.config.store(Arc::new(BasicAuthConfig {
            user: TEST_USER.into(),
            password_bcrypt: bcrypt::hash("new", 4).unwrap(),
        }));
        assert!(!gate.admits(TEST_USER, TEST_PASS).await);
        assert!(gate.admits(TEST_USER, "new").await);
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
    async fn other_sites_requests_are_refused() {
        let app = build_app(test_state());
        let refused = |request: &str, headers: &str| {
            let (method, uri) = request.split_once(' ').unwrap();
            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::HOST, "uplink.example")
                .header(
                    header::AUTHORIZATION,
                    basic_auth_header(TEST_USER, TEST_PASS),
                );
            for header in headers.split_whitespace() {
                let (name, value) = header.split_once('=').unwrap();
                request = request.header(name, value);
            }
            let app = app.clone();
            async move {
                let response = app.oneshot(request.body(Body::empty()).unwrap()).await;
                response.unwrap().status() == StatusCode::FORBIDDEN
            }
        };
        let navigate = "sec-fetch-mode=navigate sec-fetch-dest=document";
        let ws = "upgrade=websocket connection=upgrade sec-websocket-version=13";
        for (request, headers, expected) in [
            // The app's own, typed in, or of no browser.
            (
                "GET /",
                "sec-fetch-site=same-origin sec-fetch-mode=cors",
                false,
            ),
            ("GET /", &format!("sec-fetch-site=none {navigate}"), false),
            ("GET /", "", false),
            // A link from elsewhere.
            (
                "GET /",
                &format!("sec-fetch-site=cross-site {navigate}"),
                false,
            ),
            // But no fetch, image, frame or form of another site's, even a
            // sibling's.
            (
                "POST /api/blobs",
                "sec-fetch-site=cross-site sec-fetch-mode=no-cors",
                true,
            ),
            (
                "POST /api/blobs",
                "sec-fetch-site=same-site sec-fetch-mode=cors",
                true,
            ),
            (
                "GET /api/remote-image/x.example?url=https://x.example/",
                "sec-fetch-site=cross-site sec-fetch-mode=no-cors sec-fetch-dest=image",
                true,
            ),
            (
                "GET /",
                "sec-fetch-site=cross-site sec-fetch-mode=navigate sec-fetch-dest=iframe",
                true,
            ),
            (
                "POST /",
                &format!("sec-fetch-site=cross-site {navigate}"),
                true,
            ),
            // Nor WebSocket, told by its origin (Chromium's, which has no
            // `Sec-Fetch-*`), whatever else it says.
            (
                "GET /ws",
                &format!("{ws} origin=https://uplink.example"),
                false,
            ),
            ("GET /ws", ws, false),
            (
                "GET /ws",
                &format!("{ws} origin=https://evil.example"),
                true,
            ),
            (
                "GET /ws",
                &format!("{ws} origin=https://uplink.example.evil.example"),
                true,
            ),
            ("GET /ws", &format!("{ws} origin=null"), true),
            (
                "GET /ws",
                &format!("{ws} origin=https://evil.example sec-fetch-site=same-origin"),
                true,
            ),
        ] {
            assert_eq!(
                refused(request, headers).await,
                expected,
                "{request} {headers}"
            );
        }
    }

    #[tokio::test]
    async fn remote_images_are_fetched_only_at_the_host_named() {
        let app = build_app(test_state());
        for uri in [
            "/api/remote-image/a.example?url=https://b.example/x.gif",
            "/api/remote-image/a.example?url=https://a.example.b.example/x.gif",
            "/api/remote-image/a.example?url=ftp://a.example/x.gif",
            "/api/remote-pixel/a.example?url=https://a.example/x.gif",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(
                            header::AUTHORIZATION,
                            basic_auth_header(TEST_USER, TEST_PASS),
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                matches!(
                    response.status(),
                    StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
                ),
                "{uri}: {}",
                response.status()
            );
        }
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
    async fn raw_messages_need_auth_and_an_existing_entry() {
        let get = |auth: bool| {
            let mut request = Request::builder().uri("/api/entries/7/raw");
            if auth {
                request = request.header(
                    header::AUTHORIZATION,
                    basic_auth_header(TEST_USER, TEST_PASS),
                );
            }
            build_app(test_state()).oneshot(request.body(Body::empty()).unwrap())
        };
        assert_eq!(get(false).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(get(true).await.unwrap().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn attachments_need_auth_and_an_existing_message() {
        let get = |auth: bool| {
            let mut request = Request::builder().uri("/api/messages/7/attachments/0");
            if auth {
                request = request.header(
                    header::AUTHORIZATION,
                    basic_auth_header(TEST_USER, TEST_PASS),
                );
            }
            build_app(test_state()).oneshot(request.body(Body::empty()).unwrap())
        };
        assert_eq!(get(false).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(get(true).await.unwrap().status(), StatusCode::NOT_FOUND);
        let copy = build_app(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/messages/7/attachments/0/blob")
                    .header(
                        header::AUTHORIZATION,
                        basic_auth_header(TEST_USER, TEST_PASS),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(copy.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn blobs_are_uploaded_within_the_limit() {
        let post = |body: Vec<u8>| {
            build_app(test_state()).oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/blobs")
                    .header(
                        header::AUTHORIZATION,
                        basic_auth_header(TEST_USER, TEST_PASS),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
        };
        let response = post(b"hello".to_vec()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let uploaded: uplink_protocol::compose::UploadedBlob =
            serde_json::from_slice(&body).unwrap();
        assert_eq!(uploaded.size, 5);
        assert_eq!(
            uploaded.blob.0,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let too_big = vec![0; uplink_email::service::MAX_ATTACHMENTS as usize + 1];
        assert_eq!(
            post(too_big).await.unwrap().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn attachment_names_are_percent_encoded() {
        assert_eq!(percent_encode("Résumé 1.pdf"), "R%C3%A9sum%C3%A9%201.pdf");
        assert_eq!(percent_encode("a\"b;c"), "a%22b%3Bc");
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
        state.gate.config.store(Arc::new(BasicAuthConfig {
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
