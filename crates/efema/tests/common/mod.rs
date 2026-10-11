//! A real relay on a real socket for the client to talk to, a transport that
//! stands between them to play a relay that misbehaves, and - in front of the
//! relay - a proxy that tears connections the way a network does.

#![allow(dead_code)]

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use efema::wire::{Batch, Heads, Page, Watch, Written};
use efema::{Client, Cursor, Epoch, Limits, Relay, Secret, State, StreamName, Transport, TransportError};
use tempfile::TempDir;
use tokio::sync::{Barrier, oneshot};
use tokio::task::JoinHandle;

pub const PASSPHRASE: &[u8] = b"correct horse battery staple";
pub const NEW_PASSPHRASE: &[u8] = b"a longer and a better passphrase";

pub struct TestRelay {
    pub url: String,
    pub addr: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
    pub dir: TempDir,
}

impl TestRelay {
    pub async fn start() -> Self {
        Self::start_in(tempfile::tempdir().unwrap()).await
    }

    pub async fn start_in(dir: TempDir) -> Self {
        let relay = efema_server::Relay::open(dir.path()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(relay.serve(listener, async move {
            let _ = stopped.await;
        }));
        Self { url, addr, stop: Some(stop), task: Some(task), dir }
    }

    /// Stops the relay and hands back its directory.
    pub async fn stop(mut self) -> TempDir {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.unwrap().unwrap();
        }
        std::mem::replace(&mut self.dir, tempfile::tempdir().unwrap())
    }

    pub fn transport(&self) -> Relay {
        Relay::new(&self.url).unwrap()
    }
}

/// A device: a state file of its own.
pub struct Device {
    pub dir: TempDir,
    pub state: State,
}

impl Device {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(dir.path().join("sync.sqlite3")).unwrap();
        Self { dir, state }
    }

    /// The same device after a restart: its state, opened again.
    pub fn restart(self) -> Self {
        let Self { dir, state } = self;
        drop(state);
        let state = State::open(dir.path().join("sync.sqlite3")).unwrap();
        Self { dir, state }
    }

    pub async fn open<T: Transport>(&self, transport: T, stream: &str, epoch: u32) -> Client<T> {
        self.try_open(transport, stream, epoch, Secret::Passphrase(PASSPHRASE)).await.unwrap()
    }

    pub async fn try_open<T: Transport>(
        &self,
        transport: T,
        stream: &str,
        epoch: u32,
        secret: Secret<'_>,
    ) -> Result<Client<T>, efema::Error> {
        Client::open(transport, &self.state, name(stream), Epoch(epoch), secret).await
    }
}

pub fn name(text: &str) -> StreamName {
    text.parse().unwrap()
}

/// Pulls and acknowledges until the stream is read to its end; the data of
/// every entry, in order.
pub async fn drain<T: Transport>(client: &Client<T>) -> Vec<efema::Received> {
    let mut all = Vec::new();
    loop {
        let pulled = client.pull().await.unwrap();
        client.ack(&pulled).await.unwrap();
        let more = pulled.more();
        all.extend(pulled.entries);
        if !more {
            return all;
        }
    }
}

/// A push or a send that stopped with items kept: how many wait, and why it
/// stopped.
pub fn unsent(result: Result<efema::Sent, efema::Error>) -> (usize, efema::Error) {
    match result {
        Err(efema::Error::Unsent { unsent, source }) => (unsent, *source),
        other => panic!("expected items left waiting, got {other:?}"),
    }
}

pub fn data(received: &[efema::Received]) -> Vec<&[u8]> {
    received.iter().map(|r| r.data.as_slice()).collect()
}

/// Every byte the relay keeps on disk: its database and its write-ahead log.
pub fn relay_bytes(dir: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "lock") {
            bytes.extend(std::fs::read(&path).unwrap_or_default());
        }
    }
    bytes
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// Everything logged in this test process from the first call on, at every
/// level - the relay's log and the client's.
pub fn log() -> Arc<Mutex<Vec<u8>>> {
    static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let sink = buffer.clone();
        tracing_subscriber::fmt()
            .with_max_level(tracing_subscriber::filter::LevelFilter::TRACE)
            .with_ansi(false)
            .with_writer(move || Sink(sink.clone()))
            .init();
        buffer
    })
    .clone()
}

struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a man in the middle does to a page on its way to the client, given
/// the cursor the page was read after.
pub type Tamper = dyn Fn(Option<&Cursor>, &mut Page) + Send + Sync;

/// What a man in the middle does to the relay's answer to a write.
pub type Answer = dyn Fn(&mut Written) + Send + Sync;

/// The real relay, seen through a transport that can change what it says:
/// smaller limits, pages altered on the way. Everything else goes to the
/// relay as it is.
pub struct Between {
    pub relay: Relay,
    pub limits: Limits,
    pub tamper: Option<Arc<Tamper>>,
    pub appends: Arc<AtomicUsize>,
    /// Holds each append until every transport sharing it has one waiting:
    /// two devices writing at the same moment, on purpose.
    pub together: Option<Arc<Barrier>>,
    /// Told the number of each append, from 1, before it goes - to arm a
    /// cut for that one.
    pub on_append: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    /// What a man in the middle does to the relay's answer to a write.
    pub answer: Option<Arc<Answer>>,
}

impl Between {
    pub fn new(relay: Relay) -> Self {
        Self {
            relay,
            limits: Limits::V1,
            tamper: None,
            appends: Arc::default(),
            together: None,
            on_append: None,
            answer: None,
        }
    }
}

impl Transport for Between {
    fn describe(&self) -> String {
        self.relay.describe()
    }

    async fn limits(&self) -> Result<Limits, TransportError> {
        Ok(self.limits)
    }

    async fn append(&self, stream: &StreamName, batch: &Batch) -> Result<Written, TransportError> {
        let number = self.appends.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(on_append) = &self.on_append {
            on_append(number);
        }
        if let Some(together) = &self.together {
            together.wait().await;
        }
        let mut written = self.relay.append(stream, batch).await?;
        if let Some(answer) = &self.answer {
            answer(&mut written);
        }
        Ok(written)
    }

    async fn read(&self, stream: &StreamName, after: Option<&Cursor>, limit: usize) -> Result<Page, TransportError> {
        let mut page = self.relay.read(stream, after, limit).await?;
        if let Some(tamper) = &self.tamper {
            tamper(after, &mut page);
        }
        Ok(page)
    }

    async fn wait(&self, watches: &[Watch], timeout: Duration) -> Result<Heads, TransportError> {
        self.relay.wait(watches, timeout).await
    }
}

/// A relay with a proxy in front that tears connections on cue.
pub struct TornRelay {
    pub relay: TestRelay,
    pub tear: Arc<lacodda_tear::Tear>,
}

impl TornRelay {
    pub async fn start() -> Self {
        let relay = TestRelay::start().await;
        let tear = Arc::new(lacodda_tear::Tear::start(relay.addr).await.unwrap());
        Self { relay, tear }
    }

    /// The relay as a client reaches it: through the proxy.
    pub fn through(&self) -> Relay {
        Relay::new(&format!("http://{}", self.tear.addr())).unwrap()
    }

    /// The relay as it is, around the proxy: what it really holds.
    pub fn direct(&self) -> Relay {
        self.relay.transport()
    }

    /// The stream's last position, asked of the relay itself.
    pub async fn head(&self, stream: &str) -> u64 {
        self.direct().read(&name(stream), None, 1).await.unwrap().head
    }
}

/// Bytes that zstd cannot shorten: a push of them is as long on the wire as
/// it is in memory, so a cut lands where the test meant it to.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}
