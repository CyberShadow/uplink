use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use uplink_config::{AccountConfig, Config};
use uplink_email::{gmail::oauth, seal::MasterKey};

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
    /// Manage accounts' OAuth authorization.
    Oauth {
        #[command(subcommand)]
        command: OauthCommand,
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
        Command::Oauth {
            command: OauthCommand::Init { account },
        } => oauth_init(&cli.config, &account).await,
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
