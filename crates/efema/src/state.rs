//! What a device remembers between runs: who it is, and for each stream the
//! incarnation it synced with, the stream's locked key, how far it has read,
//! and what it has yet to send.
//!
//! One SQLite file per device. The key is kept only locked - under the
//! passphrase, as the relay keeps it - so the file opens nothing on its own,
//! and a device can unlock its key without the network. When the passphrase
//! changes, the device keeps the newest lock it has read.
//!
//! What waits to be sent is kept sealed, cut into the batches it will be
//! sent as: a batch sent again after a lost answer has to be the same bytes
//! for the relay to know it, so nothing in a queued batch ever changes. The
//! one exception is a queue the relay refused as too large: the relay does
//! not have a batch it refused, and none after it was sent, so the whole
//! queue is cut again - in one transaction, in the same order.
//!
//! Next to each stream, the limits the relay last announced for the device's
//! access key: a push cuts its batches to them without asking the network.

use std::collections::HashMap;
use std::fmt;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use efema_proto::wire::{self, Limits};
use efema_proto::{Cursor, Epoch, Hash, StreamId, StreamName};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::Error;

/// The schema version this release writes and reads (`PRAGMA user_version`).
const SCHEMA_VERSION: i64 = 4;

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
    r#"
-- Batches sealed and cut, waiting to be sent, in the order they were queued.
-- Each belongs to the incarnation of the stream it was sealed for: forgetting
-- the stream takes its queue with it, and a queue cannot outlive the stream it
-- would be sent into.
CREATE UNIQUE INDEX streams_incarnation ON streams (name, stream);

CREATE TABLE unsent (
    id        INTEGER PRIMARY KEY,
    name      TEXT    NOT NULL,
    stream    BLOB    NOT NULL,
    epoch     INTEGER NOT NULL CHECK (epoch BETWEEN 0 AND 4294967295),
    queued_at INTEGER NOT NULL,
    FOREIGN KEY (name, stream) REFERENCES streams (name, stream) ON DELETE CASCADE
) STRICT;

CREATE INDEX unsent_in_order ON unsent (name, id);

CREATE TABLE unsent_entries (
    batch INTEGER NOT NULL REFERENCES unsent (id) ON DELETE CASCADE,
    idx   INTEGER NOT NULL CHECK (idx >= 0),
    data  BLOB    NOT NULL,
    PRIMARY KEY (batch, idx)
) STRICT;

-- A batch is sent again as it was cut, byte for byte: the relay knows a batch
-- it has by what it holds. Entries join only the batch being queued, and
-- nothing queued changes.
CREATE TRIGGER unsent_entries_join_the_newest_batch
BEFORE INSERT ON unsent_entries
BEGIN
    SELECT RAISE(ABORT, 'entries join only the batch being queued')
    WHERE NEW.batch <> (SELECT max(id) FROM unsent);
END;

CREATE TRIGGER unsent_is_immutable
BEFORE UPDATE ON unsent
BEGIN
    SELECT RAISE(ABORT, 'a queued batch cannot be changed');
END;

CREATE TRIGGER unsent_entries_are_immutable
BEFORE UPDATE ON unsent_entries
BEGIN
    SELECT RAISE(ABORT, 'a queued entry cannot be changed');
END;

-- Batches leave the queue as they are sent, in order: only the oldest of a
-- stream, unless the stream itself is forgotten. An entry leaves only with its
-- batch.
CREATE TRIGGER unsent_leaves_in_order
BEFORE DELETE ON unsent
BEGIN
    SELECT RAISE(ABORT, 'only the oldest queued batch of a stream leaves the queue')
    WHERE EXISTS (SELECT 1 FROM streams WHERE name = OLD.name AND stream = OLD.stream)
      AND OLD.id <> (SELECT min(id) FROM unsent WHERE name = OLD.name);
END;

