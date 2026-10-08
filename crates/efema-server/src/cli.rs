use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The efema relay: keeps sealed batches in order, by cursor, for the devices
/// that sync through it
#[derive(Parser)]
#[command(name = "efema-server", version, about, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Serve the relay over HTTP until stopped
    Serve(ServeArgs),
    /// List the streams a relay keeps, or the entries of one - positions,
    /// epochs and sizes, never contents
    Streams(StreamsArgs),
}

#[derive(Args)]
pub struct ServeArgs {
    /// The address to listen on. Loopback by default: put the relay behind a
    /// reverse proxy that terminates TLS before letting anything else reach it
    #[arg(long, env = "EFEMA_LISTEN", default_value = "127.0.0.1:2451")]
    pub listen: SocketAddr,

    #[command(flatten)]
    pub data: DataArgs,
}

#[derive(Args)]
pub struct StreamsArgs {
    /// Show the entries of this stream instead of the list of streams
    pub name: Option<String>,

    #[command(flatten)]
    pub data: DataArgs,
}

#[derive(Args)]
pub struct DataArgs {
    /// The directory the relay keeps its database in [default: the user's
    /// local data directory, under `efema`]
    #[arg(long, env = "EFEMA_DATA", value_name = "DIR")]
    pub data: Option<PathBuf>,
}

impl DataArgs {
    /// The data directory: the one given, or the platform's local data
    /// directory - `~/.local/share/efema` on Linux, `%LOCALAPPDATA%\efema` on
    /// Windows, `~/Library/Application Support/efema` on macOS.
    pub fn dir(&self) -> anyhow::Result<PathBuf> {
        match &self.data {
            Some(dir) => Ok(dir.clone()),
            None => dirs::data_local_dir()
                .map(|base| base.join("efema"))
                .ok_or_else(|| anyhow::anyhow!("this system has no local data directory; pass --data")),
        }
    }
}
