use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
pub use uplink_protocol::{calendar::TimeZone, Badge};

// ── Config schema (SPEC §3.2) ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    /// Where uplink keeps everything (SPEC §4.1): the database, mail, the
    /// master key, accounts' tokens and Matrix stores. Created on first
    /// use.
    pub data_dir: PathBuf,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    /// How long sent mail waits in the outbox, during which sending can be
    /// undone (SPEC §6.7).
    #[serde(default = "default_send_delay_secs")]
    pub send_delay_secs: u32,
    /// The home view's buckets (SPEC §12.13), top to bottom: an item is in
    /// the first whose query matches it.
    #[serde(default = "default_home")]
    pub home: Vec<HomeBucket>,
    #[serde(default)]
    pub calendar: CalendarConfig,
    #[serde(default)]
    pub remote_images: RemoteImagesConfig,
}

/// How the calendar shows (SPEC §10.2).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarConfig {
    /// Time zones the week's hours are also shown in, left to right,
    /// beside the browser's.
    #[serde(default)]
    pub time_zones: Vec<TimeZone>,
}

/// How mail's remote images are fetched (SPEC §8.9).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteImagesConfig {
    /// Hosts fetched from though they resolve to addresses not of the
    /// public internet (an intranet's): exact names, any mail's images of
    /// which make the server reach them.
    #[serde(default)]
    pub private_hosts: Vec<String>,
    /// Hosts whose images the browser loads itself, as the mail has them,
    /// with its cookies: for those that serve them only to those signed
    /// in (an intranet's GitLab's avatars). Exact names.
    #[serde(default)]
    pub direct_hosts: Vec<String>,
}

fn default_send_delay_secs() -> u32 {
    10
}

/// A bucket of the home view: what `query` matches of every kind; if
/// `badge`, it shows on the app's icon (SPEC §12.14).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeBucket {
    pub name: String,
    pub query: String,
    #[serde(default)]
    pub badge: Option<Badge>,
}

/// What needs doing now, counted on the icon; what keeps things in
/// order, a dot there; what is to be done some day, and what is to be
/// read at leisure.
fn default_home() -> Vec<HomeBucket> {
    let bucket = |name: &str, query: &str, badge| HomeBucket {
        name: name.into(),
        query: query.into(),
        badge,
    };
    vec![
        bucket(
            "Immediate",
            "is:mention OR is:invite OR (is:dm is:unread) OR is:overdue OR due:today \
             OR (in:inbox is:unread is:important)",
            Some(Badge::Count),
        ),
        bucket(
            "Upkeep",
            "(is:unread is:notifying -tag:lowpriority) \
             OR (in:inbox -category:promotions -category:social)",
            Some(Badge::Dot),
        ),
        bucket("Backlog", "is:starred OR is:todo", None),
        bucket("Leisure", "is:unread", None),
    ]
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Without it, plain HTTP: for behind a reverse proxy that terminates
    /// TLS, or on loopback.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    pub basic_auth: BasicAuthConfig,
}

fn default_bind() -> String {
    "127.0.0.1:8443".to_string()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasicAuthConfig {
    pub user: String,
    pub password_bcrypt: String,
}

/// Account configuration — tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AccountConfig {
    Gmail(GmailAccountConfig),
    M365(M365AccountConfig),
    Imap(ImapAccountConfig),
    Pop3(Pop3AccountConfig),
    Jmap(JmapAccountConfig),
    Matrix(MatrixAccountConfig),
}

impl AccountConfig {
    pub fn id(&self) -> &str {
        match self {
            AccountConfig::Gmail(account) => &account.id,
            AccountConfig::M365(account) => &account.id,
            AccountConfig::Imap(account) => &account.id,
            AccountConfig::Pop3(account) => &account.id,
            AccountConfig::Jmap(account) => &account.id,
            AccountConfig::Matrix(account) => &account.id,
        }
    }

