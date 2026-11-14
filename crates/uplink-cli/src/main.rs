use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use uplink_config::{AccountConfig, Config, Login};
use uplink_email::{
    gmail::oauth as gmail_oauth, m365::oauth as m365_oauth, mail_server::sign_in, oauth,
};
use uplink_matrix::client;
use uplink_storage::seal::MasterKey;

/// uplink — self-hosted personal communication hub.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The configuration file.
    #[arg(long, global = true, default_value = "uplink.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a configuration to start from (at `--config`), asking for the
    /// web UI's user and password, and make its data directory.
    Init,
    /// Run the server.
    Serve,
    /// Print the bcrypt hash of a password read from standard input, for
    /// `server.basic_auth.password_bcrypt`.
    HashPassword,
    /// Set the password an IMAP, POP3 or JMAP account signs in to its servers with,
    /// sealing it in the data directory; read from the terminal, or from
    /// standard input.
    Password {
        /// The account's id in the configuration.
        #[arg(long)]
        account: String,
    },
    /// Manage accounts' OAuth authorization.
    Oauth {
        #[command(subcommand)]
        command: OauthCommand,
    },
    /// Manage Matrix accounts.
    Matrix {
        #[command(subcommand)]
        command: MatrixCommand,
    },
}

#[derive(Subcommand)]
enum MatrixCommand {
    /// Log in as a new device of the account, and optionally verify it
    /// with the account's recovery key (SPEC §9.4).
    Pair {
        /// The account's id in the configuration.
        #[arg(long)]
        account: String,
    },
}

#[derive(Subcommand)]
enum OauthCommand {
    /// Authorize uplink to access an account, storing its refresh token.
    Init {
        /// The account's id in the configuration.
        #[arg(long)]
        account: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // By default, what uplink's crates say at info, and the migrations
    // refinery applies to the database (a slow one holds up startup).
    const DEFAULT_FILTER: &str = "uplink=info,uplink_calendar=info,uplink_config=info,\
        uplink_email=info,uplink_matrix=info,uplink_protocol=info,uplink_realtime=info,\
        uplink_server=info,uplink_storage=info,uplink_todos=info,uplink_tx_log=info,\
        refinery_core=info";
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_FILTER.into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Init => init(&cli.config),
        Command::Serve => uplink_server::serve(cli.config).await,
        Command::HashPassword => {
            let mut password = String::new();
            std::io::stdin()
                .read_line(&mut password)
                .context("reading the password")?;
            let password = password.trim_end_matches(['\r', '\n']);
            anyhow::ensure!(!password.is_empty(), "no password on standard input");
            println!("{}", bcrypt::hash(password, 12)?);
            Ok(())
        }
        Command::Password { account } => set_password(&cli.config, &account),
        Command::Oauth {
            command: OauthCommand::Init { account },
        } => oauth_init(&cli.config, &account).await,
        Command::Matrix {
            command: MatrixCommand::Pair { account },
        } => matrix_pair(&cli.config, &account).await,
    }
}

fn set_password(config_path: &std::path::Path, id: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    let account = config
        .accounts
        .iter()
        .find_map(|account| match account {
            AccountConfig::Imap(imap) if imap.id == id => Some(imap.login()),
            AccountConfig::Pop3(pop3) if pop3.id == id => Some(pop3.login()),
            AccountConfig::Jmap(jmap) if jmap.id == id => Some(jmap.login()),
            _ => None,
        })
        .with_context(|| {
            format!(
                "no IMAP, POP3 or JMAP account {id} in {}",
                config_path.display()
            )
        })?;
    anyhow::ensure!(
        account.oauth.is_none(),
        "{id} signs in with OAuth: authorize it with `uplink oauth init --account {id}`"
    );
    let key = MasterKey::open(&config.master_key_file())?;
    let password = Prompt::new().secret(&format!("Password of {} ({id}): ", account.user))?;
    anyhow::ensure!(!password.is_empty(), "no password given");
    sign_in::store_password(&key, account, &password)?;
    println!(
        "The password is sealed in {}; a running uplink signs in with it from its next attempt.",
        account.password_file.display()
    );
    Ok(())
}

