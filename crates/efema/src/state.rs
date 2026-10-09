//! What a device remembers between runs: who it is, and for each stream the
//! incarnation it synced with, the stream's locked key and how far it has
//! read.
//!
//! One SQLite file per device. The key is kept only locked - under the
//! passphrase, as the relay keeps it - so the file opens nothing on its own,
//! and a device can unlock its key without the network. When the passphrase
//! changes, the device keeps the newest lock it has read.

use std::fmt;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use efema_proto::{Cursor, Hash, StreamId, StreamName};
use rusqlite::{Connection, OptionalExtension, params};

use crate::Error;

/// The schema version this release writes and reads (`PRAGMA user_version`).
const SCHEMA_VERSION: i64 = 2;

/// Migrations, in order: index `i` takes the file from version `i` to
/// `i + 1`. An applied migration is never edited - a change is a new one.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE device (
    only       INTEGER PRIMARY KEY CHECK (only = 1),
    id         BLOB    NOT NULL CHECK (length(id) = 16),
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE streams (
    name       TEXT    PRIMARY KEY,
    stream     BLOB    NOT NULL CHECK (length(stream) = 16),
    lock       BLOB    NOT NULL,
    seq        INTEGER NOT NULL DEFAULT 0 CHECK (seq >= 0),
    hash       BLOB    NOT NULL CHECK (length(hash) = 32),
    updated_at INTEGER NOT NULL
) STRICT;
"#,
    r#"
-- Which change of passphrase the lock is from: 0 for the lock the stream was
-- created with. A device only ever moves to a newer one.
ALTER TABLE streams ADD COLUMN generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0);
"#,
];

/// Which device this is: sixteen random bytes drawn the first time its state
/// is opened.
///
/// Sealed into every entry the device writes - the relay never sees it - so
/// a reader knows which device wrote what: its own entries coming back, and a
/// tie-breaker when two devices changed the same thing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId([u8; 16]);