CREATE TRIGGER unsent_entries_leave_with_their_batch
BEFORE DELETE ON unsent_entries
BEGIN
    SELECT RAISE(ABORT, 'a queued entry leaves only with its batch')
    WHERE EXISTS (SELECT 1 FROM unsent WHERE id = OLD.batch);
END;
"#,
    r#"
-- The limits the relay last announced for the device's access key, as it
-- sent them (CBOR), and when: a push cuts its batches to them without the
-- network. Empty until the relay is first asked.
ALTER TABLE streams ADD COLUMN limits BLOB;
ALTER TABLE streams ADD COLUMN limits_at INTEGER;
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
    /// One sender at a time per stream: two clients of a stream in one
    /// process take turns rather than send the same batch twice.
    sending: Mutex<HashMap<StreamName, Arc<tokio::sync::Mutex<()>>>>,
    // Held for as long as the state is open: two processes on one file would
    // be one device writing twice, and each would move the other's cursors.
    _lock: File,
}

/// A batch waiting to be sent: its entries sealed, as they will go.
#[derive(Clone, Debug)]
pub(crate) struct Waiting {
    pub(crate) id: i64,
    pub(crate) epoch: Epoch,
    pub(crate) entries: Vec<Vec<u8>>,
}

/// What waits on a device to be sent to one stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Queue {
    /// Batches, each to be sent whole.
    pub batches: usize,
    /// Items in them.
    pub items: usize,
    /// Their size as they will be sent - sealed, and compressed where that
    /// helped.
    pub bytes: u64,
    /// When the oldest of them was pushed; `None` when nothing waits.
    pub oldest: Option<SystemTime>,
}

