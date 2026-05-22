use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

// ── Config schema (SPEC §3.2) ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub data: Option<DataConfig>,
    #[serde(default)]
    pub master_key_file: Option<PathBuf>,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    pub tls: TlsConfig,
    pub basic_auth: BasicAuthConfig,
}

fn default_bind() -> String {
    "0.0.0.0:8443".to_string()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasicAuthConfig {
    pub user: String,
    pub password_bcrypt: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataConfig {
    pub root: PathBuf,
}

/// Account configuration — tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AccountConfig {
    Gmail(GmailAccountConfig),
    M365(M365AccountConfig),
    Matrix(MatrixAccountConfig),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GmailAccountConfig {
    pub id: String,
    pub address: String,
    pub oauth: GmailOAuthConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GmailOAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub token_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct M365AccountConfig {
    pub id: String,
    pub address: String,
    pub oauth: M365OAuthConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct M365OAuthConfig {
    pub client_id: String,
    pub authority: String,
    pub resource: String,
    pub token_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixAccountConfig {
    pub id: String,
    pub homeserver: String,
    pub mxid: String,
    pub state_dir: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let config: Config =
            serde_yaml::from_str(&text).with_context(|| "parsing config YAML")?;
        Ok(config)
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
    if old.server.tls != new.server.tls {
        changed.push(ConfigSection::Tls);
    }
    if old.server.basic_auth != new.server.basic_auth {
        changed.push(ConfigSection::BasicAuth);
    }
    // M3+: per-account add/remove/modify diffing
    if old.accounts != new.accounts {
        changed.push(ConfigSection::Accounts);
    }
    changed
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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

data:
  root: /var/lib/uplink

master_key_file: /etc/uplink/master.key

accounts:
  - id: alice-gmail
    kind: gmail
    address: alice@gmail.com
    oauth:
      client_id:     "1234.apps.googleusercontent.com"
      client_secret: "GOCSPX-xxxxxxxxxx"
      token_file:    /var/lib/uplink/accounts/alice-gmail.token

  - id: alice-m365
    kind: m365
    address: alice@example.com
    oauth:
      client_id: "d3590ed6-52b3-4102-aeff-aad2292ab01c"
      authority: "https://login.microsoftonline.com/common"
      resource: "https://graph.microsoft.com"
      token_file: /var/lib/uplink/accounts/alice-m365.token

  - id: alice-matrix
    kind: matrix
    homeserver: "https://matrix.example.org"
    mxid: "@alice:example.org"
    state_dir: /var/lib/uplink/matrix/alice-matrix
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).unwrap();

        assert_eq!(cfg.server.bind, "0.0.0.0:8443");
        assert_eq!(cfg.server.tls.cert, "/etc/uplink/cert.pem");
        assert_eq!(
            cfg.data.as_ref().unwrap().root,
            PathBuf::from("/var/lib/uplink")
        );
        assert_eq!(
            cfg.master_key_file,
            Some(PathBuf::from("/etc/uplink/master.key"))
        );
        assert_eq!(cfg.accounts.len(), 3);

        let AccountConfig::Gmail(ref g) = cfg.accounts[0] else {
            panic!("expected gmail account");
        };
        assert_eq!(g.id, "alice-gmail");
        assert_eq!(g.oauth.client_secret, "GOCSPX-xxxxxxxxxx");

        let AccountConfig::M365(ref m) = cfg.accounts[1] else {
            panic!("expected m365 account");
        };
        assert_eq!(m.oauth.authority, "https://login.microsoftonline.com/common");

        let AccountConfig::Matrix(ref mx) = cfg.accounts[2] else {
            panic!("expected matrix account");
        };
        assert_eq!(mx.mxid, "@alice:example.org");
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
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.server.bind, "127.0.0.1:8443");
        assert_eq!(cfg.server.basic_auth.user, "alice");
        assert!(cfg.accounts.is_empty());
        assert!(cfg.data.is_none());
        assert!(cfg.master_key_file.is_none());
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
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.server.bind, "0.0.0.0:8443");
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
accounts:
  - id: bad
    kind: exchange
    address: user@corp.com
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(
            err.to_string().contains("exchange")
                || format!("{err:#}").contains("exchange"),
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
accounts:
  - id: alice-gmail
    kind: gmail
    address: alice@gmail.com
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        assert!(Config::load(f.path()).is_err());
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
                tls: TlsConfig {
                    cert: "/tmp/cert.pem".into(),
                    key: "/tmp/key.pem".into(),
                },
                basic_auth: BasicAuthConfig {
                    user: "alice".into(),
                    password_bcrypt: "hash".into(),
                },
            },
            data: None,
            master_key_file: None,
            accounts: vec![],
        }
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
        new.server.tls.cert = "/tmp/cert2.pem".into();
        assert_eq!(
            diff_config_sections(&old, &new),
            vec![ConfigSection::Tls]
        );
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
        assert_eq!(
            diff_config_sections(&old, &new),
            vec![ConfigSection::Bind]
        );
    }

    #[test]
    fn diff_detects_accounts_change() {
        let old = base_config();
        let mut new = old.clone();
        new.accounts.push(AccountConfig::Matrix(MatrixAccountConfig {
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
}
