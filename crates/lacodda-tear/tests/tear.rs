//! The proxy against real sockets: a server behind it, a client in front.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use lacodda_tear::{Cut, Direction, Tear};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A server that sends back every byte it gets.
async fn echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut read, mut write) = socket.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    addr
}

/// A server that sends `bytes` to every connection, then waits for the
/// client to go.
async fn source(bytes: Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let bytes = bytes.clone();
            tokio::spawn(async move {
                let _ = socket.write_all(&bytes).await;
                let _ = socket.read(&mut [0u8; 1]).await;
            });
        }
    });
    addr
}

/// A connection through the proxy, shown to be open: one byte there and
/// back, so the proxy holds both ends of it.
async fn open(tear: &Tear) -> TcpStream {
    let mut socket = TcpStream::connect(tear.addr()).await.unwrap();
    round_trip(&mut socket, b"o").await;
    socket
}

async fn round_trip(socket: &mut TcpStream, bytes: &[u8]) {
    socket.write_all(bytes).await.unwrap();
    let mut back = vec![0u8; bytes.len()];
    socket.read_exact(&mut back).await.unwrap();
    assert_eq!(back, bytes);
}

/// Reads until the connection ends; what came, and whether it ended in a
/// reset rather than an end of stream.
async fn read_to_end(socket: &mut TcpStream) -> (Vec<u8>, bool) {
    let mut got = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.read(&mut buffer)).await {
            Err(_) => panic!("the connection neither ended nor was torn"),
            Ok(Ok(0)) => return (got, false),
            Ok(Ok(len)) => got.extend_from_slice(&buffer[..len]),
            Ok(Err(e)) => {
                assert!(
                    matches!(e.kind(), ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted),
                    "torn with {e}, not a reset"
                );
                return (got, true);
            }
        }
    }
}

/// Whether the connection is torn: a reset on read, or - once the reset
/// has arrived - on write.
async fn is_torn(socket: &mut TcpStream) -> bool {
    let (_, reset) = read_to_end(socket).await;
    reset
}

#[tokio::test]
async fn bytes_pass_both_ways_as_they_are() {
    let tear = Tear::start(echo().await).await.unwrap();
    let mut socket = TcpStream::connect(tear.addr()).await.unwrap();
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    round_trip(&mut socket, &payload).await;
    assert_eq!(tear.passed(Direction::Up), payload.len());
    assert_eq!(tear.passed(Direction::Down), payload.len());
    assert_eq!((tear.connections(), tear.torn()), (1, 0));
}

#[tokio::test]
async fn an_up_cut_passes_that_many_bytes_then_tears() {
    let tear = Tear::start(echo().await).await.unwrap();
    let mut socket = open(&tear).await;
    tear.cut(Cut::Up(40));
    socket.write_all(&[7u8; 100]).await.unwrap();
    let (back, reset) = read_to_end(&mut socket).await;
    assert!(reset, "the connection ended rather than tore");
    assert!(back.len() <= 40, "{} bytes came back through a cut at 40", back.len());
    assert_eq!(tear.passed(Direction::Up), 1 + 40);
    assert_eq!(tear.torn(), 1);
}

#[tokio::test]
async fn a_down_cut_passes_that_many_bytes_then_tears() {
    let tear = Tear::start(source(vec![9u8; 100]).await).await.unwrap();
    tear.cut(Cut::Down(30));
    let mut socket = TcpStream::connect(tear.addr()).await.unwrap();
    let (got, reset) = read_to_end(&mut socket).await;
    assert!(reset);
    assert!(got.len() <= 30, "{} bytes came through a cut at 30", got.len());
    assert_eq!(tear.passed(Direction::Down), 30);
    assert_eq!(tear.torn(), 1);
}

#[tokio::test]
async fn down_zero_loses_an_answer_the_server_sent() {
    let tear = Tear::start(echo().await).await.unwrap();
    let mut socket = open(&tear).await;
    tear.cut(Cut::Down(0));
    socket.write_all(b"question").await.unwrap();
    let (got, reset) = read_to_end(&mut socket).await;
    assert!(reset && got.is_empty());
    // The question reached the server; its answer went nowhere.
    assert_eq!(tear.passed(Direction::Up), 1 + 8);
    assert_eq!(tear.passed(Direction::Down), 1);
}

