//! `mini-vault`: keep a folder in sync with other devices through an efema
//! relay, or through an S3 bucket.
//!
//! ```console
//! $ export MINI_VAULT_KEY="$(efema-server keys add notes write)"
//! $ export MINI_VAULT_PASSPHRASE='a long passphrase, not this one'
//! $ mini-vault --relay http://127.0.0.1:2451 --folder ./notes --state ./notes-state sync
//! $ mini-vault --relay http://127.0.0.1:2451 --folder ./notes --state ./notes-state doctor
//! ```
//!
//! Through a bucket, the credentials come from `AWS_ACCESS_KEY_ID` and
//! `AWS_SECRET_ACCESS_KEY`, the region from `AWS_REGION` or `--region`:
//!
//! ```console
//! $ mini-vault --bucket https://notes.s3.eu-central-1.amazonaws.com --region eu-central-1 --folder ./notes --state ./notes-state sync
//! ```
//!
//! Moving the folder's stream from a relay to a bucket - each device then
//! syncs through the bucket with the state it has:
//!
//! ```console
//! $ mini-vault --relay http://127.0.0.1:2451 copy --to-bucket https://notes.s3.eu-central-1.amazonaws.com --to-region eu-central-1
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use efema::{AccessKey, Bucket, Client, Credentials, Epoch, Relay, Secret, State, StreamName, Transport, copy};
use mini_vault::{Summary, Vault};

/// Keep a folder in sync with other devices through an efema relay, or
/// through an S3 bucket.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The relay.
    #[arg(long, env = "MINI_VAULT_RELAY")]
    relay: Option<String>,
    /// The access key the relay's operator handed out for this folder's
    /// space.
    #[arg(long, env = "MINI_VAULT_KEY", hide_env_values = true)]
    key: Option<AccessKey>,
    /// An S3 bucket to sync through instead of a relay: its address. The
    /// credentials come from AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY.
    #[arg(long, env = "MINI_VAULT_BUCKET")]
    bucket: Option<String>,
    /// The bucket's region.
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,
    /// The prefix in the bucket the stream is under, if not at its root.
    #[arg(long, env = "MINI_VAULT_PREFIX")]
    prefix: Option<String>,
    /// The stream the folder syncs through.
    #[arg(long, default_value = "vault")]
    stream: StreamName,
    /// The folder to keep in sync - for `sync`.
    #[arg(long)]
    folder: Option<PathBuf>,
    /// Where this device keeps its sync state, outside the folder - for
    /// `sync` and `doctor`.
    #[arg(long)]
    state: Option<PathBuf>,
    /// The stream's passphrase - for `sync` and `doctor`.
    #[arg(long, env = "MINI_VAULT_PASSPHRASE", hide_env_values = true)]
    passphrase: Option<String>,
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
    /// Copy the stream to another relay or bucket, every batch at the
    /// position it has: each device then syncs through the copy with the
    /// state it has. Run again, it brings over what was written since.
    Copy {
        /// The relay to copy to.
        #[arg(long, env = "MINI_VAULT_TO_RELAY")]
        to_relay: Option<String>,
        /// The access key to the space there.
        #[arg(long, env = "MINI_VAULT_TO_KEY", hide_env_values = true)]
        to_key: Option<AccessKey>,
        /// The bucket to copy to, reached with AWS_ACCESS_KEY_ID and
        /// AWS_SECRET_ACCESS_KEY.
        #[arg(long, env = "MINI_VAULT_TO_BUCKET")]
        to_bucket: Option<String>,
        /// Its region, if not the one --region gives.
        #[arg(long)]
        to_region: Option<String>,
        /// The prefix in it.
        #[arg(long)]
        to_prefix: Option<String>,
    },
}

/// Where a stream is: a relay, or a bucket.
enum Remote {
    Relay(Relay),
    Bucket(Bucket),
}

