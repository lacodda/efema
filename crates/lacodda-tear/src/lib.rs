//! A TCP proxy for tests that tears connections where you say.
//!
//! Code that talks over a network has to survive the network breaking: a
//! request torn halfway up, an answer lost on its way back, a connection that
//! goes silent, a phone that drops off the network and comes back. A mock of
//! the other side cannot show any of that - it never breaks the way a socket
//! does. [`Tear`] stands between the code under test and the real server, on
//! a real socket, passes every byte through, and breaks the connection on
//! cue:
//!
//! - [`Cut::Up`] - after so many more bytes towards the server: a request
//!   torn partway;
//! - [`Cut::Down`] - after so many more bytes back from it: an answer torn
//!   partway, or - with zero - lost before its first byte, after the server
//!   has done the work;
//! - [`Cut::After`] - whatever is open once a while has passed;
//! - [`Cut::Now`] - whatever is open now;
//! - [`Tear::offline`] and [`Tear::online`] - the network gone: open
//!   connections torn, new ones reset at once, until it is back;
//! - [`Tear::stall`] and [`Tear::flow`] - bytes held, the connection open and
//!   silent, as on a network that drops packets without saying so;
//! - [`Tear::throttle`] and [`Tear::unthrottle`] - bytes passed at so many a
//!   second, as on a slow network that keeps moving.
//!
//! Byte counts run across every connection from the moment a cut is armed,
//! not per connection: an HTTP client that reuses a pooled connection for
//! the next request is cut just the same. A tear is a reset (`RST`), the way
//! a dropped mobile connection looks to both ends - not a polite end of the
//! stream.
//!
//! The proxy knows nothing of what it carries: HTTP, a database's protocol,
//! anything over TCP.
//!
//! ```no_run
//! use lacodda_tear::{Cut, Tear};
//!
//! # async fn run(server: std::net::SocketAddr) -> std::io::Result<()> {
//! let tear = Tear::start(server).await?;
//! // Point the client at `tear.addr()` instead of the server.
//!
//! // The next answer is lost: the server does the work, the client hears
//! // nothing back.
//! tear.cut(Cut::Down(0));
//! // ... make the request, see it fail, make it again ...
//! assert_eq!(tear.torn(), 1);
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

/// Where a connection breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Cut {
    /// Let this many more bytes through towards the server, then tear the
    /// connection that carries the next one.
    Up(usize),
    /// Let this many more bytes through back from the server, then tear the
    /// connection that carries the next one. `Down(0)` loses an answer
    /// before its first byte - after the server has sent it.
    Down(usize),
    /// Tear every connection open once this long has passed.
    After(Duration),
    /// Tear every connection open now.
    Now,
}

/// Which way bytes go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the client towards the server.
    Up,
    /// From the server back to the client.
    Down,
}

/// A proxy on a port of its own, in front of one server.
///
/// Dropping it stops the proxy and closes every connection through it.
pub struct Tear {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
}

struct Shared {
    upstream: SocketAddr,
    rules: Mutex<Rules>,
    /// Whether bytes move: false while stalled.
    flowing: watch::Sender<bool>,
    /// The connections open now, each with the switch that tears it.
    open: Mutex<HashMap<u64, watch::Sender<bool>>>,
    next: AtomicU64,
    accepted: AtomicUsize,
    torn: AtomicUsize,
    passed_up: AtomicUsize,
    passed_down: AtomicUsize,
}

#[derive(Default)]
struct Rules {
    /// Bytes left before the tear, towards the server; `None` when unarmed.
    up: Option<usize>,
    /// Bytes left before the tear, back from the server.
    down: Option<usize>,
    offline: bool,
    /// Bytes a second each way on each connection; `None` when unthrottled.
    rate: Option<usize>,
}

/// How often a throttled connection passes its next slice of bytes.
const TICK: Duration = Duration::from_millis(100);

