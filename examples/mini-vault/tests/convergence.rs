//! The orderings two processes cannot be made to hit on cue - both devices
//! sending before either reads - driven step by step through the library.

use std::collections::BTreeMap;
use std::path::PathBuf;

use efema::{Client, Epoch, Relay, Secret, State};
use mini_vault::Vault;
use tempfile::TempDir;
use tokio::sync::oneshot;

struct Device {
    root: TempDir,
    vault: Vault<Relay>,
}

impl Device {
    async fn open(url: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = State::open(root.path().join("sync.sqlite3")).unwrap();
        let client = Client::open(
            Relay::new(url).unwrap(),
            &state,
            "vault".parse().unwrap(),
            Epoch(1),
            Secret::Passphrase(b"a passphrase for tests only"),
        )
        .await
        .unwrap();
        std::fs::create_dir_all(root.path().join("folder")).unwrap();
        let vault = Vault::open(client, &root.path().join("folder"), &root.path().join("index")).unwrap();
        Self { root, vault }
    }

    fn folder(&self) -> PathBuf {
        self.root.path().join("folder")
    }

    fn write(&self, path: &str, content: &str) {
        std::fs::write(self.folder().join(path), content).unwrap();
    }

    fn files(&self) -> BTreeMap<String, String> {
        std::fs::read_dir(self.folder())
            .unwrap()
            .map(|e| e.unwrap())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read_to_string(e.path()).unwrap()))
            .collect()
    }

    async fn sync(&mut self) {
        self.vault.sync().await.unwrap();
    }
}

async fn relay() -> (String, oneshot::Sender<()>, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let relay = efema_server::Relay::open(dir.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(relay.serve(listener, async move {
        let _ = stopped.await;
    }));
    (url, stop, dir)
}

/// Syncs every device until a round changes nothing anywhere.
async fn settle(devices: &mut [&mut Device]) {
    for _ in 0..4 {
        for device in devices.iter_mut() {
            device.sync().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_sent_at_once_converge_with_both_kept() {
    let (url, _stop, _dir) = relay().await;
    let (mut a, mut b) = (Device::open(&url).await, Device::open(&url).await);
    a.write("plan.md", "v0");
    settle(&mut [&mut a, &mut b]).await;

    // Both send their edit before either reads the other's.
    a.write("plan.md", "from a");
    b.write("plan.md", "from b");
    a.vault.push().await.unwrap();
    b.vault.push().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;

    let files = a.files();
    assert_eq!(files, b.files());
    // The later one in the stream - b's - keeps the name; a's sits beside it.
    assert_eq!(files["plan.md"], "from b");
    let copies: Vec<&String> = files.keys().filter(|name| name.contains("(conflict")).collect();
    assert_eq!(copies.len(), 1, "{files:?}");
    assert_eq!(files[copies[0]], "from a");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_created_and_removed_before_it_comes_back_stays_removed() {
    let (url, _stop, _dir) = relay().await;
    let (mut a, mut b) = (Device::open(&url).await, Device::open(&url).await);
    a.write("draft.md", "short-lived");
    a.vault.push().await.unwrap();
    std::fs::remove_file(a.folder().join("draft.md")).unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert!(a.files().is_empty() && b.files().is_empty(), "{:?} {:?}", a.files(), b.files());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_wins_over_a_removal_it_did_not_see() {
    let (url, _stop, _dir) = relay().await;
    let (mut a, mut b) = (Device::open(&url).await, Device::open(&url).await);
    a.write("keep.md", "v0");
    settle(&mut [&mut a, &mut b]).await;

    std::fs::remove_file(a.folder().join("keep.md")).unwrap();
    a.vault.push().await.unwrap();
    b.write("keep.md", "edited");
    settle(&mut [&mut a, &mut b]).await;
    assert_eq!(a.files(), b.files());
    assert_eq!(a.files()["keep.md"], "edited");
}

#[tokio::test(flavor = "multi_thread")]
async fn three_devices_converge() {
    let (url, _stop, _dir) = relay().await;
    let (mut a, mut b, mut c) = (Device::open(&url).await, Device::open(&url).await, Device::open(&url).await);
    a.write("shared.md", "v0");
    settle(&mut [&mut a, &mut b, &mut c]).await;

    a.write("shared.md", "a");
    b.write("shared.md", "b");
    c.write("shared.md", "c");
    c.write("only-c.md", "c alone");
    for device in [&mut a, &mut b, &mut c] {
        device.vault.push().await.unwrap();
    }
    settle(&mut [&mut a, &mut b, &mut c]).await;
    assert_eq!(a.files(), b.files());
    assert_eq!(b.files(), c.files());
    let files = a.files();
    let contents: std::collections::BTreeSet<&str> = files.values().map(String::as_str).collect();
    for version in ["a", "b", "c", "c alone"] {
        assert!(contents.contains(version), "{version} was lost: {files:?}");
    }
}
