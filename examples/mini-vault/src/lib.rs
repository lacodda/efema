//! A folder of files kept in sync between devices through an efema relay -
//! the app half of a sync, written the way a real consumer writes it.
//!
//! efema moves sealed items in one order for everyone; everything here is
//! what it leaves to the app: what an item means (a [`Change`] to one file),
//! how the app finds its own changes (an index of what the stream says each
//! file is), and what it does when two devices changed one file (keeps both).
//!
//! # Merging
//!
//! Every change names the version of the file it replaces - its *base*. Every
//! device applies the stream in the same order, so every device sees the same
//! thing when a change's base is not the file's current version in the
//! stream: the change was made without seeing that version. The change wins,
//! being later, and the version it overlooked is kept beside it as
//! `name (conflict <device>).ext`, named after the device that wrote it. The
//! decision depends on the stream alone, so every device makes the same one
//! and the folders converge.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use efema::{Client, Received, Transport};
use minicbor::bytes::ByteVec;
use minicbor::{Decode, Encode};
use sha2::{Digest, Sha256};

/// The suffix of a file being written, before it takes its name.
const TEMP_SUFFIX: &str = ".mini-vault-tmp";

/// A file's content hash.
pub type Digest32 = [u8; 32];

/// One item in the stream: a file's new content, or its removal.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Change {
    /// The file, relative to the folder, with `/` between parts.
    #[n(0)]
    pub path: String,
    /// The new content; `None` when the file was removed.
    #[n(1)]
    pub content: Option<ByteVec>,
    /// The version this change replaces; `None` for a file that did not
    /// exist.
    #[n(2)]
    pub base: Option<ByteVec>,
}

/// What the stream says a file is.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Known {
    hash: Digest32,
    writer: String,
}

/// What one sync did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Changes from other devices applied to the folder.
    pub applied: usize,
    /// Local changes sent.
    pub pushed: usize,
    /// Conflict copies made, by path.
    pub conflicts: Vec<String>,
    /// Remote changes refused, with the reason.
    pub refused: Vec<String>,
}

/// A folder and what the device knows about it.
pub struct Vault<T> {
    client: Client<T>,
    folder: PathBuf,
    index_path: PathBuf,
    /// Each file as the stream has it, as far as this device has read.
    stream: BTreeMap<String, Known>,
    /// Local changes sent but not yet read back: the content's hash, or
    /// `None` for a removal.
    in_flight: BTreeMap<String, Option<Digest32>>,
    /// Local edits made on top of a version the stream has since replaced:
    /// the version they were made on.
    held: BTreeMap<String, Option<Digest32>>,
}

impl<T: Transport> Vault<T> {
    /// A vault syncing `folder` through `client`, remembering what it knows
    /// in `index`.
    pub fn open(client: Client<T>, folder: &Path, index: &Path) -> Result<Self> {
        let mut vault = Self {
            client,
            folder: folder.to_path_buf(),
            index_path: index.to_path_buf(),
            stream: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            held: BTreeMap::new(),
        };
        vault.load()?;
        Ok(vault)
    }

    /// The client underneath.
    pub fn client(&self) -> &Client<T> {
        &self.client
    }

    /// Sends what an earlier sync left waiting, reads the stream, then sends
    /// what changed here.
    pub async fn sync(&mut self) -> Result<Summary> {
        // Changes a torn connection left on this device go first, in their
        // place before anything changed since.
        self.client.send().await?;
        let mut summary = self.pull().await?;
        summary.pushed = self.push().await?;
        Ok(summary)
    }

    /// Applies everything the stream has that this device has not seen.
    pub async fn pull(&mut self) -> Result<Summary> {
        let mut summary = Summary::default();
        loop {
            let pulled = self.client.pull().await?;
            for received in &pulled.entries {
                self.apply(received, &mut summary)?;
            }
            // What was applied is on disk, and the index says so, before the
            // cursor moves past it: a crash in between applies it again.
            self.save()?;
            self.client.ack(&pulled).await?;
            if !pulled.more() {
                return Ok(summary);
            }
        }
    }