impl DeviceId {
    /// Wraps sixteen bytes.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The sixteen bytes.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The first eight hex digits - enough to tell devices apart in a
    /// listing.
    pub fn short(&self) -> String {
        self.to_string()[..8].to_string()
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

/// A device's sync state: one file, opened by one process at a time.
///
/// Open it once and share it - it is cheap to clone - between the clients of
/// every stream the app syncs.
#[derive(Clone)]
pub struct State {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    device: DeviceId,
    conn: Mutex<Connection>,
    // Held for as long as the state is open: two processes on one file would
    // be one device writing twice, and each would move the other's cursors.
    _lock: File,
}

/// What a device knows about one stream.
#[derive(Clone, Debug)]
pub(crate) struct Known {
    /// The incarnation the device synced with.
    pub(crate) stream: StreamId,
    /// The stream's key, locked under the passphrase.
    pub(crate) lock: Vec<u8>,
    /// The lock's generation.
    pub(crate) generation: u32,
    /// How far the device has read and acknowledged.
    pub(crate) cursor: Cursor,
}

impl State {
    /// Opens the state in `path`, creating the file - and the device's
    /// identity - the first time.
    ///
    /// # Errors
    ///
    /// [`Error::StateBusy`] when another process has it open;
    /// [`Error::StateNewer`] when a newer release wrote it; [`Error::State`]
    /// when SQLite fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        let lock_path = lock_path(&path);
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|source| Error::StateLock { path: lock_path.clone(), source })?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::StateBusy(path)),
            Err(TryLockError::Error(source)) => return Err(Error::StateLock { path: lock_path, source }),
        }

        let sqlite = |source| Error::State { path: path.clone(), source };
        let mut conn = Connection::open(&path).map_err(sqlite)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(sqlite)?;
        // An acknowledged cursor is the device's promise not to be handed
        // those entries again: it has to survive a power cut.
        conn.pragma_update(None, "synchronous", "FULL").map_err(sqlite)?;
        migrate(&mut conn, &path)?;

        let existing: Option<Vec<u8>> =
            conn.query_row("SELECT id FROM device", [], |row| row.get(0)).optional().map_err(sqlite)?;
        let device = match existing {
            Some(bytes) => DeviceId(bytes.try_into().expect("the schema keeps a device identity at 16 bytes")),
            None => {
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes).map_err(|_| Error::Random)?;
                conn.execute(
                    "INSERT INTO device (only, id, created_at) VALUES (1, ?1, ?2)",
                    params![bytes, unix_now()],
                )
                .map_err(sqlite)?;
                DeviceId(bytes)
            }
        };
        Ok(Self { inner: Arc::new(Inner { path, device, conn: Mutex::new(conn), _lock: lock }) })
    }

    /// This device's identity.
    pub fn device(&self) -> DeviceId {
        self.inner.device
    }

    /// Where the state lives.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Forgets everything this device knew about `stream`: the incarnation,
    /// the locked key, the cursor. The next [`Client::open`](crate::Client::open)
    /// starts afresh - it finds the stream's key on the relay, or creates one,
    /// and reads from the first entry.
    ///
    /// For a stream the relay no longer has as this device knew it
    /// ([`Error::StreamGone`], [`Error::StreamReplaced`],
    /// [`Error::CursorAhead`], [`Error::CursorDiverged`]): what the app does
    /// with its own data after that - send it again, merge it with what it
    /// reads - is the app's to decide.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when SQLite fails.
    pub fn forget(&self, stream: &StreamName) -> Result<(), Error> {
        self.conn()
            .execute("DELETE FROM streams WHERE name = ?1", [stream.as_str()])
            .map(drop)
            .map_err(|source| self.error(source))
    }

    pub(crate) fn known(&self, stream: &StreamName) -> Result<Option<Known>, Error> {
        self.conn()
            .query_row(
                "SELECT stream, lock, seq, hash, generation FROM streams WHERE name = ?1",
                [stream.as_str()],
                |row| {
                    let id = StreamId::from_bytes(blob(row.get(0)?)?);
                    let seq: i64 = row.get(2)?;
                    let generation: i64 = row.get(4)?;
                    Ok(Known {
                        stream: id,
                        lock: row.get(1)?,
                        generation: u32::try_from(generation).expect("generations are written from a u32"),
                        cursor: Cursor {
                            stream: id,
                            seq: u64::try_from(seq).expect("the schema keeps positions non-negative"),
                            hash: Hash::from_bytes(blob(row.get(3)?)?),
                        },
                    })
                },
            )
            .optional()
            .map_err(|source| self.error(source))
    }

    /// Records the incarnation and the key of a stream this device has just
    /// joined, with its cursor before the first entry.
    pub(crate) fn join(&self, stream: &StreamName, id: StreamId, lock: &[u8], generation: u32) -> Result<(), Error> {
        let start = Cursor::start(id);
        self.conn()
            .execute(
                "INSERT INTO streams (name, stream, lock, generation, seq, hash, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
                params![stream.as_str(), id.as_bytes(), lock, generation, start.hash.as_bytes(), unix_now()],
            )
            .map(drop)
            .map_err(|source| self.error(source))
    }

    /// Keeps a newer lock of the stream's key. A lock of an older generation,
    /// or of another incarnation, changes nothing: entries read again after a
    /// restart never take the device back to a passphrase it moved on from.
    pub(crate) fn relock(&self, stream: &StreamName, id: StreamId, lock: &[u8], generation: u32) -> Result<(), Error> {
        self.conn()
            .execute(
                "UPDATE streams SET lock = ?1, generation = ?2, updated_at = ?3
                 WHERE name = ?4 AND stream = ?5 AND generation < ?2",
                params![lock, generation, unix_now(), stream.as_str(), id.as_bytes()],
            )
            .map(drop)
            .map_err(|source| self.error(source))
    }

    /// Moves the cursor of `stream` forward to `cursor`. A cursor of another
    /// incarnation, or one behind the recorded one, changes nothing: an ack
    /// only ever moves the device on through the history it joined.
    pub(crate) fn advance(&self, stream: &StreamName, cursor: &Cursor) -> Result<(), Error> {
        self.conn()
            .execute(
                "UPDATE streams SET seq = ?1, hash = ?2, updated_at = ?3
                 WHERE name = ?4 AND stream = ?5 AND seq < ?1",
                params![
                    i64::try_from(cursor.seq).expect("positions stay below 2^63"),
                    cursor.hash.as_bytes(),
                    unix_now(),
                    stream.as_str(),
                    cursor.stream.as_bytes()
                ],
            )
            .map(drop)
            .map_err(|source| self.error(source))
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock means a thread panicked holding it; SQLite rolled
        // back whatever it was doing, and the connection is still good.
        self.inner.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn error(&self, source: rusqlite::Error) -> Error {
        Error::State { path: self.inner.path.clone(), source }
    }
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State").field("path", &self.inner.path).field("device", &self.inner.device).finish()
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".lock");
    path.with_file_name(name)
}