    /// Sets the paths under `data_dir` that are the account's.
    fn place(&mut self, data_dir: &Path) {
        let token_file = |id: &str| data_dir.join("accounts").join(format!("{id}.token"));
        match self {
            AccountConfig::Gmail(account) => account.oauth.token_file = token_file(&account.id),
            AccountConfig::M365(account) => account.oauth.token_file = token_file(&account.id),
            AccountConfig::Imap(ImapAccountConfig {
                id,
                oauth,
                password_file,
                ..
            })
            | AccountConfig::Pop3(Pop3AccountConfig {
                id,
                oauth,
                password_file,
                ..
            })
            | AccountConfig::Jmap(JmapAccountConfig {
                id,
                oauth,
                password_file,
                ..
            }) => {
                *password_file = data_dir.join("accounts").join(format!("{id}.password"));
                if let Some(oauth) = oauth {
                    oauth.token_file = token_file(id);
                }
            }
            AccountConfig::Matrix(account) => {
                account.state_dir = data_dir.join("matrix").join(&account.id)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GmailAccountConfig {
    pub id: String,
    pub address: String,
    pub oauth: GmailOAuthConfig,
    /// Where Google's services are; tests point these at a fake.
    #[serde(default)]
    pub endpoints: GoogleEndpoints,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleEndpoints {
    /// OAuth authorization endpoint.
    pub auth: String,
    /// OAuth token endpoint.
    pub token: String,
    /// Base URL of the Gmail API, below which `/gmail/v1/...` lives.
    pub api: String,
    /// Base URL of the Cloud Pub/Sub API, below which `/v1/projects/...`
    /// lives.
    pub pubsub: String,
}

impl Default for GoogleEndpoints {
    fn default() -> Self {
        Self {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            api: "https://gmail.googleapis.com".into(),
            pubsub: "https://pubsub.googleapis.com".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GmailOAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    /// The id of the Google Cloud project the client is in, where Gmail's
    /// push notifications are published (SPEC §8.1).
    pub project: String,
    /// `<data_dir>/accounts/<id>.token`.
    #[serde(skip)]
    pub token_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct M365AccountConfig {
    pub id: String,
    pub address: String,
    pub oauth: M365OAuthConfig,
}

/// How uplink signs in to Microsoft 365 (SPEC §13.3): as a public client
/// registered in Entra ID, which the user names.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct M365OAuthConfig {
    /// The registration's application (client) id.
    pub client_id: String,
    /// One of the registration's redirect URIs.
    #[serde(default = "M365OAuthConfig::native_client")]
    pub redirect_uri: String,
    #[serde(default = "M365OAuthConfig::common_authority")]
    pub authority: String,
    #[serde(default = "M365OAuthConfig::graph")]
    pub resource: String,
    /// `<data_dir>/accounts/<id>.token`.
    #[serde(skip)]
    pub token_file: PathBuf,
}

impl M365OAuthConfig {
    /// The redirect URI Entra ID offers desktop applications.
    fn native_client() -> String {
        "https://login.microsoftonline.com/common/oauth2/nativeclient".into()
    }

    fn common_authority() -> String {
        "https://login.microsoftonline.com/common".into()
    }

    fn graph() -> String {
        "https://graph.microsoft.com".into()
    }
}

/// An account at any mail server, read by IMAP and sent from by SMTP
/// (SPEC §8.10–8.11).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImapAccountConfig {
    pub id: String,
    pub address: String,
    /// The name the servers know the account by, if not its address.
    #[serde(default)]
    pub user: Option<String>,
    pub imap: MailServer,
    pub smtp: MailServer,
    /// How uplink signs in to both servers: with XOAUTH2 if set, else with
    /// the password `uplink password --account <id>` sealed.
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
    /// `<data_dir>/accounts/<id>.password`.
    #[serde(skip)]
    pub password_file: PathBuf,
}

impl ImapAccountConfig {
    pub fn login(&self) -> Login<'_> {
        Login {
            id: &self.id,
            address: &self.address,
            user: self.user.as_deref().unwrap_or(&self.address),
            oauth: self.oauth.as_ref(),
            password_file: &self.password_file,
        }
    }

    /// The IMAP server's port: 993, or 143 for STARTTLS, unless set.
    pub fn imap_port(&self) -> u16 {
        self.imap.port_or(993, 143)
    }
}

/// An account at any mail server, read by POP3 and sent from by SMTP
/// (SPEC §8.12): as an IMAP account, but its mail downloaded.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pop3AccountConfig {
    pub id: String,
    pub address: String,
    /// The name the servers know the account by, if not its address.
    #[serde(default)]
    pub user: Option<String>,
    pub pop3: MailServer,
    pub smtp: MailServer,
    /// Whether mail is deleted from the server once uplink has it, rather
    /// than left there for other clients.
    #[serde(default)]
    pub delete_after_download: bool,
    /// As an IMAP account's.
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
    /// `<data_dir>/accounts/<id>.password`.
    #[serde(skip)]
    pub password_file: PathBuf,
}

impl Pop3AccountConfig {
    pub fn login(&self) -> Login<'_> {
        Login {
            id: &self.id,
            address: &self.address,
            user: self.user.as_deref().unwrap_or(&self.address),
            oauth: self.oauth.as_ref(),
            password_file: &self.password_file,
        }
    }

    /// The POP3 server's port: 995, or 110 for STARTTLS, unless set.
    pub fn pop3_port(&self) -> u16 {
        self.pop3.port_or(995, 110)
    }
}

/// How an account at a mail server, IMAP, POP3 or JMAP, signs in.
#[derive(Debug, Clone, Copy)]
pub struct Login<'a> {
    pub id: &'a str,
    pub address: &'a str,
    /// The name the servers know the account by.
    pub user: &'a str,
    /// An OAuth access token if set, else the password `uplink password`
    /// sealed.
    pub oauth: Option<&'a OAuthConfig>,
    pub password_file: &'a Path,
}

/// An account at a mail server that speaks JMAP (RFC 8620, 8621; SPEC
/// §8.13): read, filed and sent from by it alone.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JmapAccountConfig {
    pub id: String,
    pub address: String,
    /// The name the server knows the account by, if not its address.
    #[serde(default)]
    pub user: Option<String>,
    /// The JMAP session resource: for most servers
    /// `https://<host>/.well-known/jmap`.
    pub session_url: HttpsUrl,
    /// How uplink signs in: with an OAuth access token (`Bearer`) if set,
    /// else with the password `uplink password --account <id>` sealed
    /// (`Basic`).
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
    /// `<data_dir>/accounts/<id>.password`.
    #[serde(skip)]
    pub password_file: PathBuf,
}

impl JmapAccountConfig {
    pub fn login(&self) -> Login<'_> {
        Login {
            id: &self.id,
            address: &self.address,
            user: self.user.as_deref().unwrap_or(&self.address),
            oauth: self.oauth.as_ref(),
            password_file: &self.password_file,
        }
    }
}

/// A URL of `https:`: credentials and mail go in no other.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct HttpsUrl(String);

impl HttpsUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HttpsUrl {
    type Error = String;