    /// Sends every local change, as one push.
    pub async fn push(&mut self) -> Result<usize> {
        let mut changes = Vec::new();
        let mut flying = Vec::new();
        let on_disk = scan(&self.folder)?;
        for (path, hash) in &on_disk {
            if self.stream.get(path).map(|k| k.hash) == Some(*hash) && !self.in_flight.contains_key(path) {
                continue;
            }
            if self.in_flight.get(path) == Some(&Some(*hash)) {
                continue;
            }
            let content = fs::read(self.folder.join(path)).with_context(|| format!("reading {path}"))?;
            changes.push(Change { path: path.clone(), content: Some(content.into()), base: self.base(path) });
            flying.push((path.clone(), Some(*hash)));
        }
        // Removed here: a file the stream has, or one sent from here and not
        // read back yet - removing that one before it comes back must not
        // let it come back.
        let known: std::collections::BTreeSet<&String> = self.stream.keys().chain(self.in_flight.keys()).collect();
        for path in known {
            if on_disk.contains_key(path) || self.in_flight.get(path) == Some(&None) {
                continue;
            }
            changes.push(Change { path: path.clone(), content: None, base: self.base(path) });
            flying.push((path.clone(), None));
        }
        if changes.is_empty() {
            return Ok(0);
        }
        let items: Vec<Vec<u8>> =
            changes.iter().map(|c| minicbor::to_vec(c).expect("encoding into a vector")).collect();
        let sent = self.client.push(&items).await;
        // Kept by the client either way - sent now, or left waiting on this
        // device for the next sync: in flight from here, so the next scan
        // does not send them a second time.
        if matches!(sent, Ok(_) | Err(efema::Error::Unsent { .. })) {
            for (path, hash) in flying {
                self.held.remove(&path);
                self.in_flight.insert(path, hash);
            }
            self.save()?;
        }
        sent?;
        Ok(changes.len())
    }

    /// The version a local change of `path` is made on: the one still in
    /// flight, else the one a held edit started from, else the stream's.
    fn base(&self, path: &str) -> Option<ByteVec> {
        let hash = match (self.in_flight.get(path), self.held.get(path)) {
            (Some(flying), _) => *flying,
            (None, Some(held)) => *held,
            (None, None) => self.stream.get(path).map(|k| k.hash),
        };
        hash.map(|h| h.to_vec().into())
    }

    fn apply(&mut self, received: &Received, summary: &mut Summary) -> Result<()> {
        let change: Change = match minicbor::decode(&received.data) {
            Ok(change) => change,
            Err(e) => {
                summary.refused.push(format!("entry {}: not a change ({e})", received.seq));
                return Ok(());
            }
        };
        if let Err(reason) = check_path(&change.path) {
            summary.refused.push(format!("entry {}: {reason}", received.seq));
            return Ok(());
        }
        let path = change.path.clone();
        let writer = received.device.short();
        let new = change.content.as_ref().map(|c| hash(c));
        let base = change.base.as_ref().and_then(|b| <Digest32>::try_from(b.as_slice()).ok());
        let current = self.stream.get(&path).cloned();
        let local = file_hash(&self.folder.join(&path))?;

        // The change was made without seeing the version the stream has now:
        // that version is kept beside it, the same way on every device.
        if let Some(current) = &current
            && base != Some(current.hash)
            && new != Some(current.hash)
            && local == Some(current.hash)
        {
            // An ordinary new file from here on: sent with the next push. Every
            // device that has the version makes the same copy, and the copies
            // meet in the stream as one file.
            let copy = conflict_name(&path, &current.writer);
            let content = fs::read(self.folder.join(&path))?;
            write_file(&self.folder.join(&copy), &content)?;
            summary.conflicts.push(copy);
        }

        // Whether the file here is what the stream - or this device's own
        // change on its way through it - says it is. Anything else is an
        // edit made here since, not sent yet.
        let in_flight = self.in_flight.get(&path).copied();
        let clean = local == new
            || match in_flight {
                Some(flying) => local == flying,
                None => local == current.as_ref().map(|k| k.hash),
            };
        if received.own && in_flight == Some(new) {
            self.in_flight.remove(&path);
        }

        if clean {
            match &change.content {
                Some(content) if local != new => write_file(&self.folder.join(&path), content)?,
                Some(_) => {}
                None => remove_file(&self.folder.join(&path))?,
            }
            if !received.own {
                // This device's own change to the file, if one is on its way,
                // is no longer what the folder holds: it is applied, like any
                // other, when it comes back in its place in the order.
                self.in_flight.remove(&path);
                summary.applied += 1;
            }
        } else if !received.own {
            // An edit here that is not sent yet, on top of the version this
            // change replaces. The edit stays; the change is kept beside it,
            // where the stream's own conflict rule will put it once the edit
            // is sent - and the edit is sent as made on the old version.
            self.held.entry(path.clone()).or_insert(current.as_ref().map(|k| k.hash));
            if let Some(content) = &change.content {
                let copy = conflict_name(&path, &writer);
                write_file(&self.folder.join(&copy), content)?;
                summary.conflicts.push(copy);
            }
        }
        match new {
            Some(hash) => self.stream.insert(path, Known { hash, writer }),
            None => self.stream.remove(&path),
        };
        Ok(())
    }

