//! `first-sync`: how long a vault takes to reach an empty relay, and to come
//! down whole to a second device - the first sync, timed and checked.
//!
//! ```console
//! $ cargo run --release -p mini-vault --example first-sync -- --limit 30
//! ```
//!
//! The way up is the relay's and the client's: reading, compressing, sealing,
//! keeping and sending every file, and the relay writing it. The way down is
//! mostly the app's: writing thousands of new files is the operating system's
//! slowest part of it, so the limit holds the way up.
//!
//! Without `--folder`, it syncs a made-up vault shaped like a working one:
//! 5080 files sized the way the notes and attachments of a real vault are -
//! half under 4 KB, one in a hundred over 240 KB, the largest a few MB - notes
//! as text, the largest files as bytes that do not compress. Without
//! `--relay` or `--bucket`, it starts a relay of its own in a temporary
//! folder. With `--relay`, the access key comes from `FIRST_SYNC_KEY`; with
//! `--bucket`, the bucket's credentials from `AWS_ACCESS_KEY_ID` and
//! `AWS_SECRET_ACCESS_KEY`, its region from `AWS_REGION`. Each run writes a
//! new stream with a name of its own.
//!
//! Each run seals with a passphrase of its own, random and never shown: what
//! it leaves on a relay can be opened by no one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use efema::{AccessKey, Bucket, Client, Credentials, Epoch, Relay, Secret, State, StreamName, Transport};
use mini_vault::Summary;
use mini_vault::{Vault, scan};

/// Time the first sync of a vault: up to an empty relay, then down to a
/// second device, and check the two folders hold the same bytes.
#[derive(Parser)]
#[command(name = "first-sync")]
struct Cli {
    /// How many files the made-up vault has.
    #[arg(long, default_value_t = 5080)]
    files: usize,
    /// The seed the made-up vault is drawn from.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// A folder to sync instead of a made-up vault. It is only read.
    #[arg(long, conflicts_with_all = ["files", "seed"])]
    folder: Option<PathBuf>,
    /// A relay to sync through, instead of one started for the run.
    #[arg(long, conflicts_with = "bucket")]
    relay: Option<String>,
    /// The access key to `--relay`.
    #[arg(long, env = "FIRST_SYNC_KEY", hide_env_values = true)]
    key: Option<AccessKey>,
    /// An S3 bucket to sync through, instead of a relay: its address.
    #[arg(long)]
    bucket: Option<String>,
    /// The bucket's region.
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,
    /// Fail when the way up - the whole vault to the empty relay - takes
    /// longer than this many seconds.
    #[arg(long)]
    limit: Option<f64>,
}

/// The format of what the vault writes, as mini-vault's own.
const EPOCH: Epoch = Epoch(1);

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<bool> {
    let work = tempfile::tempdir()?;
    let folder = match &cli.folder {
        Some(folder) => folder.clone(),
        None => {
            let folder = work.path().join("made-up");
            made_up(&folder, cli.files, cli.seed)?;
            folder
        }
    };
    let (count, bytes) = measure(&folder)?;
    let source = cli.folder.as_ref().map_or_else(|| format!("made up, seed {}", cli.seed), |f| f.display().to_string());
    println!("vault  {count} files, {} ({source})", size(bytes));

    let stream: StreamName = format!("first-sync-{}", hex(&random::<4>())).parse()?;
    let passphrase = hex(&random::<32>());
    let local;
    let (up_took, summary, down_folder) = match (cli.relay, cli.bucket) {
        (Some(url), None) => {
            let key = cli.key.context("--relay needs an access key: FIRST_SYNC_KEY or --key")?;
            println!("relay  {url}, stream {stream}");
            both_ways(|| Ok(Relay::new(&url, key.clone())?), work.path(), &folder, &stream, &passphrase).await?
        }
        (None, Some(url)) => {
            let region = cli.region.context("--bucket needs its region: AWS_REGION or --region")?;
            let credentials = credentials()?;
            println!("bucket {url}, stream {stream}");
            let bucket = || Ok(Bucket::new(&url, &region, credentials.clone())?);
            both_ways(bucket, work.path(), &folder, &stream, &passphrase).await?
        }
        _ => {
            local = LocalRelay::start(&work.path().join("relay")).await?;
            println!("relay  {}, stream {stream}", local.url);
            let relay = || Ok(Relay::new(&local.url, local.key.clone())?);
            both_ways(relay, work.path(), &folder, &stream, &passphrase).await?
        }
    };

    let mut expected = scan(&folder)?;
    for file in &summary.too_large {
        expected.remove(&file.path);
    }
    if scan(&down_folder)? != expected {
        bail!("the second device's folder does not hold what the first one sent");
    }
    println!("check  both folders hold the same bytes");

    let Some(limit) = cli.limit else {
        return Ok(true);
    };
    let within = up_took.as_secs_f64() <= limit;
    println!("limit  {limit} s up: {}", if within { "within" } else { "OVER" });
    Ok(within)
}