    fn try_from(url: String) -> Result<Self, String> {
        match url.strip_prefix("https://") {
            Some(rest) if !rest.is_empty() => Ok(Self(url)),
            _ => Err(format!("{url} is not an https: URL")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailServer {
    pub host: String,
    /// The protocol's standard port for `security`, unless set.
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub security: Security,
}

impl MailServer {
    /// As the account's SMTP server, its port: 465, or 587 for STARTTLS,
    /// unless set.
    pub fn smtp_port(&self) -> u16 {
        self.port_or(465, 587)
    }

    /// `port`, else the protocol's port for TLS or for STARTTLS.
    fn port_or(&self, tls: u16, starttls: u16) -> u16 {
        self.port.unwrap_or(match self.security {
            Security::Tls => tls,
            Security::Starttls => starttls,
        })
    }
}

/// How a connection to a mail server is secured. Either way it is TLS:
/// uplink sends no credentials or mail in the clear.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// TLS from the start ("implicit TLS", RFC 8314).
    #[default]
    Tls,
    /// Upgraded from plain text by STARTTLS.
    Starttls,
}

/// A provider's OAuth client, for the authorization-code flow with PKCE
/// and a loopback redirect (SPEC §13.2), whose access tokens sign in by
/// XOAUTH2.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    pub client_id: String,
    /// For providers that give installed applications a secret too.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// The authorization endpoint.
    pub auth_url: String,
    /// The token endpoint.
    pub token_url: String,
    /// Space-separated, as the provider names them: mail access, and
    /// whatever its refresh tokens need (e.g. `offline_access`).
    pub scope: String,
    /// `<data_dir>/accounts/<id>.token`.
    #[serde(skip)]
    pub token_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixAccountConfig {
    pub id: String,
    pub homeserver: String,
    pub mxid: String,
    /// `<data_dir>/matrix/<id>`: matrix-rust-sdk's stores and the session.
    #[serde(skip)]
    pub state_dir: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading config file {} (`uplink init` writes one)",
                path.display()
            )
        })?;
        anyhow::ensure!(
            !text.trim().is_empty(),
            "config file {} is empty (`uplink init` writes one)",
            path.display()
        );
        let mut config: Config = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        // Relative paths are relative to the file's directory.
        let base = path.parent().expect("a file has a parent");
        config.data_dir = base.join(&config.data_dir);
        if let Some(tls) = &mut config.server.tls {
            tls.cert = base.join(&tls.cert);
            tls.key = base.join(&tls.key);
        }
        for account in &mut config.accounts {
            account.place(&config.data_dir);
        }
        config.check_home().context("in home")?;
        config.check_calendar().context("in calendar")?;
        Ok(config)
    }

    /// Fails on a time zone not of the IANA database, or without a label.
    fn check_calendar(&self) -> Result<()> {
        for zone in &self.calendar.time_zones {
            anyhow::ensure!(
                !zone.label.trim().is_empty(),
                "time zone {} has no label",
                zone.zone
            );
            jiff::tz::TimeZone::get(&zone.zone)
                .with_context(|| format!("time zone {:?}", zone.zone))?;
        }
        Ok(())
    }

    /// Fails on a bucket without a name, or of a name another has, or
    /// whose query is not of the language.
    fn check_home(&self) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        for bucket in &self.home {
            anyhow::ensure!(!bucket.name.trim().is_empty(), "a bucket has no name");
            anyhow::ensure!(
                names.insert(&bucket.name),
                "two buckets are named {:?}",
                bucket.name
            );
            anyhow::ensure!(
                !bucket.query.trim().is_empty(),
                "bucket {:?} has no query",
                bucket.name
            );
            uplink_query::parse(&bucket.query, 0)
                .map_err(|e| anyhow::anyhow!("bucket {:?}: {}", bucket.name, e.0))?;
        }
        Ok(())
    }

    /// Seals accounts' secrets; made on first use.
    pub fn master_key_file(&self) -> PathBuf {
        self.data_dir.join("master.key")
    }
}

// ── Section-diff ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSection {
    Tls,
    BasicAuth,
    Accounts,
    /// Restart required per SPEC §3.3.
    Bind,
    /// Restart required: the database is opened once at startup.
    Data,
    /// Restart required.
    SendDelay,
    Home,
    Calendar,
    RemoteImages,
}

#[derive(Debug, Clone)]
pub struct ConfigChanged {
    pub section: ConfigSection,
}

/// Returns one entry per config section that differs between `old` and `new`.
pub fn diff_config_sections(old: &Config, new: &Config) -> Vec<ConfigSection> {
    let mut changed = Vec::new();
    if old.server.bind != new.server.bind {
        changed.push(ConfigSection::Bind);
    }
    if old.data_dir != new.data_dir {
        changed.push(ConfigSection::Data);
    }
    if old.send_delay_secs != new.send_delay_secs {
        changed.push(ConfigSection::SendDelay);
    }
    if old.server.tls != new.server.tls {
        changed.push(ConfigSection::Tls);
    }
    if old.server.basic_auth != new.server.basic_auth {
        changed.push(ConfigSection::BasicAuth);
    }
    if old.home != new.home {
        changed.push(ConfigSection::Home);
    }
    if old.calendar != new.calendar {
        changed.push(ConfigSection::Calendar);
    }
    if old.remote_images != new.remote_images {
        changed.push(ConfigSection::RemoteImages);
    }
    // M3+: per-account add/remove/modify diffing
    if old.accounts != new.accounts {
        changed.push(ConfigSection::Accounts);
    }
    changed
}

// ── Watcher ───────────────────────────────────────────────────────────────────

/// Watches the config file (and the configured TLS cert/key files) for
/// changes and broadcasts `ConfigChanged` events via the supplied sender.
///
/// Coalesces rapid save-sequences (~250 ms debounce) so editor patterns
/// (vim temp-file rename) don't produce spurious duplicate events.
///
/// Dropping the `Watcher` stops the background thread cleanly.
pub struct Watcher {
    // Kept to drive the inotify subscription; dropping it closes the event
    // channel, which causes the background thread to exit.
    _watcher: notify::RecommendedWatcher,
    _thread: std::thread::JoinHandle<()>,
}