impl Queue {
    /// Whether nothing waits.
    pub fn is_empty(&self) -> bool {
        self.items == 0
    }
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
        // A queue is held to the stream it belongs to by foreign keys, which
        // SQLite checks only when asked to.
        conn.pragma_update(None, "foreign_keys", "ON").map_err(sqlite)?;
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
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                device,
                conn: Mutex::new(conn),
                sending: Mutex::new(HashMap::new()),
                _lock: lock,
            }),
        })
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
    /// the locked key, the cursor, and what was waiting to be sent to it. The
    /// next [`Client::open`](crate::Client::open) starts afresh - it finds the
    /// stream's key on the relay, or creates one, and reads from the first
    /// entry.
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

    /// Queues `batches` - sealed, cut, each to be sent whole - for the
    /// incarnation `id` of `stream`, in one transaction: all of them, or
    /// none.
    pub(crate) fn queue(
        &self,
        stream: &StreamName,
        id: StreamId,
        epoch: Epoch,
        batches: &[Vec<Vec<u8>>],
    ) -> Result<(), Error> {
        let mut conn = self.conn();
        let result = (|| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let now = unix_now();
            for entries in batches {
                tx.execute(
                    "INSERT INTO unsent (name, stream, epoch, queued_at) VALUES (?1, ?2, ?3, ?4)",
                    params![stream.as_str(), id.as_bytes(), epoch.0, now],
                )?;
                let batch = tx.last_insert_rowid();
                let mut insert =
                    tx.prepare_cached("INSERT INTO unsent_entries (batch, idx, data) VALUES (?1, ?2, ?3)")?;
                for (index, data) in entries.iter().enumerate() {
                    insert.execute(params![batch, index as i64, data])?;
                }
            }
            tx.commit()
        })();
        result.map_err(|source| match source.sqlite_error().map(|e| e.extended_code) {
            // No such incarnation here: the app forgot the stream since the
            // client opened it.
            Some(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY) => Error::StreamGone { stream: stream.clone() },
            _ => self.error(source),
        })
    }

    /// The oldest batch waiting to be sent to the incarnation `id` of
    /// `stream` that was queued after the batch `after` - 0 for the oldest of
    /// all.
    pub(crate) fn next_unsent(&self, stream: &StreamName, id: StreamId, after: i64) -> Result<Option<Waiting>, Error> {
        let mut conn = self.conn();
        let read = (|| {
            let tx = conn.transaction()?;
            let Some((batch, epoch)) = tx
                .query_row(
                    "SELECT id, epoch FROM unsent WHERE name = ?1 AND stream = ?2 AND id > ?3 ORDER BY id LIMIT 1",
                    params![stream.as_str(), id.as_bytes(), after],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, u32>(1)?)),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let entries = tx
                .prepare("SELECT data FROM unsent_entries WHERE batch = ?1 ORDER BY idx")?
                .query_map([batch], |row| row.get(0))?
                .collect::<Result<Vec<Vec<u8>>, _>>()?;
            Ok(Some(Waiting { id: batch, epoch: Epoch(epoch), entries }))
        })();
        read.map_err(|source| self.error(source))
    }

    /// The limits the relay last announced for `stream`, and when; `None` if
    /// it was never asked - or answered something this release does not
    /// read, which asking again will mend.
    pub(crate) fn limits(&self, stream: &StreamName) -> Result<Option<(Limits, SystemTime)>, Error> {
        let row: Option<(Option<Vec<u8>>, Option<i64>)> = self
            .conn()
            .query_row("SELECT limits, limits_at FROM streams WHERE name = ?1", [stream.as_str()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()
            .map_err(|source| self.error(source))?;
        Ok(match row {
            Some((Some(bytes), Some(at))) => wire::decode::<Limits>(&bytes)
                .ok()
                .map(|limits| (limits, UNIX_EPOCH + Duration::from_secs(at.max(0) as u64))),
            _ => None,
        })
    }

    /// Keeps the limits the relay announced for the incarnation `id` of
    /// `stream`.
    pub(crate) fn keep_limits(&self, stream: &StreamName, id: StreamId, limits: &Limits) -> Result<(), Error> {
        self.conn()
            .execute(
                "UPDATE streams SET limits = ?1, limits_at = ?2 WHERE name = ?3 AND stream = ?4",
                params![wire::encode(limits), unix_now(), stream.as_str(), id.as_bytes()],
            )
            .map(drop)
            .map_err(|source| self.error(source))
    }

    /// Cuts what waits for the incarnation `id` of `stream` again, in one
    /// transaction: the entries in the order they were queued, each run of
    /// one epoch handed to `cut`, which says how many entries go in each new
    /// batch. A batch keeps the time its first entry was queued. When `cut`
    /// fails, the queue stays as it was.
    ///
    /// Only for a queue the relay refused as too large: a batch that left
    /// the device may be on the relay, and cut again it would land twice.
    pub(crate) fn recut(
        &self,
        stream: &StreamName,
        id: StreamId,
        mut cut: impl FnMut(Epoch, &[Vec<u8>]) -> Result<Vec<usize>, Error>,
    ) -> Result<(), Error> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|source| self.error(source))?;
        let read = (|| {
            let batches = tx
                .prepare("SELECT id, epoch, queued_at FROM unsent WHERE name = ?1 AND stream = ?2 ORDER BY id")?
                .query_map(params![stream.as_str(), id.as_bytes()], |row| {
                    Ok((row.get::<_, i64>(0)?, Epoch(row.get(1)?), row.get::<_, i64>(2)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            // Runs of one epoch: a batch is written in one epoch, so no cut
            // may join two.
            let mut runs: Vec<(Epoch, Vec<Vec<u8>>, Vec<i64>)> = Vec::new();
            for (batch, epoch, queued_at) in &batches {
                let entries = tx
                    .prepare("SELECT data FROM unsent_entries WHERE batch = ?1 ORDER BY idx")?
                    .query_map([batch], |row| row.get(0))?
                    .collect::<Result<Vec<Vec<u8>>, _>>()?;
                match runs.last_mut() {
                    Some((last, data, times)) if last == epoch => {
                        times.extend(std::iter::repeat_n(*queued_at, entries.len()));
                        data.extend(entries);
                    }
                    _ => runs.push((*epoch, entries.clone(), vec![*queued_at; entries.len()])),
                }
            }
            Ok::<_, rusqlite::Error>((batches, runs))
        })();
        let (batches, runs) = read.map_err(|source| self.error(source))?;

        let mut cut_again: Vec<(Epoch, i64, Vec<Vec<u8>>)> = Vec::new();
        for (epoch, data, times) in runs {
            let counts = cut(epoch, &data)?;
            debug_assert_eq!(counts.iter().sum::<usize>(), data.len(), "a cut covers every entry once");
            let mut entries = data.into_iter();
            let mut first = 0;
            for count in counts {
                cut_again.push((epoch, times[first], entries.by_ref().take(count).collect()));
                first += count;
            }
        }

        let write = (|| {
            // Oldest first: the schema lets batches leave the queue in order
            // only.
            for (batch, _, _) in &batches {
                tx.execute("DELETE FROM unsent WHERE id = ?1", [batch])?;
            }
            for (epoch, queued_at, entries) in &cut_again {
                tx.execute(
                    "INSERT INTO unsent (name, stream, epoch, queued_at) VALUES (?1, ?2, ?3, ?4)",
                    params![stream.as_str(), id.as_bytes(), epoch.0, queued_at],
                )?;
                let batch = tx.last_insert_rowid();
                let mut insert =
                    tx.prepare_cached("INSERT INTO unsent_entries (batch, idx, data) VALUES (?1, ?2, ?3)")?;
                for (index, data) in entries.iter().enumerate() {
                    insert.execute(params![batch, index as i64, data])?;
                }
            }
            Ok::<_, rusqlite::Error>(())
        })();
        write.map_err(|source| self.error(source))?;
        tx.commit().map_err(|source| self.error(source))
    }

    /// Takes a batch the relay has out of the queue.
    pub(crate) fn sent(&self, batch: i64) -> Result<(), Error> {
        self.conn().execute("DELETE FROM unsent WHERE id = ?1", [batch]).map(drop).map_err(|source| self.error(source))
    }

    /// What waits to be sent to `stream`.
    pub(crate) fn queued(&self, stream: &StreamName) -> Result<Queue, Error> {
        self.conn()
            .query_row(
                "SELECT count(DISTINCT u.id), count(e.idx), coalesce(sum(length(e.data)), 0), min(u.queued_at)
                 FROM unsent u JOIN unsent_entries e ON e.batch = u.id
                 WHERE u.name = ?1",
                [stream.as_str()],
                |row| {
                    let since: Option<i64> = row.get(3)?;
                    Ok(Queue {
                        batches: row.get::<_, i64>(0)? as usize,
                        items: row.get::<_, i64>(1)? as usize,
                        bytes: row.get::<_, i64>(2)? as u64,
                        oldest: since.map(|secs| UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64)),
                    })
                },
            )
            .map_err(|source| self.error(source))
    }

    /// The turn to send to `stream`, shared by every client of it in this
    /// process.
    pub(crate) fn sending(&self, stream: &StreamName) -> Arc<tokio::sync::Mutex<()>> {
        let mut sending = self.inner.sending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        sending.entry(stream.clone()).or_default().clone()
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

    fn entries(texts: &[&str]) -> Vec<Vec<u8>> {
        texts.iter().map(|t| t.as_bytes().to_vec()).collect()
    }

    #[test]
    fn batches_wait_in_order_and_leave_as_they_are_sent() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(dir.path().join("sync.sqlite3")).unwrap();
        let id = StreamId::from_bytes([3; 16]);
        state.join(&name("s"), id, b"lock", 0).unwrap();
        assert!(state.next_unsent(&name("s"), id, 0).unwrap().is_none());

        state.queue(&name("s"), id, Epoch(1), &[entries(&["a", "b"]), entries(&["c"])]).unwrap();
        state.queue(&name("s"), id, Epoch(2), &[entries(&["d"])]).unwrap();
        let queued = state.queued(&name("s")).unwrap();
        assert_eq!((queued.batches, queued.items, queued.bytes), (3, 4, 4));
        assert!(queued.oldest.is_some());

        // Passing over a batch leaves it in the queue, for the next send.
        let first = state.next_unsent(&name("s"), id, 0).unwrap().unwrap();
        let second = state.next_unsent(&name("s"), id, first.id).unwrap().unwrap();
        assert_eq!(second.entries, entries(&["c"]));
        assert!(state.next_unsent(&name("s"), id, i64::MAX).unwrap().is_none());

        let mut seen = Vec::new();
        while let Some(waiting) = state.next_unsent(&name("s"), id, 0).unwrap() {
            seen.push((waiting.epoch, waiting.entries.clone()));
            state.sent(waiting.id).unwrap();
        }
        assert_eq!(seen, [(Epoch(1), entries(&["a", "b"])), (Epoch(1), entries(&["c"])), (Epoch(2), entries(&["d"]))]);
        assert_eq!(state.queued(&name("s")).unwrap(), Queue::default());
    }

    #[test]
    fn a_queued_batch_stays_as_it_was_cut() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(dir.path().join("sync.sqlite3")).unwrap();
        let id = StreamId::from_bytes([3; 16]);
        state.join(&name("s"), id, b"lock", 0).unwrap();
        state.queue(&name("s"), id, Epoch(1), &[entries(&["a", "b"]), entries(&["c"])]).unwrap();
        let conn = state.conn();
        let refused = |sql: &str, why: &str| {
            let error = conn.execute(sql, []).unwrap_err();
            assert!(error.to_string().contains(why), "{sql}: {error}");
        };
        refused("UPDATE unsent_entries SET data = x'00'", "cannot be changed");
        refused("UPDATE unsent SET epoch = 2", "cannot be changed");
        refused("INSERT INTO unsent_entries (batch, idx, data) VALUES (1, 2, x'00')", "only the batch being queued");
        refused("DELETE FROM unsent_entries WHERE batch = 1 AND idx = 1", "only with its batch");
        refused("DELETE FROM unsent WHERE id = 2", "only the oldest");
    }

    #[test]
    fn a_queue_belongs_to_its_incarnation_and_goes_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(dir.path().join("sync.sqlite3")).unwrap();
        let id = StreamId::from_bytes([3; 16]);
        let other = StreamId::from_bytes([4; 16]);
        state.join(&name("s"), id, b"lock", 0).unwrap();
        let refused = state.queue(&name("s"), other, Epoch(1), &[entries(&["a"])]);
        assert!(matches!(refused, Err(Error::StreamGone { .. })), "{refused:?}");
        let refused = state.queue(&name("t"), id, Epoch(1), &[entries(&["a"])]);
        assert!(matches!(refused, Err(Error::StreamGone { .. })), "{refused:?}");
        state.queue(&name("s"), id, Epoch(1), &[entries(&["a"]), entries(&["b"]), entries(&["c"])]).unwrap();
        assert!(state.next_unsent(&name("s"), other, 0).unwrap().is_none(), "a queue went to another incarnation");
        state.forget(&name("s")).unwrap();
        state.join(&name("s"), other, b"lock", 0).unwrap();
        assert_eq!(state.queued(&name("s")).unwrap(), Queue::default(), "a forgotten stream left its queue behind");
        let left: i64 = state.conn().query_row("SELECT count(*) FROM unsent_entries", [], |row| row.get(0)).unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn a_state_of_the_release_before_gets_a_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(MIGRATIONS[1]).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute("INSERT INTO device (only, id, created_at) VALUES (1, zeroblob(16), 0)", []).unwrap();
            conn.execute(
                "INSERT INTO streams (name, stream, lock, generation, seq, hash, updated_at)
                 VALUES ('s', zeroblob(16), x'4c', 1, 4, zeroblob(32), 0)",
                [],
            )
            .unwrap();
        }
        let state = State::open(&path).unwrap();
        let id = StreamId::from_bytes([0; 16]);
        assert_eq!(state.known(&name("s")).unwrap().unwrap().generation, 1);
        state.queue(&name("s"), id, Epoch(1), &[entries(&["a"])]).unwrap();
        assert_eq!(state.queued(&name("s")).unwrap().items, 1);
    }

    #[test]
    fn the_announced_limits_are_kept_for_the_incarnation() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(dir.path().join("sync.sqlite3")).unwrap();
        let id = StreamId::from_bytes([3; 16]);
        state.join(&name("s"), id, b"lock", 0).unwrap();
        assert!(state.limits(&name("s")).unwrap().is_none());
        let limits = Limits { write: false, body: 2000, ..Limits::DEFAULT };
        state.keep_limits(&name("s"), StreamId::from_bytes([4; 16]), &limits).unwrap();
        assert!(state.limits(&name("s")).unwrap().is_none(), "kept for another incarnation");
        state.keep_limits(&name("s"), id, &limits).unwrap();
        let (kept, at) = state.limits(&name("s")).unwrap().unwrap();
        assert_eq!(kept, limits);
        assert!(SystemTime::now().duration_since(at).unwrap() < Duration::from_secs(60));
        // Something this release cannot read is as good as nothing.
        state.conn().execute("UPDATE streams SET limits = x'00'", []).unwrap();
        assert!(state.limits(&name("s")).unwrap().is_none());
    }

    #[test]
    fn a_queue_is_cut_again_in_its_order_and_epochs() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(dir.path().join("sync.sqlite3")).unwrap();
        let id = StreamId::from_bytes([3; 16]);
        state.join(&name("s"), id, b"lock", 0).unwrap();
        state.join(&name("t"), StreamId::from_bytes([5; 16]), b"lock", 0).unwrap();
        state.queue(&name("s"), id, Epoch(1), &[entries(&["a", "b", "c"]), entries(&["d"])]).unwrap();
        state.queue(&name("t"), StreamId::from_bytes([5; 16]), Epoch(1), &[entries(&["other"])]).unwrap();
        state.queue(&name("s"), id, Epoch(2), &[entries(&["e", "f"])]).unwrap();

        // One entry a batch.
        let mut runs = Vec::new();
        state
            .recut(&name("s"), id, |epoch, data| {
                runs.push((epoch, data.len()));
                Ok(vec![1; data.len()])
            })
            .unwrap();
        assert_eq!(runs, [(Epoch(1), 4), (Epoch(2), 2)], "runs of one epoch, in order");
        let mut seen = Vec::new();
        while let Some(waiting) = state.next_unsent(&name("s"), id, 0).unwrap() {
            seen.push((waiting.epoch, waiting.entries.clone()));
            state.sent(waiting.id).unwrap();
        }
        let one = |e: u32, t: &str| (Epoch(e), entries(&[t]));
        assert_eq!(seen, [one(1, "a"), one(1, "b"), one(1, "c"), one(1, "d"), one(2, "e"), one(2, "f")]);
        assert_eq!(state.queued(&name("t")).unwrap().items, 1, "another stream's queue was touched");
    }

    #[test]
    fn a_cut_that_fails_leaves_the_queue_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(dir.path().join("sync.sqlite3")).unwrap();
        let id = StreamId::from_bytes([3; 16]);
        state.join(&name("s"), id, b"lock", 0).unwrap();
        state.queue(&name("s"), id, Epoch(1), &[entries(&["a", "b"])]).unwrap();
        let failed = state.recut(&name("s"), id, |_, _| Err(Error::ItemTooLarge { size: 9, limit: 1 }));
        assert!(matches!(failed, Err(Error::ItemTooLarge { .. })), "{failed:?}");
        let waiting = state.next_unsent(&name("s"), id, 0).unwrap().unwrap();
        assert_eq!(waiting.entries, entries(&["a", "b"]));
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