/// The first sync through what `transport` makes: up from `folder` to the
/// empty `stream`, then down to a second device's empty folder - how long
/// the way up took, what it sent, and where the way down landed.
async fn both_ways<T: Transport>(
    transport: impl Fn() -> Result<T>,
    work: &Path,
    folder: &Path,
    stream: &StreamName,
    passphrase: &str,
) -> Result<(Duration, Summary, PathBuf)> {
    // Up: the first device, from its own folder to the empty stream.
    let started = Instant::now();
    let up_state = State::open(work.join("up.sqlite3"))?;
    let client =
        Client::open(transport()?, &up_state, stream.clone(), EPOCH, Secret::Passphrase(passphrase.as_bytes())).await?;
    let opened = started.elapsed();
    let mut up = Vault::open(client, folder, &work.join("up-index"))?;
    let summary = up.sync().await?;
    let up_took = started.elapsed();
    println!("up     {} files in {} - the stream's key made in {}", summary.pushed, secs(up_took), secs(opened));
    for file in &summary.too_large {
        println!("       not sent: {file}");
    }

    // Down: a second device, from the stream to an empty folder.
    let started = Instant::now();
    let down_state = State::open(work.join("down.sqlite3"))?;
    let client =
        Client::open(transport()?, &down_state, stream.clone(), EPOCH, Secret::Passphrase(passphrase.as_bytes()))
            .await?;
    let opened = started.elapsed();
    let down_folder = work.join("down");
    fs::create_dir_all(&down_folder)?;
    let mut down = Vault::open(client, &down_folder, &work.join("down-index"))?;
    let summary_down = down.sync().await?;
    let down_took = started.elapsed();
    println!("down   {} files in {} - the key unlocked in {}", summary_down.applied, secs(down_took), secs(opened));
    Ok((up_took, summary, down_folder))
}

/// A bucket's credentials, from the variables every S3 tool reads.
fn credentials() -> Result<Credentials> {
    let id = std::env::var("AWS_ACCESS_KEY_ID").context("--bucket needs AWS_ACCESS_KEY_ID")?;
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY").context("--bucket needs AWS_SECRET_ACCESS_KEY")?;
    Ok(Credentials::new(id, secret))
}

