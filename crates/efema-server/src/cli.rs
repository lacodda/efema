use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use efema_server::{Access, Network, SpaceName, limits};

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
    /// List, hand out and revoke the access keys that open the relay's spaces
    Keys(KeysArgs),
}

#[derive(Args)]
pub struct ServeArgs {
    /// The address to listen on. Loopback by default: put the relay behind a
    /// reverse proxy that terminates TLS before letting anything else reach it
    #[arg(long, env = "EFEMA_LISTEN", default_value = "127.0.0.1:2451")]
    pub listen: SocketAddr,

    /// An address or network (10.0.0.2, 172.18.0.0/16) to trust as the
    /// reverse proxy, besides loopback - a proxy in another container or on
    /// another host. Repeat for more than one
    #[arg(long = "trust-proxy", env = "EFEMA_TRUST_PROXY", value_name = "ADDRESS", value_delimiter = ',')]
    pub trust_proxy: Vec<Network>,

    /// The largest request body - a batch - in bytes, or with KiB, MiB or GiB
    /// [default and most: 16MiB]
    #[arg(long = "max-body", env = "EFEMA_MAX_BODY", value_name = "SIZE", value_parser = size)]
    pub max_body: Option<u64>,

    /// The largest entry [default: 8MiB]
    #[arg(long = "max-entry", env = "EFEMA_MAX_ENTRY", value_name = "SIZE", value_parser = size)]
    pub max_entry: Option<u64>,

    /// Bytes of entry data one space may hold, all its streams together
    /// [default: 8GiB]
    #[arg(long = "space-quota", env = "EFEMA_SPACE_QUOTA", value_name = "SIZE", value_parser = size)]
    pub space_quota: Option<u64>,

    /// Requests one access key may make in a minute
    #[arg(long, env = "EFEMA_RATE", value_name = "PER_MINUTE", default_value_t = limits::RATE_DEFAULT)]
    pub rate: u32,

    #[command(flatten)]
    pub data: DataArgs,
}

#[derive(Args)]
pub struct StreamsArgs {
    /// Show the entries of this stream, as `<space>/<name>`, instead of the
    /// list of streams
    #[arg(value_name = "SPACE/NAME")]
    pub stream: Option<String>,

    #[command(flatten)]
    pub data: DataArgs,
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct KeysArgs {
    #[command(subcommand)]
    pub action: Option<KeysAction>,

    #[command(flatten)]
    pub data: DataArgs,
}

#[derive(Subcommand)]
pub enum KeysAction {
    /// Hand out a new key to a space, making the space the first time. The
    /// key is printed once, on standard output
    Add {
        /// The space the key opens
        space: SpaceName,
        /// `read`: read the space's streams. `write`: read and write them
        access: Access,
        /// A note to tell the key by in the list - the device it is for, say
        #[arg(long, default_value = "")]
        label: String,

        #[command(flatten)]
        data: DataArgs,
    },
    /// Revoke a key: it opens nothing from its next request on, on a running
    /// relay too
    Revoke {
        /// The key's id, as the list shows it
        id: String,

        #[command(flatten)]
        data: DataArgs,
    },
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

/// A size in bytes: a number, or a number and `KiB`, `MiB` or `GiB`.
fn size(text: &str) -> Result<u64, String> {
    let invalid = || format!("`{text}` is not a size: write it in bytes, or as 512KiB, 16MiB, 8GiB");
    let (digits, unit) = text.find(|c: char| !c.is_ascii_digit()).map_or((text, ""), |at| text.split_at(at));
    let scale: u64 = match unit {
        "" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        _ => return Err(invalid()),
    };
    if digits.is_empty() {
        return Err(invalid());
    }
    digits.parse::<u64>().ok().and_then(|n| n.checked_mul(scale)).ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_bytes_and_binary_units() {
        assert_eq!(size("1024"), Ok(1024));
        assert_eq!(size("512KiB"), Ok(512 * 1024));
        assert_eq!(size("16MiB"), Ok(16 * 1024 * 1024));
        assert_eq!(size("8GiB"), Ok(8 << 30));
        for bad in ["", "MiB", "16MB", "16 MiB", "-1", "1.5GiB", "99999999999999999999GiB"] {
            assert!(size(bad).is_err(), "{bad}");
        }
    }
}
