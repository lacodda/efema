//! The efema relay.
//!
//! A relay keeps streams of entries for the devices that sync through it. A
//! device appends a batch; the relay gives its entries the next positions,
//! links them into the stream's hash chain and stores them; other devices read
//! what came after their cursor, or wait until there is something to read. The
//! entries are opaque: the relay stores the bytes it was given and returns
//! them, and never decodes, merges or judges them.
//!
//! The binary, `efema-server`, is the supported way to run a relay. This
//! library is the same relay as code - for the binary itself, for tests that
//! talk to a real relay over a real socket, and for programs that embed one.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let relay = efema_server::Relay::open("relay-data".as_ref(), efema_server::Config::default())?;
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:2451").await?;
//! relay.serve(listener, async { tokio::signal::ctrl_c().await.ok(); }).await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::fs::{self, File, TryLockError};
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;
use tokio::sync::watch;

mod door;
mod heads;
mod http;
mod rate;
mod space;
pub mod store;

pub use door::{InvalidNetwork, Network};
pub use space::{Access, InvalidSpace, SpaceName};
pub use store::DATABASE_FILE;

/// The protocol's ceilings, and the defaults of a relay below them.
pub use efema_proto::limits;

/// What the relay holds each access key to. Each is at most the protocol's
/// ceiling, and announced to the key at `GET /v1/limits`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The largest request body - a batch, as it comes - in bytes.
    pub body: usize,
    /// The largest entry, in bytes.
    pub entry: usize,
    /// Bytes of entry data one space may hold, all its streams together.
    pub quota: u64,
    /// Requests one access key may make in a minute; as many may come at
    /// once.
    pub rate: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            body: limits::MAX_BODY_BYTES,
            entry: limits::ENTRY_BYTES_DEFAULT,
            quota: limits::SPACE_QUOTA_DEFAULT,
            rate: limits::RATE_DEFAULT,
        }
    }
}

impl Limits {
    /// Why these limits cannot be a relay's, if they cannot.
    fn check(&self) -> Result<(), String> {
        if self.body == 0 || self.body > limits::MAX_BODY_BYTES {
            return Err(format!("the body limit is from 1 to {} bytes, not {}", limits::MAX_BODY_BYTES, self.body));
        }
        if self.entry == 0 || self.entry > self.body {
            return Err(format!(
                "the entry limit is from 1 byte to the body limit ({}), not {}",
                self.body, self.entry
            ));
        }
        if self.rate == 0 {
            return Err("the rate is at least one request a minute".into());
        }
        Ok(())
    }
}

/// How a relay is set up.
#[derive(Clone, Debug)]
pub struct Config {
    /// What it holds each key to.
    pub limits: Limits,
    /// The peers it trusts to be its own machine or the proxy in front of
    /// it: loopback by default. Anything else is answered `https_required`.
    pub trusted: Vec<Network>,
}

impl Default for Config {
    fn default() -> Self {
        Self { limits: Limits::default(), trusted: Network::LOOPBACK.to_vec() }
    }
}

/// The lock file that says a relay is serving this data directory.
pub const LOCK_FILE: &str = "efema-server.lock";

/// What can stop a relay from opening.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("cannot create the data directory {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot lock {path}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("another efema-server is already serving {0}")]
    Busy(PathBuf),
    #[error(transparent)]
    Store(#[from] store::OpenError),
    #[error("{0}")]
    Config(String),
}

/// What every request handler shares.
pub(crate) struct Shared {
    store: store::Store,
    heads: heads::Heads,
    shutdown: watch::Receiver<bool>,
    config: Config,
    rates: rate::Rates,
    /// When each key's use was last written down: at most once a minute, so
    /// a device that syncs does not make every read a write.
    seen: Mutex<HashMap<[u8; 32], i64>>,
    /// When the relay started, in seconds since the Unix epoch.
    started: i64,
}

/// A relay over one data directory, ready to serve.
pub struct Relay {
    shared: Arc<Shared>,
    shutdown: watch::Sender<bool>,
    data_dir: PathBuf,
    // Held for the relay's lifetime: two processes serving one directory
    // would each keep their own waiters, and neither would wake the other's.
    _lock: File,
}

impl Relay {
    /// Opens the relay in `data_dir`, creating the directory and the database
    /// if they do not exist yet, set up as `config` says.
    ///
    /// # Errors
    ///
    /// When a limit is outside what the protocol allows, when the directory
    /// cannot be created or locked, when another relay is serving it already,
    /// or when the database cannot be opened.
    pub fn open(data_dir: &Path, config: Config) -> Result<Self, OpenError> {
        config.limits.check().map_err(OpenError::Config)?;
        fs::create_dir_all(data_dir).map_err(|source| OpenError::CreateDir { path: data_dir.to_path_buf(), source })?;
        let lock_path = data_dir.join(LOCK_FILE);
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|source| OpenError::Lock { path: lock_path.clone(), source })?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(OpenError::Busy(data_dir.to_path_buf())),
            Err(TryLockError::Error(source)) => return Err(OpenError::Lock { path: lock_path, source }),
        }

        let store = store::Store::open(data_dir)?;
        let (shutdown, stopping) = watch::channel(false);
        let rates = rate::Rates::new(config.limits.rate);
        Ok(Self {
            shared: Arc::new(Shared {
                store,
                heads: heads::Heads::default(),
                shutdown: stopping,
                config,
                rates,
                seen: Mutex::new(HashMap::new()),
                started: store::unix_now(),
            }),
            shutdown,
            data_dir: data_dir.to_path_buf(),
            _lock: lock,
        })
    }

    /// The directory this relay keeps its data in.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Serves the relay on `listener` until `shutdown` completes.
    ///
    /// On shutdown, requests in flight are allowed to finish and waits are
    /// answered at once with what is known, so stopping a relay never leaves a
    /// client hanging for a minute.
    ///
    /// # Errors
    ///
    /// When accepting connections fails.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let router = http::router(self.shared.clone());
        let stop = self.shutdown.clone();
        // The peer's address decides whether the relay answers at all.
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async move {
                shutdown.await;
                let _ = stop.send(true);
            })
            .await
    }
}