#[tokio::test]
async fn a_cut_counts_bytes_across_connections_and_fires_once() {
    let tear = Tear::start(echo().await).await.unwrap();
    let mut first = open(&tear).await;
    let mut second = open(&tear).await;
    tear.cut(Cut::Up(10));
    round_trip(&mut first, b"sixsix").await;
    second.write_all(b"sixsix").await.unwrap();
    let (back, reset) = read_to_end(&mut second).await;
    assert!(reset);
    assert!(back.len() <= 4, "{} bytes came back through what was left of the cut", back.len());
    // Fired and gone: the other connection, and new ones, carry on.
    round_trip(&mut first, b"more bytes than the cut had").await;
    let mut third = open(&tear).await;
    round_trip(&mut third, b"and here too").await;
    assert_eq!(tear.torn(), 1);
}

#[tokio::test]
async fn a_disarmed_cut_never_fires() {
    let tear = Tear::start(echo().await).await.unwrap();
    let mut socket = open(&tear).await;
    tear.cut(Cut::Up(0));
    tear.cut(Cut::Down(0));
    tear.disarm();
    round_trip(&mut socket, b"through").await;
    assert_eq!(tear.torn(), 0);
}

#[tokio::test]
async fn now_tears_what_is_open() {
    let tear = Tear::start(echo().await).await.unwrap();
    let mut first = open(&tear).await;
    let mut second = open(&tear).await;
    tear.cut(Cut::Now);
    assert!(is_torn(&mut first).await);
    assert!(is_torn(&mut second).await);
    assert_eq!(tear.torn(), 2);
    let mut later = open(&tear).await;
    round_trip(&mut later, b"after").await;
}

#[tokio::test]
async fn after_tears_what_is_open_once_the_time_has_passed() {
    let tear = Tear::start(echo().await).await.unwrap();
    let mut socket = open(&tear).await;
    let started = Instant::now();
    tear.cut(Cut::After(Duration::from_millis(300)));
    round_trip(&mut socket, b"still open").await;
    assert!(is_torn(&mut socket).await);
    assert!(started.elapsed() >= Duration::from_millis(300), "torn after {:?}", started.elapsed());
}

#[tokio::test]
async fn offline_tears_what_is_open_and_resets_what_comes_until_online() {
    let tear = Tear::start(echo().await).await.unwrap();
    let mut open_before = open(&tear).await;
    tear.offline();
    assert!(is_torn(&mut open_before).await);

    let mut during = TcpStream::connect(tear.addr()).await.unwrap();
    // The write may land before the reset does; the read cannot.
    let _ = during.write_all(b"anyone?").await;
    assert!(is_torn(&mut during).await);

    tear.online();
    let mut after = open(&tear).await;
    round_trip(&mut after, b"back").await;
    assert_eq!(tear.torn(), 2);
}

#[tokio::test]
async fn a_stall_holds_bytes_until_they_flow() {
    let tear = Tear::start(echo().await).await.unwrap();
    let mut socket = open(&tear).await;
    tear.stall();
    socket.write_all(b"held").await.unwrap();
    let mut back = [0u8; 4];
    let waited = tokio::time::timeout(Duration::from_millis(300), socket.read_exact(&mut back)).await;
    assert!(waited.is_err(), "bytes moved through a stalled connection");
    tear.flow();
    socket.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"held");
    assert_eq!(tear.torn(), 0);
}

#[tokio::test]
async fn a_throttle_passes_bytes_at_its_rate_until_lifted() {
    let tear = Tear::start(echo().await).await.unwrap();
    let mut socket = open(&tear).await;
    let payload = vec![7u8; 40_000];

    // 40 kB each way at 20 kB a second: two seconds, the two ways together.
    tear.throttle(20_000);
    let started = Instant::now();
    round_trip(&mut socket, &payload).await;
    let throttled = started.elapsed();
    assert!(throttled >= Duration::from_millis(1_800), "40 kB passed in {throttled:?}");

    tear.unthrottle();
    let started = Instant::now();
    round_trip(&mut socket, &payload).await;
    let lifted = started.elapsed();
    assert!(lifted < Duration::from_secs(1), "still throttled: {lifted:?}");
    assert_eq!(tear.torn(), 0);
}

#[tokio::test]
async fn a_server_that_is_not_there_resets_the_client() {
    let gone = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let tear = Tear::start(gone).await.unwrap();
    let mut socket = TcpStream::connect(tear.addr()).await.unwrap();
    assert!(is_torn(&mut socket).await);
}