/// A relay of the run's own, on this machine, in `dir`.
struct LocalRelay {
    url: String,
    key: AccessKey,
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl LocalRelay {
    async fn start(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let store = efema_server::store::Store::open(dir)?;
        let key = store.add_key(&"bench".parse()?, efema_server::Access::Write, "first-sync")?;
        drop(store);
        let relay = efema_server::Relay::open(dir, efema_server::Config::default())?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(relay.serve(listener, async move {
            let _ = stopped.await;
        }));
        Ok(Self { url, key, _stop: stop })
    }
}

/// The sizes of a working vault's files, as quantiles - measured on one of
/// some 6300 notes and attachments, files over the relay's entry limit left
/// out: (the share of files, the size at or under which that share falls).
const SIZES: [(f64, f64); 10] = [
    (0.0, 16.0),
    (0.1, 264.0),
    (0.25, 1_608.0),
    (0.5, 3_692.0),
    (0.75, 13_065.0),
    (0.9, 26_499.0),
    (0.95, 42_988.0),
    (0.99, 242_277.0),
    (0.999, 1_499_408.0),
    (1.0, 6_621_116.0),
];

/// Files larger than this are attachments - images, archives - and do not
/// compress; smaller ones are notes.
const ATTACHMENT: usize = 256 * 1024;

/// Writes a made-up vault of `files` files into `folder`, drawn from `seed`:
/// the same vault for the same seed.
fn made_up(folder: &Path, files: usize, seed: u64) -> Result<()> {
    let mut rng = Rng::new(seed);
    let words: Vec<String> = (0..2_000).map(|_| rng.word()).collect();
    // About four files to a folder, folders nested a few deep.
    let mut dirs = vec![folder.to_path_buf()];
    for i in 0..files.div_ceil(4) {
        let parent = dirs[rng.below(dirs.len())].clone();
        dirs.push(parent.join(format!("d{i:04}")));
    }
    for i in 0..files {
        let dir = &dirs[rng.below(dirs.len())];
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let len = rng.size();
        let (name, content) = if len > ATTACHMENT {
            (format!("asset-{i:05}.bin"), rng.bytes(len))
        } else {
            (format!("note-{i:05}.md"), rng.note(len, &words).into_bytes())
        };
        fs::write(dir.join(name), content)?;
    }
    Ok(())
}

/// A small, fast, seeded generator: the vault is made up, not secret.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// A file size, from the quantiles - straight between them on a log
    /// scale.
    fn size(&mut self) -> usize {
        let u = self.unit();
        let i = SIZES.iter().rposition(|(q, _)| *q <= u).unwrap_or(0).min(SIZES.len() - 2);
        let ((q0, s0), (q1, s1)) = (SIZES[i], SIZES[i + 1]);
        let t = (u - q0) / (q1 - q0);
        (s0.ln() + t * (s1.ln() - s0.ln())).exp() as usize
    }

    fn word(&mut self) -> String {
        const SYLLABLES: [&str; 16] =
            ["ra", "to", "mi", "ve", "lan", "sor", "ke", "du", "pli", "na", "ost", "ri", "ga", "tem", "il", "bo"];
        (0..1 + self.below(3)).map(|_| SYLLABLES[self.below(SYLLABLES.len())]).collect()
    }

    /// Text that reads to a compressor like notes do: common words often,
    /// rare ones seldom, headings, lists and links.
    fn note(&mut self, len: usize, words: &[String]) -> String {
        let mut text = String::with_capacity(len + 64);
        while text.len() < len {
            match self.below(10) {
                0 => text.push_str("## "),
                1 | 2 => text.push_str("- "),
                _ => {}
            }
            for _ in 0..4 + self.below(12) {
                let u = self.unit();
                let word = &words[(u * u * u * words.len() as f64) as usize];
                if self.below(20) == 0 {
                    text.push_str(&format!("[[{word}]] "));
                } else {
                    text.push_str(word);
                    text.push(' ');
                }
            }
            text.push('\n');
        }
        text.truncate(len);
        text
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(len + 8);
        while bytes.len() < len {
            bytes.extend_from_slice(&self.next().to_le_bytes());
        }
        bytes.truncate(len);
        bytes
    }
}

/// How many files `folder` has, and how many bytes.
fn measure(folder: &Path) -> Result<(usize, u64)> {
    let mut found = (0, 0);
    let mut dirs = vec![folder.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file() {
                found.0 += 1;
                found.1 += entry.metadata()?.len();
            }
        }
    }
    Ok(found)
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the operating system's random source");
    bytes
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn secs(took: Duration) -> String {
    format!("{:.1} s", took.as_secs_f64())
}

fn size(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
}