impl Remote {
    fn new(
        relay: Option<String>,
        key: Option<AccessKey>,
        bucket: Option<String>,
        region: Option<String>,
        prefix: Option<String>,
    ) -> Result<Self> {
        match (relay, bucket) {
            (Some(url), None) => {
                let key = key.context("a relay needs an access key: MINI_VAULT_KEY or --key")?;
                Ok(Self::Relay(Relay::new(&url, key)?))
            }
            (None, Some(url)) => {
                let region = region.context("a bucket needs its region: AWS_REGION or --region")?;
                let id = std::env::var("AWS_ACCESS_KEY_ID").context("a bucket needs AWS_ACCESS_KEY_ID")?;
                let secret = std::env::var("AWS_SECRET_ACCESS_KEY").context("a bucket needs AWS_SECRET_ACCESS_KEY")?;
                let bucket = Bucket::new(&url, &region, Credentials::new(id, secret))?;
                Ok(Self::Bucket(match prefix {
                    Some(prefix) => bucket.within(&prefix)?,
                    None => bucket,
                }))
            }
            (Some(_), Some(_)) => bail!("a relay or a bucket, not both"),
            (None, None) => bail!("nowhere to reach the stream: a relay or a bucket"),
        }
    }
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
    let from =
        Remote::new(cli.relay.clone(), cli.key.clone(), cli.bucket.clone(), cli.region.clone(), cli.prefix.clone())
            .context("--relay (MINI_VAULT_RELAY) or --bucket (MINI_VAULT_BUCKET)")?;
    if let Command::Copy { to_relay, to_key, to_bucket, to_region, to_prefix } = &cli.command {
        let to = Remote::new(
            to_relay.clone(),
            to_key.clone(),
            to_bucket.clone(),
            to_region.clone().or_else(|| cli.region.clone()),
            to_prefix.clone(),
        )
        .context("--to-relay (MINI_VAULT_TO_RELAY) or --to-bucket (MINI_VAULT_TO_BUCKET)")?;
        return match from {
            Remote::Relay(from) => copy_to(&from, to, &cli.stream).await,
            Remote::Bucket(from) => copy_to(&from, to, &cli.stream).await,
        };
    }
    match from {
        Remote::Relay(relay) => through(relay, cli).await,
        Remote::Bucket(bucket) => through(bucket, cli).await,
    }
}

async fn copy_to<F: Transport>(from: &F, to: Remote, stream: &StreamName) -> Result<()> {
    let copied = match &to {
        Remote::Relay(to) => copy(from, to, stream).await?,
        Remote::Bucket(to) => copy(from, to, stream).await?,
    };
    let at = match to {
        Remote::Relay(to) => to.to_string(),
        Remote::Bucket(to) => to.to_string(),
    };
    println!(
        "copied {} batch(es), {} entries: {stream} is the same at {at} up to position {}",
        copied.batches, copied.entries, copied.cursor.seq
    );
    Ok(())
}

async fn through<T: Transport>(transport: T, cli: Cli) -> Result<()> {
    let state_dir = cli.state.context("--state: where this device keeps its sync state")?;
    let passphrase = cli.passphrase.context("the stream's passphrase: MINI_VAULT_PASSPHRASE or --passphrase")?;
    std::fs::create_dir_all(&state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    let folder = match &cli.command {
        Command::Sync { .. } => {
            let folder = cli.folder.clone().context("--folder: the folder to keep in sync")?;
            std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
            if inside(&state_dir, &folder)? {
                bail!("the state directory must be outside the folder it syncs");
            }
            Some(folder)
        }
        _ => None,
    };
    let state = State::open(state_dir.join("sync.sqlite3"))?;
    let client = Client::open(transport, &state, cli.stream, EPOCH, Secret::Passphrase(passphrase.as_bytes())).await?;

    match cli.command {
        Command::Doctor => {
            println!("{}", client.doctor().await);
            Ok(())
        }
        Command::Copy { .. } => unreachable!("a copy opens no client"),
        Command::Sync { once } => {
            let folder = folder.expect("a sync has its folder");
            let mut vault = Vault::open(client, &folder, &state_dir.join("index"))?;
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
                // Woken at once by a relay when another device writes - by a
                // bucket within a few seconds; the timeout is how often local
                // changes are looked for. A relay or a bucket that cannot be
                // reached is tried again after a pause.
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
    for file in &summary.too_large {
        eprintln!("not sent: {file}");
    }
}

fn inside(path: &Path, folder: &Path) -> Result<bool> {
    let path = path.canonicalize()?;
    let folder = folder.canonicalize()?;
    Ok(path.starts_with(folder))
}