async fn matrix_pair(config_path: &std::path::Path, id: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    let account = config
        .accounts
        .iter()
        .find_map(|account| match account {
            AccountConfig::Matrix(matrix) if matrix.id == id => Some(matrix),
            _ => None,
        })
        .with_context(|| format!("no Matrix account {id} in {}", config_path.display()))?;
    let key = MasterKey::open(&config.master_key_file())?;
    println!(
        "Pairing {} as {} on {}.",
        account.id, account.mxid, account.homeserver
    );
    let mut prompt = Prompt::new();
    let password = prompt.secret("Password: ")?;
    let device_name = format!("uplink ({})", account.id);
    let client = client::pair(account, &key, &password, &device_name).await?;
    println!(
        "Logged in as device {}; the session is sealed in {}.",
        client.device_id().context("no device id")?,
        client::session_path(account).display()
    );
    println!(
        "To verify the device, so that your other devices share keys with it, enter \
         the account's recovery key or Secret Storage passphrase. Leave it empty to skip."
    );
    let recovery = prompt.secret("Recovery key or passphrase: ")?;
    if recovery.trim().is_empty() {
        println!("Skipped: the device is not verified.");
    } else if client::import_secrets(&client, recovery.trim()).await? {
        println!("Verified: the device is signed by your cross-signing identity.");
    } else {
        anyhow::bail!("imported the secrets, but the device is still not verified");
    }
    println!("Paired. A running uplink starts syncing the account within a minute.");
    Ok(())
}

