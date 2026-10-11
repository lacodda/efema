//! `mini-vault`: keep a folder in sync with other devices through an efema
//! relay.
//!
//! ```console
//! $ export MINI_VAULT_PASSPHRASE='a long passphrase, not this one'
//! $ mini-vault --relay http://127.0.0.1:2451 --folder ./notes --state ./notes-state sync
//! $ mini-vault --relay http://127.0.0.1:2451 --folder ./notes --state ./notes-state doctor
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use efema::{Client, Epoch, Relay, Secret, State, StreamName};
use mini_vault::{Summary, Vault};

/// Keep a folder in sync with other devices through an efema relay.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The relay.
    #[arg(long, env = "MINI_VAULT_RELAY")]
    relay: String,
    /// The stream the folder syncs through.
    #[arg(long, default_value = "vault")]
    stream: StreamName,
    /// The folder to keep in sync.
    #[arg(long)]
    folder: PathBuf,
    /// Where this device keeps its sync state - outside the folder.
    #[arg(long)]
    state: PathBuf,
    /// The stream's passphrase.
    #[arg(long, env = "MINI_VAULT_PASSPHRASE", hide_env_values = true)]
    passphrase: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sync now, then on every change until stopped.
    Sync {
        /// Sync once and exit.
        #[arg(long)]
        once: bool,
    },
    /// Report on this device's sync.
    Doctor,
}

/// The format of what this app writes - a [`mini_vault::Change`] in CBOR.
const EPOCH: Epoch = Epoch(1);

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    std::fs::create_dir_all(&cli.folder).with_context(|| format!("creating {}", cli.folder.display()))?;
    std::fs::create_dir_all(&cli.state).with_context(|| format!("creating {}", cli.state.display()))?;
    if inside(&cli.state, &cli.folder)? {
        bail!("the state directory must be outside the folder it syncs");
    }
    let state = State::open(cli.state.join("sync.sqlite3"))?;
    let relay = Relay::new(&cli.relay)?;
    let client = Client::open(relay, &state, cli.stream, EPOCH, Secret::Passphrase(cli.passphrase.as_bytes())).await?;

    match cli.command {
        Command::Doctor => {
            println!("{}", client.doctor().await);
            Ok(())
        }
        Command::Sync { once } => {
            let mut vault = Vault::open(client, &cli.folder, &cli.state.join("index"))?;
            loop {
                match vault.sync().await {
                    Ok(summary) => report(&summary),
                    Err(e) if once => return Err(e),
                    // The network comes and goes; what was not sent waits on
                    // this device, and the next round sends it.
                    Err(e) => eprintln!("sync stopped: {e:#} - trying again"),
                }
                if once {
                    return Ok(());
                }
                // Woken at once by the relay when another device writes; the
                // timeout is how often local changes are looked for. A relay
                // that cannot be reached is tried again after a pause.
                let pause = async {
                    if vault.client().wait(Duration::from_secs(5)).await.is_err() {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                };
                tokio::select! {
                    () = pause => {}
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
            }
        }
    }
}

fn report(summary: &Summary) {
    if summary.applied > 0 || summary.pushed > 0 {
        println!("applied {} change(s), sent {}", summary.applied, summary.pushed);
    }
    for copy in &summary.conflicts {
        println!("conflict: both versions kept, the other one as {copy}");
    }
    for refused in &summary.refused {
        eprintln!("skipped: {refused}");
    }
}

fn inside(path: &Path, folder: &Path) -> Result<bool> {
    let path = path.canonicalize()?;
    let folder = folder.canonicalize()?;
    Ok(path.starts_with(folder))
}