fn migrate(conn: &mut Connection, path: &Path) -> Result<(), Error> {
    let sqlite = |source| Error::State { path: path.to_path_buf(), source };
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(sqlite)?;
    if version > SCHEMA_VERSION {
        return Err(Error::StateNewer { path: path.to_path_buf(), found: version });
    }
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction().map_err(sqlite)?;
        tx.execute_batch(migration).map_err(sqlite)?;
        tx.pragma_update(None, "user_version", index as i64 + 1).map_err(sqlite)?;
        tx.commit().map_err(sqlite)?;
    }
    Ok(())
}

fn blob<const N: usize>(bytes: Vec<u8>) -> rusqlite::Result<[u8; N]> {
    bytes.try_into().map_err(|_| rusqlite::Error::InvalidColumnType(0, "blob".into(), rusqlite::types::Type::Blob))
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> StreamName {
        text.parse().unwrap()
    }

    #[test]
    fn the_device_keeps_its_identity_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.sqlite3");
        let first = State::open(&path).unwrap().device();
        let again = State::open(&path).unwrap().device();
        assert_eq!(first, again);
        let other = State::open(dir.path().join("other.sqlite3")).unwrap().device();
        assert_ne!(first, other);
    }

    #[test]
    fn one_process_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.sqlite3");
        let held = State::open(&path).unwrap();
        assert!(matches!(State::open(&path), Err(Error::StateBusy(_))));
        drop(held);
        State::open(&path).unwrap();
    }

    #[test]
    fn a_cursor_only_moves_forward_within_its_incarnation() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(dir.path().join("sync.sqlite3")).unwrap();
        let id = StreamId::from_bytes([3; 16]);
        state.join(&name("s"), id, b"lock", 0).unwrap();
        let at = |seq| Cursor { stream: id, seq, hash: Hash::from_bytes([seq as u8; 32]) };

        state.advance(&name("s"), &at(5)).unwrap();
        state.advance(&name("s"), &at(3)).unwrap();
        assert_eq!(state.known(&name("s")).unwrap().unwrap().cursor, at(5));

        let foreign = Cursor { stream: StreamId::from_bytes([4; 16]), ..at(9) };
        state.advance(&name("s"), &foreign).unwrap();
        assert_eq!(state.known(&name("s")).unwrap().unwrap().cursor, at(5));

        state.forget(&name("s")).unwrap();
        assert!(state.known(&name("s")).unwrap().is_none());
    }

    #[test]
    fn a_lock_only_moves_to_a_newer_generation() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(dir.path().join("sync.sqlite3")).unwrap();
        let id = StreamId::from_bytes([3; 16]);
        state.join(&name("s"), id, b"first", 0).unwrap();
        let lock = || {
            let known = state.known(&name("s")).unwrap().unwrap();
            (known.lock, known.generation)
        };

        state.relock(&name("s"), id, b"second", 2).unwrap();
        assert_eq!(lock(), (b"second".to_vec(), 2));
        for generation in [1, 2] {
            state.relock(&name("s"), id, b"replayed", generation).unwrap();
            assert_eq!(lock(), (b"second".to_vec(), 2), "generation {generation} moved the lock back");
        }
        state.relock(&name("s"), StreamId::from_bytes([4; 16]), b"foreign", 9).unwrap();
        assert_eq!(lock(), (b"second".to_vec(), 2), "a lock of another incarnation");
    }

    #[test]
    fn a_state_of_the_previous_release_is_carried_forward() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute("INSERT INTO device (only, id, created_at) VALUES (1, zeroblob(16), 0)", []).unwrap();
            conn.execute(
                "INSERT INTO streams (name, stream, lock, seq, hash, updated_at)
                 VALUES ('s', zeroblob(16), x'4c', 4, zeroblob(32), 0)",
                [],
            )
            .unwrap();
        }
        let state = State::open(&path).unwrap();
        let known = state.known(&name("s")).unwrap().unwrap();
        assert_eq!((known.lock, known.generation, known.cursor.seq), (b"L".to_vec(), 0, 4));
    }

    #[test]
    fn a_newer_state_is_refused_rather_than_misread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.sqlite3");
        drop(State::open(&path).unwrap());
        Connection::open(&path).unwrap().pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        assert!(matches!(State::open(&path), Err(Error::StateNewer { .. })));
    }
}
