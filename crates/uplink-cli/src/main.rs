use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use uplink_config::{AccountConfig, Config};
use uplink_email::gmail::oauth;
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
        .find_map(|account| match account {
            AccountConfig::Gmail(gmail) if gmail.id == id => Some(gmail),
            _ => None,
        })
        .with_context(|| format!("no Gmail account {id} in {}", config_path.display()))?;
    let key = MasterKey::load(
        config
            .master_key_file
            .as_ref()
            .context("master_key_file is required with OAuth accounts")?,
    )?;
    oauth::authorize(
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
        oauth::read_pasted_url(),
    )
    .await?;
    println!(
        "Authorized; the token is stored in {}.",
        account.oauth.token_file.display()
    );
    Ok(())
}
