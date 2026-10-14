//! The built `efema-server`, run the way an operator runs it.
//!
//! The relay tests check the protocol in-process; these check the seams a
//! process adds: the address it prints, the lock on its directory, what
//! survives a kill, what the listings show, keys handed out and revoked from
//! another process while the relay runs, and plain HTTP from another machine.

mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use efema_proto::wire::{self, Batch, Page, ProblemCode, Written};
use efema_proto::{Epoch, MEDIA_TYPE};
use reqwest::StatusCode;

const BIN: &str = env!("CARGO_BIN_EXE_efema-server");

/// A running `efema-server serve`, killed when dropped.
struct Served {
    child: Child,
    base: String,
    /// A key that writes to the space `test`, handed out before the start.
    key: String,
    /// The lines logged up to the address, and the channel the rest arrive
    /// on.
    started: Vec<String>,
    log: mpsc::Receiver<String>,
}

impl Served {
    /// Kills the relay and hands back every line it logged.
    fn log(mut self) -> Vec<String> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut lines = std::mem::take(&mut self.started);
        // The reader ends with the pipe, and the channel with the reader.
        while let Ok(line) = self.log.recv_timeout(Duration::from_secs(30)) {
            lines.push(line);
        }
        lines
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(dir: &Path) -> Served {
    serve_logging(dir, "info")
}

fn serve_logging(dir: &Path, level: &str) -> Served {
    serve_on(dir, "127.0.0.1:0", level)
}

/// Hands out a key the way an operator does, and returns it as printed.
fn hand_out(dir: &Path, space: &str, access: &str) -> String {
    let output = run(&["keys", "add", space, access], dir);
    assert!(output.status.success(), "{output:?}");
    let key = String::from_utf8(output.stdout).unwrap();
    assert_eq!(key.lines().count(), 1, "standard output is the key and nothing else: {key:?}");
    key.trim().to_string()
}

fn serve_on(dir: &Path, listen: &str, level: &str) -> Served {
    let key = hand_out(dir, "test", "write");
    let mut child = Command::new(BIN)
        .args(["serve", "--listen", listen, "--data"])
        .arg(dir)
        .env("EFEMA_LOG", level)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // The port is the system's choice; the log line is how to learn it.
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    // Owned by the guard from here on, so a failed start still kills it.
    let mut served = Served { child, base: String::new(), key, started: Vec::new(), log: rx };
    loop {
        let line = served.log.recv_timeout(Duration::from_secs(30)).expect("the relay printed where it listens");
        served.started.push(line.clone());
        // Piped, as under a service manager: colour codes would land in the
        // journal as text.
        assert!(!line.contains('\u{1b}'), "escape codes in a log that is not a terminal: {line:?}");
        if let Some(rest) = line.split("listening on ").nth(1) {
            served.base = rest.split(',').next().unwrap().replace("0.0.0.0", "127.0.0.1");
            assert!(served.base.starts_with("http://127.0.0.1:"), "{line}");
            return served;
        }
    }
}

fn run(args: &[&str], dir: &Path) -> Output {
    Command::new(BIN).args(args).arg("--data").arg(dir).output().unwrap()
}

async fn write(served: &Served, stream: &str, entries: &[&[u8]]) -> Written {
    try_write(&served.base, &served.key, stream, entries).await.unwrap()
}

async fn try_write(base: &str, key: &str, stream: &str, entries: &[&[u8]]) -> Result<Written, common::Refused> {
    let batch = Batch { epoch: Epoch(1), entries: entries.iter().map(|e| e.to_vec()).collect(), stream: None };
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/streams/{stream}/entries"))
        .header("content-type", MEDIA_TYPE)
        .bearer_auth(key)
        .body(wire::encode(&batch))
        .send()
        .await
        .unwrap();
    common::answer(response).await
}

async fn read(served: &Served, stream: &str) -> Page {
    try_read(&served.base, &served.key, stream).await.unwrap()
}

async fn try_read(base: &str, key: &str, stream: &str) -> Result<Page, common::Refused> {
    let response = reqwest::Client::new()
        .get(format!("{base}/v1/streams/{stream}/entries"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    common::answer(response).await
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_lists_survives_a_kill_and_keeps_its_directory_to_itself() {
    let dir = tempfile::tempdir().unwrap();
    let served = serve(dir.path());

    let health = reqwest::get(format!("{}/health", served.base)).await.unwrap();
    assert_eq!(health.text().await.unwrap(), "ok\n");

    let written = write(&served, "notes", &[b"a secret", b"another"]).await;
    write(&served, "photos", &[&[0u8; 2048]]).await;

    // A second relay on the same directory refuses instead of splitting the
    // waiters between two processes.
    let second =
        Command::new(BIN).args(["serve", "--listen", "127.0.0.1:0", "--data"]).arg(dir.path()).output().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already serving"), "{second:?}");

    // The listing works while the relay serves, and shows sizes, not contents.
    let listing = run(&["streams"], dir.path());
    assert!(listing.status.success(), "{listing:?}");
    let text = String::from_utf8(listing.stdout).unwrap();
    assert!(text.lines().next().unwrap().starts_with("STREAM"));
    let notes = text.lines().find(|l| l.starts_with("test/notes")).expect("notes is listed with its space");
    assert!(notes.contains(&written.stream.to_string()[..8]) && notes.contains("15 B"), "{notes}");
    assert!(text.contains("photos") && text.contains("2.0 KiB"), "{text}");

    let entries = run(&["streams", "test/notes"], dir.path());
    let text = String::from_utf8(entries.stdout).unwrap();
    assert_eq!(text.lines().count(), 3, "a header and two entries:\n{text}");
    assert!(!text.contains("secret"), "the listing never shows data");

    // Killed, not stopped: no chance to flush anything on the way out.
    drop(served);
    let served = serve(dir.path());
    let page = read(&served, "notes").await;
    assert_eq!(page.stream, written.stream);
    assert_eq!(page.entries.iter().map(|e| e.data.as_slice()).collect::<Vec<_>>(), [&b"a secret"[..], b"another"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_relay_never_logs_what_it_keeps() {
    let dir = tempfile::tempdir().unwrap();
    let served = serve_logging(dir.path(), "trace");
    let marker = b"ENTRY-BYTES-that-no-log-line-may-show";
    write(&served, "notes", &[marker, marker]).await;
    // Sent again, as after a lost answer: known, and not written - nor logged.
    write(&served, "notes", &[marker, marker]).await;
    read(&served, "notes").await;
    // A body that is not a batch is refused - and the refusal does not echo
    // it back into the log either.
    let refused = reqwest::Client::new()
        .post(format!("{}/v1/streams/notes/entries", served.base))
        .header("content-type", MEDIA_TYPE)
        .bearer_auth(&served.key)
        .body(marker.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    // A key the relay does not know is refused, and neither key is logged.
    let stranger = format!("efema_{}", "ab".repeat(32));
    assert!(try_read(&served.base, &stranger, "notes").await.is_err());
    let key = served.key.clone();

    let log = served.log().join("\n");
    // A check that finds nothing in an empty log proves nothing.
    assert!(
        log.contains("test/notes: wrote 2 entries") && log.contains("sent again") && log.contains("400"),
        "the log was not captured:\n{log}"
    );
    let hex: String = marker.iter().map(|b| format!("{b:02x}")).collect();
    let listed = marker[..8].iter().map(u8::to_string).collect::<Vec<_>>().join(", ");
    for (form, needle) in
        [("as text", String::from_utf8_lossy(marker).into_owned()), ("in hex", hex), ("as numbers", listed)]
    {
        assert!(!log.contains(&needle), "the log shows an entry {form}:\n{log}");
    }
    for shown in [&key, &stranger] {
        let digits = shown.trim_start_matches("efema_");
        assert!(!log.contains(digits), "the log shows an access key:\n{log}");
    }
}

#[test]
fn listing_a_directory_without_a_relay_says_so_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(&["streams"], dir.path());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no relay database"), "{output:?}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_a_stream_that_is_not_there_fails() {
    let dir = tempfile::tempdir().unwrap();
    let served = serve(dir.path());
    write(&served, "notes", &[b"x"]).await;
    let output = run(&["streams", "test/elsewhere"], dir.path());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no stream test/elsewhere"), "{output:?}");
    let output = run(&["streams", "notes"], dir.path());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("<space>/<name>"), "{output:?}");
    let output = run(&["streams", "test/Not-A-Name"], dir.path());
    assert!(!output.status.success());
}

#[test]
fn an_empty_relay_lists_nothing_politely() {
    let dir = tempfile::tempdir().unwrap();
    drop(efema_server::Relay::open(dir.path(), efema_server::Config::default()).unwrap());
    let output = run(&["streams"], dir.path());
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "No streams yet.\n");
}

#[test]
fn reports_its_version() {
    let output = Command::new(BIN).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), format!("efema-server {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn a_busy_port_fails_with_the_address() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = taken.local_addr().unwrap().to_string();
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(BIN).args(["serve", "--listen", &address, "--data"]).arg(dir.path()).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("cannot listen on {address}")), "{output:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_are_handed_out_listed_and_revoked_while_the_relay_runs() {
    let dir = tempfile::tempdir().unwrap();
    let listing = run(&["keys"], dir.path());
    assert!(!listing.status.success(), "listing keys created a relay: {listing:?}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    // The first key, before the relay ever ran, into a directory not made yet.
    let fresh = dir.path().join("not-made-yet");
    hand_out(&fresh, "test", "write");
    assert!(fresh.join(efema_server::DATABASE_FILE).is_file());

    let served = serve(dir.path());
    // Handed out while it runs, good from the next request.
    let phone = hand_out(dir.path(), "test", "read");
    let added = run(&["keys", "add", "test", "read", "--label", "phone"], dir.path());
    let said = String::from_utf8_lossy(&added.stderr);
    assert!(said.contains("reads space test") && said.contains("only time"), "{said}");
    let labelled = String::from_utf8(added.stdout).unwrap().trim().to_string();
    write(&served, "notes", &[b"x"]).await;
    try_read(&served.base, &phone, "notes").await.unwrap();
    let refused = try_write(&served.base, &phone, "notes", &[b"y"]).await.unwrap_err();
    assert_eq!(refused.problem.code, ProblemCode::Forbidden);
    try_read(&served.base, &labelled, "notes").await.unwrap();

    let listing = String::from_utf8(run(&["keys"], dir.path()).stdout).unwrap();
    assert!(listing.lines().next().unwrap().starts_with("ID"), "{listing}");
    assert_eq!(listing.lines().count(), 4, "a header and three keys:\n{listing}");
    let row = listing.lines().find(|line| line.contains("phone")).expect("the labelled key is listed");
    let id = row.split_whitespace().next().unwrap().to_string();
    assert!(row.contains("test") && row.contains("read"), "{row}");
    for key in [&served.key, &phone, &labelled] {
        assert!(!listing.contains(key.trim_start_matches("efema_")), "the listing shows a key:\n{listing}");
    }

    // Revoked from another process, refused from its next request on.
    let revoked = run(&["keys", "revoke", &id], dir.path());
    assert!(revoked.status.success(), "{revoked:?}");
    assert!(String::from_utf8_lossy(&revoked.stdout).contains("(phone)"), "{revoked:?}");
    let refused = try_read(&served.base, &labelled, "notes").await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::UNAUTHORIZED, ProblemCode::Unauthorized));
    try_read(&served.base, &phone, "notes").await.unwrap();
    let again = run(&["keys", "revoke", &id], dir.path());
    assert!(!again.status.success() && String::from_utf8_lossy(&again.stderr).contains("no key has the id"));
    let bad = run(&["keys", "add", "Test", "write"], dir.path());
    assert!(!bad.status.success(), "{bad:?}");
    let bad = run(&["keys", "add", "test", "admin"], dir.path());
    assert!(!bad.status.success(), "{bad:?}");
}

/// This machine's address on its network - not loopback: where a device on
/// the network would reach a relay that listens on every interface. A UDP
/// socket "connected" to an address learns the route there and sends
/// nothing.
fn address_on_the_network() -> std::net::IpAddr {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.connect("192.0.2.1:9").expect("this test needs a network interface besides loopback");
    let address = socket.local_addr().unwrap().ip();
    assert!(!address.is_loopback() && !address.is_unspecified(), "no address besides loopback: {address}");
    address
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_http_from_another_machine_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    // The mistake the door is for: the relay on every interface, no proxy.
    let served = serve_on(dir.path(), "0.0.0.0:0", "info");
    let port = served.base.rsplit(':').next().unwrap().to_string();
    let outside = format!("http://{}:{port}", address_on_the_network());

    let refused = try_write(&outside, &served.key, "notes", &[b"over plain HTTP"]).await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::FORBIDDEN, ProblemCode::HttpsRequired));
    let health = reqwest::get(format!("{outside}/health")).await.unwrap();
    assert_eq!(health.status(), StatusCode::FORBIDDEN);
    let metrics = reqwest::get(format!("{outside}/metrics")).await.unwrap();
    assert_eq!(metrics.status(), StatusCode::FORBIDDEN);

    // The same relay answers its own machine, and wrote nothing for the
    // other one.
    assert_eq!(
        try_read(&served.base, &served.key, "notes").await.unwrap_err().problem.code,
        ProblemCode::StreamNotFound
    );
    let log = served.log().join("\n");
    assert!(log.contains("refused POST /v1/streams/notes/entries: plain HTTP from"), "{log}");
}