impl Tear {
    /// A proxy in front of `upstream`, listening on a free port of the
    /// loopback interface.
    ///
    /// # Errors
    ///
    /// When no port can be bound.
    pub async fn start(upstream: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            upstream,
            rules: Mutex::new(Rules::default()),
            flowing: watch::Sender::new(true),
            open: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
            accepted: AtomicUsize::new(0),
            torn: AtomicUsize::new(0),
            passed_up: AtomicUsize::new(0),
            passed_down: AtomicUsize::new(0),
        });
        let accept = tokio::spawn(accept(listener, shared.clone()));
        Ok(Self { addr, shared, accept })
    }

    /// Where the client connects instead of the server.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The server this proxy stands in front of.
    pub fn upstream(&self) -> SocketAddr {
        self.shared.upstream
    }

    /// Arms a cut. A byte cut replaces an earlier one of the same direction
    /// that has not fired yet; once fired, it is gone.
    pub fn cut(&self, cut: Cut) {
        match cut {
            Cut::Up(bytes) => self.shared.rules().up = Some(bytes),
            Cut::Down(bytes) => self.shared.rules().down = Some(bytes),
            Cut::Now => self.shared.tear_open(),
            Cut::After(delay) => {
                let shared = Arc::downgrade(&self.shared);
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    if let Some(shared) = shared.upgrade() {
                        shared.tear_open();
                    }
                });
            }
        }
    }

    /// Disarms the byte cuts that have not fired yet.
    pub fn disarm(&self) {
        let mut rules = self.shared.rules();
        rules.up = None;
        rules.down = None;
    }

    /// The network is gone: every open connection is torn, and every new one
    /// is reset as soon as it is accepted, until [`Tear::online`].
    pub fn offline(&self) {
        self.shared.rules().offline = true;
        self.shared.tear_open();
    }

    /// The network is back.
    pub fn online(&self) {
        self.shared.rules().offline = false;
    }

    /// Holds every byte where it is, in both directions, with the
    /// connections left open - until [`Tear::flow`].
    pub fn stall(&self) {
        self.shared.flowing.send_replace(false);
    }

    /// Lets bytes move again after [`Tear::stall`].
    pub fn flow(&self) {
        self.shared.flowing.send_replace(true);
    }

    /// Passes bytes at `bytes_per_second` - at least one - each way on each
    /// connection, open or new, a tenth of a second's worth at a time: a slow
    /// network that keeps moving, until [`Tear::unthrottle`].
    pub fn throttle(&self, bytes_per_second: usize) {
        self.shared.rules().rate = Some(bytes_per_second.max(1));
    }

    /// Passes bytes as fast as they come again after [`Tear::throttle`].
    pub fn unthrottle(&self) {
        self.shared.rules().rate = None;
    }

    /// How many connections the proxy has accepted.
    pub fn connections(&self) -> usize {
        self.shared.accepted.load(Ordering::SeqCst)
    }

    /// How many connections the proxy has torn - by a cut, or by going
    /// offline - counting those reset as they were accepted.
    pub fn torn(&self) -> usize {
        self.shared.torn.load(Ordering::SeqCst)
    }

    /// How many bytes the proxy has passed on in `direction`, over every
    /// connection.
    pub fn passed(&self, direction: Direction) -> usize {
        match direction {
            Direction::Up => self.shared.passed_up.load(Ordering::SeqCst),
            Direction::Down => self.shared.passed_down.load(Ordering::SeqCst),
        }
    }
}

impl Drop for Tear {
    fn drop(&mut self) {
        // The accept loop owns every connection's task: aborting it drops
        // them, and their sockets with them.
        self.accept.abort();
    }
}

impl std::fmt::Debug for Tear {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tear").field("addr", &self.addr).field("upstream", &self.shared.upstream).finish()
    }
}

impl Shared {
    fn rules(&self) -> MutexGuard<'_, Rules> {
        lock(&self.rules)
    }

    fn tear_open(&self) {
        for switch in lock(&self.open).values() {
            switch.send_replace(true);
        }
    }

    /// Takes `len` bytes from the budget of `direction`: how many of them
    /// pass, and whether the connection is torn after them.
    fn take(&self, direction: Direction, len: usize) -> (usize, bool) {
        let mut rules = self.rules();
        let budget = match direction {
            Direction::Up => &mut rules.up,
            Direction::Down => &mut rules.down,
        };
        match budget {
            None => (len, false),
            Some(left) if len <= *left => {
                *left -= len;
                (len, false)
            }
            Some(left) => {
                let pass = *left;
                *budget = None;
                (pass, true)
            }
        }
    }

    fn passed(&self, direction: Direction, len: usize) {
        let counter = match direction {
            Direction::Up => &self.passed_up,
            Direction::Down => &self.passed_down,
        };
        counter.fetch_add(len, Ordering::SeqCst);
    }
}

