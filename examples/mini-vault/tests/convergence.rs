//! The orderings two processes cannot be made to hit on cue - both devices
//! sending before either reads, an answer lost on its way back - driven step
//! by step through the library.

use std::collections::BTreeMap;
use std::path::PathBuf;

use efema::{AccessKey, Client, Epoch, Relay, Secret, State, Transport};
use lacodda_tear::{Cut, Tear};
use mini_vault::Vault;
use tempfile::TempDir;
use tokio::sync::oneshot;

struct Device {
    root: TempDir,
    vault: Vault<Relay>,
}

/// Where a device reaches the relay, and with which key.
#[derive(Clone)]
struct Remote {
    url: String,
    key: AccessKey,
}

impl Remote {
    fn relay(&self) -> Relay {
        Relay::new(&self.url, self.key.clone()).unwrap()
    }

    /// The same relay, through a proxy at `address`.
    fn through(&self, address: std::net::SocketAddr) -> Self {
        Self { url: format!("http://{address}"), key: self.key.clone() }
    }
}

impl Device {
    async fn open(remote: &Remote) -> Self {
        Self::open_in(tempfile::tempdir().unwrap(), remote).await
    }

    /// The same device after the app closed and started again.
    async fn restart(self, remote: &Remote) -> Self {
        let Self { root, vault } = self;
        drop(vault);
        Self::open_in(root, remote).await
    }

    async fn open_in(root: TempDir, remote: &Remote) -> Self {
        let state = State::open(root.path().join("sync.sqlite3")).unwrap();
        let client = Client::open(
            remote.relay(),
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

async fn relay() -> (Remote, oneshot::Sender<()>, TempDir) {
    relay_with(efema_server::Config::default()).await
}

async fn relay_with(config: efema_server::Config) -> (Remote, oneshot::Sender<()>, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = efema_server::store::Store::open(dir.path()).unwrap();
    let key = store.add_key(&"vaults".parse().unwrap(), efema_server::Access::Write, "").unwrap();
    drop(store);
    let relay = efema_server::Relay::open(dir.path(), config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(relay.serve(listener, async move {
        let _ = stopped.await;
    }));
    (Remote { url, key }, stop, dir)
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
    let (remote, _stop, _dir) = relay().await;
    let (mut a, mut b) = (Device::open(&remote).await, Device::open(&remote).await);
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
    let (remote, _stop, _dir) = relay().await;
    let (mut a, mut b) = (Device::open(&remote).await, Device::open(&remote).await);
    a.write("draft.md", "short-lived");
    a.vault.push().await.unwrap();
    std::fs::remove_file(a.folder().join("draft.md")).unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert!(a.files().is_empty() && b.files().is_empty(), "{:?} {:?}", a.files(), b.files());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_wins_over_a_removal_it_did_not_see() {
    let (remote, _stop, _dir) = relay().await;
    let (mut a, mut b) = (Device::open(&remote).await, Device::open(&remote).await);
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
    let (remote, _stop, _dir) = relay().await;
    let (mut a, mut b, mut c) = (Device::open(&remote).await, Device::open(&remote).await, Device::open(&remote).await);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_file_larger_than_the_relay_takes_is_left_out_named_once_and_the_rest_goes() {
    let limits = efema_server::Limits { entry: 16 * 1024, ..efema_server::Limits::default() };
    let (remote, _stop, _dir) = relay_with(efema_server::Config { limits, ..Default::default() }).await;
    let (mut a, mut b) = (Device::open(&remote).await, Device::open(&remote).await);
    let large = "x".repeat(20_000);
    a.write("notes.md", "fits");
    a.write("export.md", &large);

    let first = a.vault.sync().await.unwrap();
    assert_eq!(first.pushed, 1);
    assert_eq!(first.too_large.len(), 1, "{first:?}");
    assert_eq!((first.too_large[0].path.as_str(), first.too_large[0].len), ("export.md", 20_000));
    assert!(first.too_large[0].to_string().starts_with("export.md - 20000 bytes, more than the "));
    // Named once for this version, not at every sync.
    assert!(a.vault.sync().await.unwrap().too_large.is_empty());
    b.sync().await;
    assert_eq!(b.files().keys().collect::<Vec<_>>(), ["notes.md"]);

    // A new version is named again; once it fits, it goes.
    a.write("export.md", &"y".repeat(20_000));
    assert_eq!(a.vault.sync().await.unwrap().too_large.len(), 1);
    a.write("export.md", "trimmed");
    assert_eq!(a.vault.sync().await.unwrap().pushed, 1);
    settle(&mut [&mut a, &mut b]).await;
    assert_eq!(a.files(), b.files());
    assert_eq!(b.files()["export.md"], "trimmed");
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_through_a_lost_answer_and_an_outage_arrive_once_after_a_restart() {
    let (remote, _stop, _dir) = relay().await;
    let tear = Tear::start(remote.url.trim_start_matches("http://").parse().unwrap()).await.unwrap();
    let through = remote.through(tear.addr());
    let (mut a, mut b) = (Device::open(&through).await, Device::open(&remote).await);

    // The relay takes the first change; its answer never reaches the device.
    a.write("trip.md", "written on a train");
    a.vault.pull().await.unwrap();
    tear.cut(Cut::Down(0));
    a.vault.push().await.unwrap_err();
    // Then the network goes: the second change is kept, not sent - and the
    // app tries again before it is back, which must not send either twice.
    tear.offline();
    a.write("tunnel.md", "written in a tunnel");
    a.vault.push().await.unwrap_err();
    let _ = a.vault.push().await;

    // The app closes; it starts again with the network back.
    let mut a = a.restart(&through).await;
    tear.online();
    settle(&mut [&mut a, &mut b]).await;

    assert_eq!(b.files()["trip.md"], "written on a train");
    assert_eq!(b.files()["tunnel.md"], "written in a tunnel");
    assert_eq!(a.files(), b.files());
    let stream = remote.relay().read(&"vault".parse().unwrap(), None, 1).await.unwrap();
    // The key, and each change once.
    assert_eq!(stream.head, 3, "a change went into the stream more than once");
}