    fn load(&mut self) -> Result<()> {
        let text = match fs::read_to_string(&self.index_path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.index_path.display())),
        };
        for line in text.lines() {
            let mut parts = line.splitn(4, ' ');
            let (Some(kind), Some(hash), Some(writer), Some(path)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                bail!("{} has a line that is not an index line: {line:?}", self.index_path.display());
            };
            let hash = parse_hash(hash)?;
            match kind {
                "S" => {
                    let hash = hash.context("a file in the stream has a hash")?;
                    self.stream.insert(path.to_string(), Known { hash, writer: writer.to_string() });
                }
                "F" => {
                    self.in_flight.insert(path.to_string(), hash);
                }
                "H" => {
                    self.held.insert(path.to_string(), hash);
                }
                other => bail!("unknown index line kind {other:?}"),
            }
        }
        Ok(())
    }

    /// The index as lines - `S` the stream's version, `F` in flight, `H` held
    /// - written whole, then renamed over the old one.
    fn save(&self) -> Result<()> {
        let mut text = String::new();
        let show = |hash: Option<Digest32>| hash.map_or_else(|| "-".to_string(), |h| hex(&h));
        for (path, known) in &self.stream {
            text.push_str(&format!("S {} {} {path}\n", hex(&known.hash), known.writer));
        }
        for (path, hash) in &self.in_flight {
            text.push_str(&format!("F {} - {path}\n", show(*hash)));
        }
        for (path, hash) in &self.held {
            text.push_str(&format!("H {} - {path}\n", show(*hash)));
        }
        write_file(&self.index_path, text.as_bytes())
    }
}

/// Every file under `folder`, by path relative to it, with its hash.
pub fn scan(folder: &Path) -> Result<BTreeMap<String, Digest32>> {
    fn walk(folder: &Path, dir: &Path, found: &mut BTreeMap<String, Digest32>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                walk(folder, &path, found)?;
            } else if kind.is_file() && !entry.file_name().to_string_lossy().ends_with(TEMP_SUFFIX) {
                let relative = path.strip_prefix(folder).expect("walked from the folder");
                let parts: Vec<String> =
                    relative.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                let content = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                found.insert(parts.join("/"), hash(&content));
            }
        }
        Ok(())
    }
    let mut found = BTreeMap::new();
    walk(folder, folder, &mut found)?;
    Ok(found)
}

/// A path another device sent, held to what is safe to write under the
/// folder: relative, no `..`, nothing a filesystem reads as special.
pub fn check_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > 1024 {
        return Err(format!("the path {path:?} is empty or too long"));
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("the path {path:?} leaves the folder or has an empty part"));
        }
        if part.chars().any(|c| matches!(c, '\\' | ':' | '\0' | '\n' | '\r')) {
            return Err(format!("the path {path:?} has a character this folder does not take"));
        }
        if part.ends_with(TEMP_SUFFIX) {
            return Err(format!("the path {path:?} is a name this app keeps for itself"));
        }
    }
    Ok(())
}

/// `notes/plan.md` kept for device `9b1e2c3d` as
/// `notes/plan (conflict 9b1e2c3d).md`.
pub fn conflict_name(path: &str, writer: &str) -> String {
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(dir, name)| (dir, name));
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    let renamed = match extension {
        Some(extension) => format!("{stem} (conflict {writer}).{extension}"),
        None => format!("{stem} (conflict {writer})"),
    };
    if dir.is_empty() { renamed } else { format!("{dir}/{renamed}") }
}

fn hash(content: &[u8]) -> Digest32 {
    Sha256::digest(content).into()
}

fn file_hash(path: &Path) -> Result<Option<Digest32>> {
    match fs::read(path) {
        Ok(content) => Ok(Some(hash(&content))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes next to the file and renames over it: a reader - or a crash - never
/// finds half a file.
fn write_file(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut temp = path.as_os_str().to_os_string();
    temp.push(TEMP_SUFFIX);
    fs::write(&temp, content).with_context(|| format!("writing {}", path.display()))?;
    fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_hash(text: &str) -> Result<Option<Digest32>> {
    if text == "-" {
        return Ok(None);
    }
    if text.len() != 64 {
        bail!("{text:?} is not a hash");
    }
    let mut hash = [0u8; 32];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).with_context(|| format!("{text:?} is not a hash"))?;
    }
    Ok(Some(hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conflict_copy_sits_next_to_its_file() {
        assert_eq!(conflict_name("notes/plan.md", "9b1e2c3d"), "notes/plan (conflict 9b1e2c3d).md");
        assert_eq!(conflict_name("README", "9b1e2c3d"), "README (conflict 9b1e2c3d)");
        assert_eq!(conflict_name(".env", "9b1e2c3d"), ".env (conflict 9b1e2c3d)");
        assert_eq!(conflict_name("a/b.tar.gz", "x"), "a/b.tar (conflict x).gz");
    }

    #[test]
    fn a_path_from_another_device_stays_in_the_folder() {
        for good in ["a.md", "notes/2026/plan.md", ".hidden", "with space.txt"] {
            assert!(check_path(good).is_ok(), "{good}");
        }
        for bad in ["", "/etc/passwd", "../up", "a/../../b", "a//b", "C:/x", "a\\b", "a/.", "x.mini-vault-tmp"] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }
}
