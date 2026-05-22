use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    pub tls: TlsConfig,
    pub basic_auth: BasicAuthConfig,
}

fn default_bind() -> String {
    "0.0.0.0:8443".to_string()
}

#[derive(Debug, Deserialize)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Deserialize)]
pub struct BasicAuthConfig {
    pub user: String,
    pub password_bcrypt: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_minimal_config() {
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
    }

    #[test]
    fn default_bind() {
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
}