impl Watcher {
    pub fn new(
        config_path: PathBuf,
        tx: tokio::sync::broadcast::Sender<ConfigChanged>,
    ) -> Result<Self> {
        use notify::Watcher as _;

        let initial_config = Config::load(&config_path)?;

        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();

        let mut file_watcher = notify::RecommendedWatcher::new(evt_tx, notify::Config::default())
            .context("creating file watcher")?;

        // Watch the config file's parent directory so we catch both in-place
        // modifications and write-then-rename patterns (e.g. vim, sed -i).
        let config_parent = config_path
            .parent()
            .context("config path must have a parent directory")?;
        file_watcher
            .watch(config_parent, notify::RecursiveMode::NonRecursive)
            .with_context(|| format!("watching config dir {}", config_parent.display()))?;

        // Derive the initial cert/key paths and watch their parent directories
        // if they differ from the config's parent (so cert renewal in a
        // separate directory also fires a reload).
        let tls_paths: Vec<PathBuf> = initial_config
            .server
            .tls
            .iter()
            .flat_map(|tls| [tls.cert.clone(), tls.key.clone()])
            .collect();

        for tls_path in &tls_paths {
            if let Some(parent) = tls_path.parent() {
                if parent != config_parent && parent.exists() {
                    if let Err(e) = file_watcher.watch(parent, notify::RecursiveMode::NonRecursive)
                    {
                        tracing::warn!("config: could not watch {}: {e}", parent.display());
                    }
                }
            }
        }

        let thread = std::thread::spawn(move || {
            watcher_thread(config_path, tls_paths, initial_config, evt_rx, tx);
        });

        Ok(Self {
            _watcher: file_watcher,
            _thread: thread,
        })
    }
}

fn watcher_thread(
    config_path: PathBuf,
    tls_paths: Vec<PathBuf>,
    initial_config: Config,
    evt_rx: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    tx: tokio::sync::broadcast::Sender<ConfigChanged>,
) {
    let mut current_config = initial_config;

    // Use filenames for matching so we're robust against path normalization
    // differences (symlinks, /proc/self/fd, etc.) between what we registered
    // and what notify returns in event.paths.
    let config_name = config_path.file_name().map(std::ffi::OsStr::to_os_string);
    let tls_names: Vec<std::ffi::OsString> = tls_paths
        .iter()
        .filter_map(|p| p.file_name().map(std::ffi::OsStr::to_os_string))
        .collect();

    loop {
        let event = match evt_rx.recv() {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => {
                tracing::warn!("config: watcher error: {e}");
                continue;
            }
            Err(_) => break, // watcher dropped → channel closed
        };

        if !is_relevant(&event) {
            continue;
        }

        // Debounce: sleep then drain any queued events, accumulating all
        // touched paths.  For rename-over (write-then-rename), the first
        // event carries the source name (e.g. cert_new.pem) and the second
        // carries the destination (cert.pem) — we must inspect both.
        let mut all_paths: Vec<PathBuf> = event.paths.clone();
        std::thread::sleep(std::time::Duration::from_millis(250));
        while let Ok(Ok(drained)) = evt_rx.try_recv() {
            if is_relevant(&drained) {
                all_paths.extend(drained.paths);
            }
        }

        // Determine what changed based on which path was affected.
        let config_touched = config_name.as_ref().is_some_and(|n| {
            all_paths
                .iter()
                .any(|p| p.file_name() == Some(n.as_os_str()))
        });
        let tls_file_touched = all_paths.iter().any(|p| {
            tls_names
                .iter()
                .any(|n| p.file_name() == Some(n.as_os_str()))
        });

        if config_touched {
            match Config::load(&config_path) {
                Ok(new_config) => {
                    let sections = diff_config_sections(&current_config, &new_config);
                    for section in sections {
                        let _ = tx.send(ConfigChanged { section });
                    }
                    current_config = new_config;
                }
                Err(e) => {
                    tracing::error!("config: reload failed, keeping old config: {e:#}");
                }
            }
        } else if tls_file_touched {
            let _ = tx.send(ConfigChanged {
                section: ConfigSection::Tls,
            });
        }
    }
}

