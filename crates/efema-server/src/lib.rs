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
//! let relay = efema_server::Relay::open("relay-data".as_ref())?;
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:2451").await?;
//! relay.serve(listener, async { tokio::signal::ctrl_c().await.ok(); }).await?;
//! # Ok(())
//! # }
//! ```

use std::fs::{self, File, TryLockError};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;

mod heads;
mod http;
pub mod store;

pub use store::DATABASE_FILE;

/// What the relay accepts and returns in one request: the limits of protocol
/// version 1, fixed in this release. The relay will announce them (and take
/// them from its configuration) once there is more than one sensible value.
pub use efema_proto::limits;

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
}

/// What every request handler shares.
pub(crate) struct Shared {
    store: store::Store,
    heads: heads::Heads,
    shutdown: watch::Receiver<bool>,
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
    /// if they do not exist yet.
    ///
    /// # Errors
    ///
    /// When the directory cannot be created or locked, when another relay is
    /// serving it already, or when the database cannot be opened.
    pub fn open(data_dir: &Path) -> Result<Self, OpenError> {
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
        Ok(Self {
            shared: Arc::new(Shared { store, heads: heads::Heads::default(), shutdown: stopping }),
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
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                shutdown.await;
                let _ = stop.send(true);
            })
            .await
    }
}
