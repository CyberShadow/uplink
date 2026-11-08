use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};
use uplink_config::{AccountConfig, Config, M365AccountConfig};
use uplink_email::{gmail::oauth as gmail_oauth, m365::oauth as m365_oauth};
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
    /// Run the server.
    Serve,
    /// Print a new master key, for `master_key_file`.
    Keygen,
    /// Print the bcrypt hash of a password read from standard input, for
    /// `server.basic_auth.password_bcrypt`.
    HashPassword,
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "uplink=info,uplink_server=info,uplink_email=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => uplink_server::serve(cli.config).await,
        Command::Keygen => {
            println!("{}", MasterKey::generate());
            Ok(())
        }
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
        Command::Oauth {
            command: OauthCommand::Init { account },
        } => oauth_init(&cli.config, &account).await,
        Command::Matrix {
            command: MatrixCommand::Pair { account },
        } => matrix_pair(&cli.config, &account).await,
    }
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
    let key = MasterKey::load(
        config
            .master_key_file
            .as_ref()
            .context("master_key_file is required with Matrix accounts")?,
    )?;
    println!(
        "Pairing {} as {} on {}.",
        account.id, account.mxid, account.homeserver
    );
    let mut secrets = Secrets::new();
    let password = secrets.read("Password: ")?;
    let device_name = format!("uplink ({})", account.id);
    let client = client::login(account, &key, &password, &device_name).await?;
    println!(
        "Logged in as device {}; the session is sealed in {}.",
        client.device_id().context("no device id")?,
        client::session_path(account).display()
    );
    client::upload_keys(&client).await?;
    println!(
        "To verify the device, so that your other devices share keys with it, enter \
         the account's recovery key or Secret Storage passphrase. Leave it empty to skip."
    );
    let recovery = secrets.read("Recovery key or passphrase: ")?;
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

/// Secrets typed at a terminal without echo, or else read from standard
/// input a line each, for scripts.
struct Secrets(Option<std::io::Lines<std::io::StdinLock<'static>>>);

impl Secrets {
    fn new() -> Self {
        use std::io::{BufRead, IsTerminal};
        let stdin = std::io::stdin();
        Self((!stdin.is_terminal()).then(|| stdin.lock().lines()))
    }

    fn read(&mut self, prompt: &str) -> Result<String> {
        match &mut self.0 {
            None => Ok(rpassword::prompt_password(prompt)?),
            Some(lines) => Ok(lines
                .next()
                .with_context(|| format!("standard input ended before: {prompt}"))??),
        }
    }
}

async fn oauth_init(config_path: &std::path::Path, id: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    let account = config
        .accounts
        .iter()
        .find(|account| account.id() == id)
        .with_context(|| format!("no account {id} in {}", config_path.display()))?;
    let key = MasterKey::load(
        config
            .master_key_file
            .as_ref()
            .context("master_key_file is required with OAuth accounts")?,
    )?;
    let token_file = match account {
        AccountConfig::Gmail(account) => {
            gmail_oauth::authorize(
                account,
                &key,
                |url| {
                    println!(
                        "Open this URL in a browser and sign in as {}:\n\n{url}\n",
                        account.address
                    );
                    println!(
                        "If the browser is on another machine, paste the URL it is redirected to here."
                    );
                },
                gmail_oauth::read_pasted_url(),
            )
            .await?;
            &account.oauth.token_file
        }
        AccountConfig::M365(account) => {
            m365_init(account, &key).await?;
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

/// The Outlook client's redirect URI is a URN no browser follows, so the
/// user copies the code out of it (SPEC §13.3).
async fn m365_init(account: &M365AccountConfig, key: &MasterKey) -> Result<()> {
    println!(
        "Open this URL in a browser and sign in as {}:\n\n{}\n",
        account.address,
        m365_oauth::authorize_url(account)?
    );
    println!(
        "The browser then fails to open a URL starting {}?code=…, or shows a\n\
         page with the code. Paste that URL, the page's text with the code, or\n\
         the code itself here (the browser's developer tools show the URL, in\n\
         the Network tab, as the last request's Location).",
        m365_oauth::REDIRECT_URI
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let code = loop {
        let line = lines
            .next_line()
            .await?
            .context("standard input closed before a code was pasted")?;
        match m365_oauth::pasted_code(&line) {
            Ok(code) => break code,
            Err(e) => eprintln!("{e:#}; try again."),
        }
    };
    m365_oauth::redeem(&reqwest::Client::new(), account, key, &code).await
}
