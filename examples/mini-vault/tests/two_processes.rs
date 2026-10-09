//! Two devices as two processes: the built `mini-vault` binary, run once per
//! step against a real relay, each with a folder and a state of its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use tokio::sync::oneshot;

const PASSPHRASE: &str = "a passphrase for tests only";

struct Relay {
    url: String,
    _stop: oneshot::Sender<()>,
    _dir: TempDir,
}

async fn relay() -> Relay {
    let dir = tempfile::tempdir().unwrap();
    let relay = efema_server::Relay::open(dir.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(relay.serve(listener, async move {
        let _ = stopped.await;
    }));
    Relay { url, _stop: stop, _dir: dir }
}

struct Device {
    root: TempDir,
}

impl Device {
    fn new() -> Self {
        Self { root: tempfile::tempdir().unwrap() }
    }

    fn folder(&self) -> PathBuf {
        self.root.path().join("folder")
    }

    fn run(&self, relay: &Relay, args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_mini-vault"))
            .args(["--relay", &relay.url, "--folder"])
            .arg(self.folder())
            .arg("--state")
            .arg(self.root.path().join("state"))
            .args(args)
            .env("MINI_VAULT_PASSPHRASE", PASSPHRASE)
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            output.status.success(),
            "mini-vault {args:?} failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }

    /// One sync, as a process of its own.
    fn sync(&self, relay: &Relay) -> String {
        self.run(relay, &["sync", "--once"])
    }

    fn write(&self, path: &str, content: &[u8]) {
        let path = self.folder().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn remove(&self, path: &str) {
        std::fs::remove_file(self.folder().join(path)).unwrap();
    }

    fn files(&self) -> BTreeMap<String, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, found: &mut BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, found);
                } else {
                    let relative = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                    found.insert(relative, std::fs::read(&path).unwrap());
                }
            }
        }
        let mut found = BTreeMap::new();
        walk(&self.folder(), &self.folder(), &mut found);
        found
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_processes_keep_two_folders_the_same() {
    let relay = relay().await;
    let (laptop, desktop) = (Device::new(), Device::new());

    // A folder with some shape to it: nested, binary, empty, unicode names.
    laptop.write("notes/plan.md", b"# Plan\n\n- ship 0.2\n");
    laptop.write("notes/2026/october.md", b"busy");
    laptop.write("photo.bin", &(0..=255u8).cycle().take(70_000).collect::<Vec<_>>());
    laptop.write("empty.txt", b"");
    laptop.write("заметки/идея.md", "не забыть".as_bytes());
    laptop.sync(&relay);
    desktop.sync(&relay);
    assert_eq!(desktop.files(), laptop.files());

    // An edit and a removal travel the other way.
    desktop.write("notes/plan.md", b"# Plan\n\n- ship 0.2\n- then 0.3\n");
    desktop.remove("empty.txt");
    desktop.sync(&relay);
    laptop.sync(&relay);
    assert_eq!(laptop.files(), desktop.files());
    assert!(!laptop.files().contains_key("empty.txt"));

    // Both edit one file before hearing from the other: both versions
    // survive, on both devices, under the same names.
    laptop.write("notes/2026/october.md", b"busy - laptop");
    desktop.write("notes/2026/october.md", b"busy - desktop");
    laptop.sync(&relay);
    let said = desktop.sync(&relay);
    assert!(said.contains("conflict: both versions kept"), "{said}");
    laptop.sync(&relay);
    desktop.sync(&relay);
    let files = laptop.files();
    assert_eq!(files, desktop.files());
    let october: Vec<&Vec<u8>> =
        files.iter().filter(|(path, _)| path.starts_with("notes/2026/october")).map(|(_, c)| c).collect();
    assert_eq!(october.len(), 2, "{:?}", files.keys());
    assert!(october.contains(&&b"busy - laptop".to_vec()) && october.contains(&&b"busy - desktop".to_vec()));

    // Settled: another round sends nothing.
    assert_eq!(laptop.sync(&relay), "");
    assert_eq!(desktop.sync(&relay), "");

    let report = desktop.run(&relay, &["doctor"]);
    assert!(report.contains("up to date"), "{report}");
}