async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    let mut connections = JoinSet::new();
    loop {
        // Finished connections are reaped as the loop goes, so the set does
        // not grow with every connection a long test makes.
        while connections.try_join_next().is_some() {}
        let Ok((client, _)) = listener.accept().await else {
            continue;
        };
        shared.accepted.fetch_add(1, Ordering::SeqCst);
        connections.spawn(serve(client, shared.clone()));
    }
}

/// How one direction of a connection ended.
enum Flow {
    /// Its sender finished; the end was passed on.
    Ended,
    /// A cut fired.
    Cut,
    /// A socket failed - the peer reset it, say.
    Failed,
}

async fn serve(client: TcpStream, shared: Arc<Shared>) {
    if shared.rules().offline {
        shared.torn.fetch_add(1, Ordering::SeqCst);
        reset(&client);
        return;
    }
    let Ok(server) = TcpStream::connect(shared.upstream).await else {
        reset(&client);
        return;
    };
    // Each byte goes on as it comes: a cut lands where it was asked to, not
    // where a send buffer happened to flush.
    let _ = client.set_nodelay(true);
    let _ = server.set_nodelay(true);

    let id = shared.next.fetch_add(1, Ordering::SeqCst);
    let (switch, mut torn) = watch::channel(false);
    lock(&shared.open).insert(id, switch);
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();

    let end = {
        let up = pump(&mut client_read, &mut server_write, Direction::Up, &shared);
        let down = pump(&mut server_read, &mut client_write, Direction::Down, &shared);
        tokio::pin!(up, down);
        let (mut up_done, mut down_done) = (false, false);
        loop {
            let flow = tokio::select! {
                flow = &mut up, if !up_done => { up_done = true; flow }
                flow = &mut down, if !down_done => { down_done = true; flow }
                _ = torn.wait_for(|torn| *torn) => Flow::Cut,
            };
            match flow {
                Flow::Ended if up_done && down_done => break Flow::Ended,
                Flow::Ended => {}
                end => break end,
            }
        }
    };
    lock(&shared.open).remove(&id);
    if matches!(end, Flow::Ended) {
        return;
    }
    if matches!(end, Flow::Cut) {
        shared.torn.fetch_add(1, Ordering::SeqCst);
    }
    // A failure on one side is passed on to the other as what it was. The
    // halves go back together first: a write half dropped on its own ends
    // the stream politely before the reset could say otherwise.
    for (read, write) in [(client_read, client_write), (server_read, server_write)] {
        if let Ok(socket) = read.reunite(write) {
            reset(&socket);
        }
    }
}

/// Moves bytes one way until the sender ends, a cut fires or a socket fails.
async fn pump(from: &mut OwnedReadHalf, to: &mut OwnedWriteHalf, direction: Direction, shared: &Shared) -> Flow {
    let mut flowing = shared.flowing.subscribe();
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let len = match from.read(&mut buffer).await {
            Ok(0) => {
                return match to.shutdown().await {
                    Ok(()) => Flow::Ended,
                    Err(_) => Flow::Failed,
                };
            }
            Ok(len) => len,
            Err(_) => return Flow::Failed,
        };
        if flowing.wait_for(|flowing| *flowing).await.is_err() {
            return Flow::Failed;
        }
        let (pass, cut) = shared.take(direction, len);
        let mut rest = &buffer[..pass];
        while !rest.is_empty() {
            let rate = shared.rules().rate;
            let slice = rate.map_or(rest.len(), |rate| rest.len().min((rate / 10).max(1)));
            if to.write_all(&rest[..slice]).await.is_err() {
                return Flow::Failed;
            }
            shared.passed(direction, slice);
            rest = &rest[slice..];
            if rate.is_some() {
                tokio::time::sleep(TICK).await;
            }
        }
        if cut {
            return Flow::Cut;
        }
    }
}

/// Closes a socket with a reset rather than an end of stream: the peer reads
/// an error, not a short message.
fn reset(socket: &TcpStream) {
    // Best effort: a socket the peer has reset already cannot be set, and is
    // torn anyway.
    let _ = socket.set_zero_linger();
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