fn is_relevant(event: &notify::Event) -> bool {
    // Accept any event that might indicate file content changed.
    // Rename-over (write-then-rename editor pattern) generates Create events
    // for the destination; Modify for in-place writes; Other/Any for platform-
    // specific events. We exclude only pure Access events (reads).
    !matches!(event.kind, notify::EventKind::Access(_))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/uplink.yaml");
        let config = Config::load(&example).unwrap();
        assert!(config.accounts.is_empty());
        assert_eq!(config.data_dir, example.parent().unwrap().join("./data"));
        assert_eq!(config.home, default_home());
        // Its commented accounts, and TLS, are right once uncommented.
        let uncommented = std::fs::read_to_string(&example)
            .unwrap()
            .replace("accounts: []", "accounts:")
            .replace("  # tls:", "  tls:")
            .replace("  #   ", "    ")
            .replace("  # - ", "  - ");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uplink.yaml");
        std::fs::write(&path, uncommented).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config
                .accounts
                .iter()
                .map(AccountConfig::id)
                .collect::<Vec<_>>(),
            [
                "alice-gmail",
                "alice-m365",
                "alice-mail",
                "alice-pop",
                "alice-jmap",
                "alice-matrix"
            ]
        );
        assert_eq!(
            config.server.tls.unwrap().cert,
            PathBuf::from("/path/to/cert.pem")
        );
    }
    use std::io::Write;

    #[test]
    fn calendar_time_zones_are_of_the_iana_database() {
        let dir = tempfile::tempdir().unwrap();
        let config = |zones: &str| {
            let path = dir.path().join("uplink.yaml");
            std::fs::write(
                &path,
                format!(
                    "server: {{ bind: \"127.0.0.1:1\", basic_auth: {{ user: a, password_bcrypt: x }} }}\n\
                     data_dir: data\ncalendar:\n  time_zones: {zones}\n"
                ),
            )
            .unwrap();
            Config::load(&path)
        };
        let ok =
            config("[{ label: Office, zone: Europe/Berlin }, { label: UTC, zone: UTC }]").unwrap();
        assert_eq!(
            ok.calendar
                .time_zones
                .iter()
                .map(|z| z.zone.as_str())
                .collect::<Vec<_>>(),
            ["Europe/Berlin", "UTC"]
        );
        let err = config("[{ label: Office, zone: Europe/Berlinn }]").unwrap_err();
        assert!(format!("{err:#}").contains("Europe/Berlinn"), "{err:#}");
        assert!(config("[{ label: \" \", zone: UTC }]").is_err());
    }

    /// Happy-path: verbatim SPEC §3.2 example (with all account kinds).
    #[test]
    fn parse_full_spec_example() {
        let yaml = r#"
server:
  bind: "0.0.0.0:8443"
  tls:
    cert: /etc/uplink/cert.pem
    key:  /etc/uplink/key.pem
  basic_auth:
    user: alice
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"

data_dir: /var/lib/uplink

accounts:
  - id: alice-gmail
    kind: gmail
    address: alice@gmail.com
    oauth:
      client_id:     "1234.apps.googleusercontent.com"
      client_secret: "GOCSPX-xxxxxxxxxx"
      project:       "uplink-123456"

  - id: alice-m365
    kind: m365
    address: alice@example.com
    oauth:
      client_id: "11111111-2222-3333-4444-555555555555"

  - id: alice-mail
    kind: imap
    address: alice@example.org
    imap: { host: imap.example.org }
    smtp: { host: smtp.example.org }

  - id: alice-pop
    kind: pop3
    address: alice@example.net
    pop3: { host: pop.example.net }
    smtp: { host: smtp.example.net }

  - id: alice-matrix
    kind: matrix
    homeserver: "https://matrix.example.org"
    mxid: "@alice:example.org"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).unwrap();

        assert_eq!(cfg.server.bind, "0.0.0.0:8443");
        assert_eq!(
            cfg.server.tls.as_ref().unwrap().cert,
            Path::new("/etc/uplink/cert.pem")
        );
        assert_eq!(cfg.data_dir, PathBuf::from("/var/lib/uplink"));
        assert_eq!(
            cfg.master_key_file(),
            PathBuf::from("/var/lib/uplink/master.key")
        );
        assert_eq!(cfg.accounts.len(), 5);

        let AccountConfig::Gmail(ref g) = cfg.accounts[0] else {
            panic!("expected gmail account");
        };
        assert_eq!(g.id, "alice-gmail");
        assert_eq!(g.oauth.client_secret, "GOCSPX-xxxxxxxxxx");
        assert_eq!(g.oauth.project, "uplink-123456");
        assert_eq!(
            g.oauth.token_file,
            PathBuf::from("/var/lib/uplink/accounts/alice-gmail.token")
        );

        let AccountConfig::M365(ref m) = cfg.accounts[1] else {
            panic!("expected m365 account");
        };
        // Microsoft's sign-in and Graph, unless said otherwise.
        assert_eq!(
            m.oauth,
            M365OAuthConfig {
                client_id: "11111111-2222-3333-4444-555555555555".into(),
                redirect_uri: "https://login.microsoftonline.com/common/oauth2/nativeclient".into(),
                authority: "https://login.microsoftonline.com/common".into(),
                resource: "https://graph.microsoft.com".into(),
                token_file: PathBuf::from("/var/lib/uplink/accounts/alice-m365.token"),
            }
        );

        let AccountConfig::Imap(ref i) = cfg.accounts[2] else {
            panic!("expected imap account");
        };
        // The address signs in, over TLS from the start, with the password
        // `uplink password` seals.
        assert_eq!(i.login().user, "alice@example.org");
        assert_eq!((i.imap_port(), i.smtp.smtp_port()), (993, 465));
        assert_eq!(i.imap.security, Security::Tls);
        assert_eq!(i.oauth, None);
        assert_eq!(
            i.password_file,
            PathBuf::from("/var/lib/uplink/accounts/alice-mail.password")
        );

        let AccountConfig::Pop3(ref p) = cfg.accounts[3] else {
            panic!("expected pop3 account");
        };
        assert!(!p.delete_after_download);
        assert_eq!(p.pop3_port(), 995);

        let AccountConfig::Matrix(ref mx) = cfg.accounts[4] else {
            panic!("expected matrix account");
        };
        assert_eq!(mx.mxid, "@alice:example.org");
        assert_eq!(
            mx.state_dir,
            PathBuf::from("/var/lib/uplink/matrix/alice-matrix")
        );
    }

    #[test]
    fn minimal_config_parses_with_defaults() {
        let yaml = r#"
server:
  bind: "127.0.0.1:8443"
  tls:
    cert: "/tmp/cert.pem"
    key: "/tmp/key.pem"
  basic_auth:
    user: alice
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
data_dir: /tmp/uplink
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.server.bind, "127.0.0.1:8443");
        assert_eq!(cfg.server.basic_auth.user, "alice");
        assert!(cfg.accounts.is_empty());
        assert_eq!(cfg.data_dir, PathBuf::from("/tmp/uplink"));
    }

    #[test]
    fn default_bind_when_omitted() {
        let yaml = r#"
server:
  tls:
    cert: "/tmp/cert.pem"
    key: "/tmp/key.pem"
  basic_auth:
    user: alice
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
data_dir: /tmp/uplink
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.server.bind, "127.0.0.1:8443");
    }

    #[test]
    fn tls_is_optional() {
        let yaml = r#"
server:
  basic_auth:
    user: alice
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
data_dir: /tmp/uplink
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        assert_eq!(Config::load(f.path()).unwrap().server.tls, None);
    }

    #[test]
    fn an_empty_file_points_to_init() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(format!("{err:#}").contains("uplink init"), "{err:#}");
    }

    #[test]
    fn imap_accounts_say_how_to_reach_and_sign_in() {
        let yaml = r#"
server:
  basic_auth:
    user: alice
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
data_dir: /tmp/uplink
accounts:
  - id: mail
    kind: imap
    address: alice@example.org
    user: alice
    imap: { host: mail.example.org, security: starttls }
    smtp: { host: mail.example.org, security: starttls, port: 2525 }
    oauth:
      client_id: uplink
      auth_url: https://auth.example.org/authorize
      token_url: https://auth.example.org/token
      scope: mail offline_access
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).unwrap();
        let AccountConfig::Imap(ref i) = cfg.accounts[0] else {
            panic!("expected imap account");
        };
        assert_eq!(i.login().user, "alice");
        assert_eq!((i.imap_port(), i.smtp.smtp_port()), (143, 2525));
        let oauth = i.oauth.as_ref().unwrap();
        assert_eq!(oauth.client_secret, None);
        assert_eq!(
            oauth.token_file,
            PathBuf::from("/tmp/uplink/accounts/mail.token")
        );

        // Nothing goes in the clear.
        let plain = yaml.replace("security: starttls, port", "security: none, port");
        std::fs::write(f.path(), plain).unwrap();
        let err = format!("{:#}", Config::load(f.path()).unwrap_err());
        assert!(err.contains("none"), "{err}");
    }

    #[test]
    fn pop3_accounts_leave_mail_on_the_server_unless_told() {
        let yaml = r#"
server:
  basic_auth:
    user: alice
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
data_dir: /tmp/uplink
accounts:
  - id: old
    kind: pop3
    address: alice@example.org
    pop3: { host: pop.example.org }
    smtp: { host: smtp.example.org, security: starttls }
  - id: older
    kind: pop3
    address: alice@example.net
    pop3: { host: pop.example.net, security: starttls }
    smtp: { host: smtp.example.net }
    delete_after_download: true
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).unwrap();
        let AccountConfig::Pop3(ref old) = cfg.accounts[0] else {
            panic!("expected pop3 account");
        };
        assert!(!old.delete_after_download);
        assert_eq!((old.pop3_port(), old.smtp.smtp_port()), (995, 587));
        assert_eq!(old.login().user, "alice@example.org");
        assert_eq!(
            old.password_file,
            PathBuf::from("/tmp/uplink/accounts/old.password")
        );
        let AccountConfig::Pop3(ref older) = cfg.accounts[1] else {
            panic!("expected pop3 account");
        };
        assert!(older.delete_after_download);
        assert_eq!((older.pop3_port(), older.smtp.smtp_port()), (110, 465));
    }

    #[test]
    fn jmap_accounts_name_their_session_over_https() {
        let yaml = r#"
server:
  basic_auth:
    user: alice
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
data_dir: /tmp/uplink
accounts:
  - id: home
    kind: jmap
    address: alice@example.org
    session_url: https://mail.example.org/.well-known/jmap
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).unwrap();
        let AccountConfig::Jmap(ref j) = cfg.accounts[0] else {
            panic!("expected jmap account");
        };
        assert_eq!(
            j.session_url.as_str(),
            "https://mail.example.org/.well-known/jmap"
        );
        // The address signs in, with the password `uplink password` seals.
        assert_eq!(j.login().user, "alice@example.org");
        assert_eq!(j.oauth, None);
        assert_eq!(
            j.password_file,
            PathBuf::from("/tmp/uplink/accounts/home.password")
        );

        // Nothing goes in the clear.
        let plain = yaml.replace("https://mail", "http://mail");
        std::fs::write(f.path(), plain).unwrap();
        let err = format!("{:#}", Config::load(f.path()).unwrap_err());
        assert!(err.contains("not an https: URL"), "{err}");
    }

    #[test]
    fn bad_account_kind_fails() {
        let yaml = r#"
server:
  tls:
    cert: /tmp/cert.pem
    key: /tmp/key.pem
  basic_auth:
    user: alice
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
data_dir: /tmp/uplink
accounts:
  - id: bad
    kind: exchange
    address: user@corp.com
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(
            err.to_string().contains("exchange") || format!("{err:#}").contains("exchange"),
            "error should mention the bad kind value: {err:#}"
        );
    }

    #[test]
    fn missing_required_field_fails() {
        // Gmail account missing 'oauth'
        let yaml = r#"
server:
  tls:
    cert: /tmp/cert.pem
    key: /tmp/key.pem
  basic_auth:
    user: alice
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
data_dir: /tmp/uplink
accounts:
  - id: alice-gmail
    kind: gmail
    address: alice@gmail.com
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        assert!(Config::load(f.path()).is_err());
    }

    /// Buckets are named, each its own name, and of the query language;
    /// without any, the defaults.
    #[test]
    fn home_buckets_are_checked() {
        let load = |home: &str| {
            let yaml = format!(
                "server:\n  basic_auth:\n    user: a\n    password_bcrypt: x\ndata_dir: /tmp/uplink\n{home}"
            );
            let mut f = tempfile::NamedTempFile::new().unwrap();
            f.write_all(yaml.as_bytes()).unwrap();
            Config::load(f.path())
        };
        let defaults = load("").unwrap().home;
        assert_eq!(defaults, default_home());
        for bucket in &defaults {
            uplink_query::parse(&bucket.query, 0).unwrap();
        }
        let home = load("home:\n  - name: Now\n    query: is:mention\n")
            .unwrap()
            .home;
        assert_eq!(home.len(), 1);
        assert_eq!(home[0].badge, None);
        let badged = load("home:\n  - name: Now\n    query: is:mention\n    badge: count\n")
            .unwrap()
            .home;
        assert_eq!(badged[0].badge, Some(Badge::Count));
        assert!(load("home: []\n").unwrap().home.is_empty());
        let refused = |home: &str| format!("{:#}", load(home).unwrap_err());
        assert!(
            refused("home:\n  - name: A\n    query: x\n  - name: A\n    query: y\n")
                .contains("two buckets are named \"A\"")
        );
        assert!(refused("home:\n  - name: ' '\n    query: x\n").contains("no name"));
        assert!(refused("home:\n  - name: A\n    query: ''\n").contains("no query"));
        assert!(refused("home:\n  - name: A\n    query: 'is:nonsense'\n").contains("bucket \"A\""));
        assert!(
            refused("home:\n  - name: A\n    query: x\n    badge: loud\n")
                .contains("unknown variant")
        );
    }

    #[test]
    fn unknown_field_fails() {
        let yaml = r#"
server:
  tls:
    cert: /tmp/cert.pem
    key: /tmp/key.pem
  basic_auth:
    user: alice
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
  unknown_top_level_key: oops
data_dir: /tmp/uplink
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        assert!(Config::load(f.path()).is_err());
    }

    // ── Section-diff tests ────────────────────────────────────────────────────

    fn base_config() -> Config {
        Config {
            server: ServerConfig {
                bind: "0.0.0.0:8443".into(),
                tls: Some(TlsConfig {
                    cert: "/tmp/cert.pem".into(),
                    key: "/tmp/key.pem".into(),
                }),
                basic_auth: BasicAuthConfig {
                    user: "alice".into(),
                    password_bcrypt: "hash".into(),
                },
            },
            data_dir: PathBuf::from("/tmp/uplink"),
            send_delay_secs: 10,
            accounts: vec![],
            home: default_home(),
            calendar: CalendarConfig::default(),
            remote_images: RemoteImagesConfig::default(),
        }
    }

    #[test]
    fn diff_detects_calendar_change() {
        let old = base_config();
        let mut new = old.clone();
        new.calendar.time_zones.push(TimeZone {
            label: "UTC".into(),
            zone: "UTC".into(),
        });
        assert_eq!(
            diff_config_sections(&old, &new),
            vec![ConfigSection::Calendar]
        );
    }

    #[test]
    fn diff_detects_home_change() {
        let old = base_config();
        let mut new = old.clone();
        new.home.pop();
        assert_eq!(diff_config_sections(&old, &new), vec![ConfigSection::Home]);
    }

    #[test]
    fn diff_detects_remote_images_change() {
        let old = base_config();
        let mut new = old.clone();
        new.remote_images
            .private_hosts
            .push("git.example.org".into());
        assert_eq!(
            diff_config_sections(&old, &new),
            vec![ConfigSection::RemoteImages]
        );
    }

    #[test]
    fn diff_empty_when_identical() {
        let cfg = base_config();
        assert!(diff_config_sections(&cfg, &cfg.clone()).is_empty());
    }

    #[test]
    fn diff_detects_tls_change() {
        let old = base_config();
        let mut new = old.clone();
        new.server.tls.as_mut().unwrap().cert = "/tmp/cert2.pem".into();
        assert_eq!(diff_config_sections(&old, &new), vec![ConfigSection::Tls]);
    }

    #[test]
    fn diff_detects_basic_auth_change() {
        let old = base_config();
        let mut new = old.clone();
        new.server.basic_auth.password_bcrypt = "newhash".into();
        assert_eq!(
            diff_config_sections(&old, &new),
            vec![ConfigSection::BasicAuth]
        );
    }

    #[test]
    fn diff_detects_bind_change() {
        let old = base_config();
        let mut new = old.clone();
        new.server.bind = "127.0.0.1:9000".into();
        assert_eq!(diff_config_sections(&old, &new), vec![ConfigSection::Bind]);
    }

    #[test]
    fn diff_detects_data_change() {
        let old = base_config();
        let mut new = old.clone();
        new.data_dir = PathBuf::from("/tmp/elsewhere");
        assert_eq!(diff_config_sections(&old, &new), vec![ConfigSection::Data]);
    }

    #[test]
    fn diff_detects_accounts_change() {
        let old = base_config();
        let mut new = old.clone();
        new.accounts
            .push(AccountConfig::Matrix(MatrixAccountConfig {
                id: "mx".into(),
                homeserver: "https://matrix.org".into(),
                mxid: "@u:matrix.org".into(),
                state_dir: PathBuf::from("/tmp/mx"),
            }));
        assert_eq!(
            diff_config_sections(&old, &new),
            vec![ConfigSection::Accounts]
        );
    }

    // ── Watcher tests ─────────────────────────────────────────────────────────

    fn write_minimal_config(path: &Path, cert: &str, key: &str, bind: &str) {
        let yaml = format!(
            "server:\n  bind: \"{bind}\"\n  tls:\n    cert: \"{cert}\"\n    key: \"{key}\"\n  basic_auth:\n    user: alice\n    password_bcrypt: \"$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"\ndata_dir: /tmp/uplink\n"
        );
        std::fs::write(path, yaml).unwrap();
    }

    #[tokio::test]
    async fn watcher_emits_tls_on_cert_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cfg_path = dir.path().join("uplink.yaml");

        std::fs::write(&cert_path, b"cert1").unwrap();
        std::fs::write(&key_path, b"key1").unwrap();
        write_minimal_config(
            &cfg_path,
            cert_path.to_str().unwrap(),
            key_path.to_str().unwrap(),
            "127.0.0.1:8443",
        );

        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let _watcher = Watcher::new(cfg_path.clone(), tx).unwrap();

        // Overwrite the cert file in-place.
        std::fs::write(&cert_path, b"cert2").unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for ConfigChanged")
            .expect("channel error");

        assert_eq!(event.section, ConfigSection::Tls);
    }

    #[tokio::test]
    async fn watcher_emits_basic_auth_on_config_change() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cfg_path = dir.path().join("uplink.yaml");

        std::fs::write(&cert_path, b"cert").unwrap();
        std::fs::write(&key_path, b"key").unwrap();
        write_minimal_config(
            &cfg_path,
            cert_path.to_str().unwrap(),
            key_path.to_str().unwrap(),
            "127.0.0.1:8443",
        );

        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let _watcher = Watcher::new(cfg_path.clone(), tx).unwrap();

        // Rewrite the config with a changed password_bcrypt, keeping bind
        // the same so only BasicAuth shows up in the diff.
        let new_yaml = format!(
            "server:\n  bind: \"127.0.0.1:8443\"\n  tls:\n    cert: \"{}\"\n    key: \"{}\"\n  basic_auth:\n    user: alice\n    password_bcrypt: \"NEWHASH\"\ndata_dir: /tmp/uplink\n",
            cert_path.display(),
            key_path.display()
        );
        std::fs::write(&cfg_path, new_yaml).unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for ConfigChanged")
            .expect("channel error");

        assert_eq!(event.section, ConfigSection::BasicAuth);
    }

    #[tokio::test]
    async fn watcher_emits_tls_on_tls_path_change() {
        let dir = tempfile::tempdir().unwrap();
        let cert1 = dir.path().join("cert1.pem");
        let cert2 = dir.path().join("cert2.pem");
        let key_path = dir.path().join("key.pem");
        let cfg_path = dir.path().join("uplink.yaml");

        std::fs::write(&cert1, b"cert1").unwrap();
        std::fs::write(&cert2, b"cert2").unwrap();
        std::fs::write(&key_path, b"key").unwrap();
        write_minimal_config(
            &cfg_path,
            cert1.to_str().unwrap(),
            key_path.to_str().unwrap(),
            "127.0.0.1:8443",
        );

        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let _watcher = Watcher::new(cfg_path.clone(), tx).unwrap();

        // Update config to point at cert2.
        write_minimal_config(
            &cfg_path,
            cert2.to_str().unwrap(),
            key_path.to_str().unwrap(),
            "127.0.0.1:8443",
        );

        let event = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for ConfigChanged")
            .expect("channel error");

        assert_eq!(event.section, ConfigSection::Tls);
    }

    #[tokio::test]
    async fn watcher_emits_accounts_on_accounts_change() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cfg_path = dir.path().join("uplink.yaml");

        std::fs::write(&cert_path, b"cert").unwrap();
        std::fs::write(&key_path, b"key").unwrap();
        write_minimal_config(
            &cfg_path,
            cert_path.to_str().unwrap(),
            key_path.to_str().unwrap(),
            "127.0.0.1:8443",
        );

        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let _watcher = Watcher::new(cfg_path.clone(), tx).unwrap();

        // Add an account to the config, keeping bind the same so only
        // Accounts shows up in the diff.
        let new_yaml = format!(
            "server:\n  bind: \"127.0.0.1:8443\"\n  tls:\n    cert: \"{cert}\"\n    key: \"{key}\"\n  basic_auth:\n    user: alice\n    password_bcrypt: \"$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"\ndata_dir: /tmp/uplink\naccounts:\n  - id: mx\n    kind: matrix\n    homeserver: \"https://matrix.example.org\"\n    mxid: \"@alice:example.org\"\n",
            cert = cert_path.display(),
            key = key_path.display()
        );
        std::fs::write(&cfg_path, new_yaml).unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for ConfigChanged")
            .expect("channel error");

        assert_eq!(event.section, ConfigSection::Accounts);
    }

    #[tokio::test]
    async fn watcher_coalesces_rapid_events() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cfg_path = dir.path().join("uplink.yaml");

        std::fs::write(&cert_path, b"cert").unwrap();
        std::fs::write(&key_path, b"key").unwrap();
        write_minimal_config(
            &cfg_path,
            cert_path.to_str().unwrap(),
            key_path.to_str().unwrap(),
            "127.0.0.1:8443",
        );

        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let _watcher = Watcher::new(cfg_path.clone(), tx).unwrap();

        // Write the cert file 5 times in quick succession.
        for i in 0..5u8 {
            std::fs::write(&cert_path, [i]).unwrap();
        }

        // We should get at most 2 events (the rapid writes coalesce).
        let first = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out")
            .expect("channel error");
        assert_eq!(first.section, ConfigSection::Tls);

        // No second event should arrive within a short window.
        let second = tokio::time::timeout(std::time::Duration::from_millis(400), rx.recv()).await;
        assert!(second.is_err(), "expected no second event but got one");
    }

    #[tokio::test]
    async fn watcher_keeps_old_config_on_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cfg_path = dir.path().join("uplink.yaml");

        std::fs::write(&cert_path, b"cert").unwrap();
        std::fs::write(&key_path, b"key").unwrap();
        write_minimal_config(
            &cfg_path,
            cert_path.to_str().unwrap(),
            key_path.to_str().unwrap(),
            "127.0.0.1:8443",
        );

        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let _watcher = Watcher::new(cfg_path.clone(), tx).unwrap();

        // Write invalid YAML.
        std::fs::write(&cfg_path, b"not: valid: yaml: [[[").unwrap();

        // No event should be emitted when the config is invalid.
        let result = tokio::time::timeout(std::time::Duration::from_millis(600), rx.recv()).await;
        assert!(
            result.is_err(),
            "expected no event on parse error but got one"
        );
    }
}
