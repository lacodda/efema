//! The built `efema-server`, run the way an operator runs it.
//!
//! The relay tests check the protocol in-process; these check the seams a
//! process adds: the address it prints, the lock on its directory, what
//! survives a kill, and what the listing shows.

mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use efema_proto::wire::{self, Batch, Page, Written};
use efema_proto::{Epoch, MEDIA_TYPE};

const BIN: &str = env!("CARGO_BIN_EXE_efema-server");

/// A running `efema-server serve`, killed when dropped.
struct Served {
    child: Child,
    base: String,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(dir: &Path) -> Served {
    let child = Command::new(BIN)
        .args(["serve", "--listen", "127.0.0.1:0", "--data"])
        .arg(dir)
        .env("EFEMA_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Owned by the guard from here on, so a failed start still kills it.
    let mut served = Served { child, base: String::new() };

    // The port is the system's choice; the log line is how to learn it.
    let stderr = served.child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    loop {
        let line = rx.recv_timeout(Duration::from_secs(30)).expect("the relay printed where it listens");
        // Piped, as under a service manager: colour codes would land in the
        // journal as text.
        assert!(!line.contains('\u{1b}'), "escape codes in a log that is not a terminal: {line:?}");
        if let Some(rest) = line.split("listening on ").nth(1) {
            served.base = rest.split(',').next().unwrap().to_owned();
            assert!(served.base.starts_with("http://127.0.0.1:"), "{line}");
            return served;
        }
    }
}

fn run(args: &[&str], dir: &Path) -> Output {
    Command::new(BIN).args(args).arg("--data").arg(dir).output().unwrap()
}

async fn write(base: &str, stream: &str, entries: &[&[u8]]) -> Written {
    let batch = Batch { epoch: Epoch(1), entries: entries.iter().map(|e| e.to_vec()).collect(), stream: None };
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/streams/{stream}/entries"))
        .header("content-type", MEDIA_TYPE)
        .body(wire::encode(&batch))
        .send()
        .await
        .unwrap();
    common::answer(response).await.unwrap()
}

async fn read(base: &str, stream: &str) -> Page {
    let response = reqwest::get(format!("{base}/v1/streams/{stream}/entries")).await.unwrap();
    common::answer(response).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_lists_survives_a_kill_and_keeps_its_directory_to_itself() {
    let dir = tempfile::tempdir().unwrap();
    let served = serve(dir.path());

    let health = reqwest::get(format!("{}/health", served.base)).await.unwrap();
    assert_eq!(health.text().await.unwrap(), "ok\n");

    let written = write(&served.base, "notes", &[b"a secret", b"another"]).await;
    write(&served.base, "photos", &[&[0u8; 2048]]).await;

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
    let notes = text.lines().find(|l| l.starts_with("notes")).expect("notes is listed");
    assert!(notes.contains(&written.stream.to_string()[..8]) && notes.contains("15 B"), "{notes}");
    assert!(text.contains("photos") && text.contains("2.0 KiB"), "{text}");

    let entries = run(&["streams", "notes"], dir.path());
    let text = String::from_utf8(entries.stdout).unwrap();
    assert_eq!(text.lines().count(), 3, "a header and two entries:\n{text}");
    assert!(!text.contains("secret"), "the listing never shows data");

    // Killed, not stopped: no chance to flush anything on the way out.
    drop(served);
    let served = serve(dir.path());
    let page = read(&served.base, "notes").await;
    assert_eq!(page.stream, written.stream);
    assert_eq!(page.entries.iter().map(|e| e.data.as_slice()).collect::<Vec<_>>(), [&b"a secret"[..], b"another"]);
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
    write(&served.base, "notes", &[b"x"]).await;
    let output = run(&["streams", "elsewhere"], dir.path());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no stream named elsewhere"));
    let output = run(&["streams", "Not-A-Name"], dir.path());
    assert!(!output.status.success());
}

#[test]
fn an_empty_relay_lists_nothing_politely() {
    let dir = tempfile::tempdir().unwrap();
    drop(efema_server::Relay::open(dir.path()).unwrap());
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
