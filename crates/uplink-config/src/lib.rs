use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

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
}

fn default_send_delay_secs() -> u32 {
    10
}

/// A bucket of the home view: what `query` matches of every kind.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeBucket {
    pub name: String,
    pub query: String,
}

/// What needs doing now, what keeps things in order, what is to be done
/// some day, and what is to be read at leisure.
fn default_home() -> Vec<HomeBucket> {
    let bucket = |name: &str, query: &str| HomeBucket {
        name: name.into(),
        query: query.into(),
    };
    vec![
        bucket(
            "Immediate",
            "is:mention OR is:invite OR (is:dm is:unread) OR is:overdue OR due:today \
             OR (in:inbox is:unread is:important)",
        ),
        bucket(
            "Upkeep",
            "(is:unread is:notifying -tag:lowpriority) \
             OR (in:inbox -category:promotions -category:social)",
        ),
        bucket("Backlog", "is:starred OR is:todo"),
        bucket("Leisure", "is:unread"),
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
    Matrix(MatrixAccountConfig),
}

impl AccountConfig {
    pub fn id(&self) -> &str {
        match self {
            AccountConfig::Gmail(account) => &account.id,
            AccountConfig::M365(account) => &account.id,
            AccountConfig::Matrix(account) => &account.id,
        }
    }

    /// Sets the paths under `data_dir` that are the account's.
    fn place(&mut self, data_dir: &Path) {
        let token_file = |id: &str| data_dir.join("accounts").join(format!("{id}.token"));
        match self {
            AccountConfig::Gmail(account) => account.oauth.token_file = token_file(&account.id),
            AccountConfig::M365(account) => account.oauth.token_file = token_file(&account.id),
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
}

impl Default for GoogleEndpoints {
    fn default() -> Self {
        Self {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            api: "https://gmail.googleapis.com".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GmailOAuthConfig {
    pub client_id: String,
    pub client_secret: String,
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
        Ok(config)
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
            ["alice-gmail", "alice-m365", "alice-matrix"]
        );
        assert_eq!(
            config.server.tls.unwrap().cert,
            PathBuf::from("/path/to/cert.pem")
        );
    }
    use std::io::Write;

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

  - id: alice-m365
    kind: m365
    address: alice@example.com
    oauth:
      client_id: "11111111-2222-3333-4444-555555555555"

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
        assert_eq!(cfg.accounts.len(), 3);

        let AccountConfig::Gmail(ref g) = cfg.accounts[0] else {
            panic!("expected gmail account");
        };
        assert_eq!(g.id, "alice-gmail");
        assert_eq!(g.oauth.client_secret, "GOCSPX-xxxxxxxxxx");
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

        let AccountConfig::Matrix(ref mx) = cfg.accounts[2] else {
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
        assert!(load("home: []\n").unwrap().home.is_empty());
        let refused = |home: &str| format!("{:#}", load(home).unwrap_err());
        assert!(
            refused("home:\n  - name: A\n    query: x\n  - name: A\n    query: y\n")
                .contains("two buckets are named \"A\"")
        );
        assert!(refused("home:\n  - name: ' '\n    query: x\n").contains("no name"));
        assert!(refused("home:\n  - name: A\n    query: ''\n").contains("no query"));
        assert!(refused("home:\n  - name: A\n    query: 'is:nonsense'\n").contains("bucket \"A\""));
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
        }
    }

    #[test]
    fn diff_detects_home_change() {
        let old = base_config();
        let mut new = old.clone();
        new.home.pop();
        assert_eq!(diff_config_sections(&old, &new), vec![ConfigSection::Home]);
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