/// Answers typed at a terminal (secrets without echo), or else read from
/// standard input a line each, for scripts.
struct Prompt(Option<std::io::Lines<std::io::StdinLock<'static>>>);

impl Prompt {
    fn new() -> Self {
        use std::io::{BufRead, IsTerminal};
        let stdin = std::io::stdin();
        Self((!stdin.is_terminal()).then(|| stdin.lock().lines()))
    }

    fn secret(&mut self, prompt: &str) -> Result<String> {
        match &mut self.0 {
            None => Ok(rpassword::prompt_password(prompt)?),
            Some(_) => self.piped(prompt),
        }
    }

    fn line(&mut self, prompt: &str) -> Result<String> {
        match &mut self.0 {
            None => {
                use std::io::Write;
                print!("{prompt}");
                std::io::stdout().flush()?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                Ok(line.trim_end_matches(['\r', '\n']).to_owned())
            }
            Some(_) => self.piped(prompt),
        }
    }

    fn piped(&mut self, prompt: &str) -> Result<String> {
        let lines = self.0.as_mut().expect("piped");
        Ok(lines
            .next()
            .with_context(|| format!("standard input ended before: {prompt}"))??)
    }
}

/// The configuration `uplink init` writes: the example's, with the web
/// UI's user and password hash.
fn initial_config(user: &str, password_bcrypt: &str) -> String {
    let template = include_str!("../../../examples/uplink.yaml");
    let quoted = |s: &str| serde_json::to_string(s).expect("serializes");
    [
        ("    user: alice\n", format!("    user: {}\n", quoted(user))),
        (
            "    # bcrypt hash of \"uplink\": a well-known password, for trying uplink on\n\
             \x20   # loopback only. `uplink init` writes your own instead; make one by\n\
             \x20   # hand with: uplink hash-password\n",
            "    # bcrypt hash of the password. Make another with: uplink hash-password\n".into(),
        ),
        (
            "    password_bcrypt: \"$2b$12$Z5QelJuvdiqVHbPGIlx/d.zNmaHFLs5vXr41y3ucCcBkndL6tErju\"\n",
            format!("    password_bcrypt: {}\n", quoted(password_bcrypt)),
        ),
    ]
    .into_iter()
    .fold(template.to_owned(), |text, (from, to)| {
        assert!(text.contains(from), "the example has {from:?}");
        text.replace(from, &to)
    })
}

/// Writes a configuration to start from at `config_path`, unless one is
/// there (an empty file is not one), and makes its data directory with
/// the master key.
fn init(config_path: &std::path::Path) -> Result<()> {
    anyhow::ensure!(
        std::fs::read_to_string(config_path).map_or(true, |text| text.trim().is_empty()),
        "{} exists; edit it, or remove it first",
        config_path.display()
    );
    let mut prompt = Prompt::new();
    let default_user = std::env::var("USER").unwrap_or_default();
    let user = match prompt.line(&format!("User name for the web UI [{default_user}]: "))? {
        user if user.is_empty() => default_user,
        user => user,
    };
    anyhow::ensure!(!user.is_empty(), "no user name");
    let password = prompt.secret("Password: ")?;
    anyhow::ensure!(!password.is_empty(), "no password");
    anyhow::ensure!(
        prompt.secret("The password again: ")? == password,
        "the passwords differ"
    );
    std::fs::write(
        config_path,
        initial_config(&user, &bcrypt::hash(&password, 12)?),
    )
    .with_context(|| format!("writing {}", config_path.display()))?;
    let config = Config::load(config_path)?;
    MasterKey::open(&config.master_key_file())?;
    println!(
        "Wrote {}, and made {} with its master key.\n\
         Add accounts to it (see its comments), authorize each with `uplink oauth init` or\n\
         `uplink matrix pair`, and start the server with `uplink serve`: it listens on\n\
         http://{}/.",
        config_path.display(),
        config.data_dir.display(),
        config.server.bind,
    );
    Ok(())
}

async fn oauth_init(config_path: &std::path::Path, id: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    let account = config
        .accounts
        .iter()
        .find(|account| account.id() == id)
        .with_context(|| format!("no account {id} in {}", config_path.display()))?;
    let key = MasterKey::open(&config.master_key_file())?;
    let token_file = match account {
        AccountConfig::Gmail(account) => {
            gmail_oauth::authorize(
                account,
                &key,
                present_authorization(&account.address),
                oauth::read_pasted_url(),
            )
            .await?;
            &account.oauth.token_file
        }
        AccountConfig::Imap(account) => mail_server_init(account.login(), &key).await?,
        AccountConfig::Pop3(account) => mail_server_init(account.login(), &key).await?,
        AccountConfig::Jmap(account) => mail_server_init(account.login(), &key).await?,
        AccountConfig::M365(account) => {
            m365_oauth::authorize(
                account,
                &key,
                present_authorization(&account.address),
                oauth::read_pasted_url(),
            )
            .await?;
            &account.oauth.token_file
        }
        AccountConfig::Matrix(_) => {
            anyhow::bail!("{id} is a Matrix account: pair it with `uplink matrix pair {id}`")
        }
    };
    println!(
        "Authorized; the token is stored in {}.",
        token_file.display()
    );
    Ok(())
}

/// An IMAP, POP3 or JMAP account's authorization; returns where its token is.
async fn mail_server_init<'a>(account: Login<'a>, key: &MasterKey) -> Result<&'a Path> {
    sign_in::authorize(
        account,
        key,
        present_authorization(account.address),
        oauth::read_pasted_url(),
    )
    .await?;
    Ok(&account
        .oauth
        .expect("authorize refuses accounts without OAuth")
        .token_file)
}

/// Shows the user where to sign in as `address`; the browser is then
/// redirected to a listener of the flow's, or, from another machine, to
/// a page that does not load, whose URL the user pastes.
fn present_authorization(address: &str) -> impl FnOnce(&reqwest::Url) + '_ {
    move |url| {
        println!("Open this URL in a browser and sign in as {address}:\n\n{url}\n");
        println!(
            "If the browser is on another machine, it then fails to load a page at\n\
             http://127.0.0.1:…; paste that page's address here."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initial_config_has_the_user_and_password() {
        let dir = std::env::temp_dir().join(format!("uplink-init-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uplink.yaml");
        let hash = bcrypt::hash("secret", 4).unwrap();
        std::fs::write(&path, initial_config("o'neil \"x\"", &hash)).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.server.basic_auth.user, "o'neil \"x\"");
        assert!(bcrypt::verify("secret", &config.server.basic_auth.password_bcrypt).unwrap());
        assert_eq!(config.server.tls, None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
