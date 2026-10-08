//! The relay's storage: one SQLite database in the data directory.
//!
//! Two tables. `streams` holds one row per stream - its identity, epoch and
//! head; `entries` holds the entries, keyed by stream and position. The schema
//! itself enforces what the relay promises, so a bug in the code above it
//! fails a write instead of corrupting a stream:
//!
//! - an entry is inserted only at the position right after the head, so a
//!   stream has no gaps and no position twice;
//! - an entry's epoch is never below the stream's;
//! - an entry, once written, cannot be changed or deleted.
//!
//! Writes go through one connection, one transaction per batch; reads use
//! their own connections and see a consistent snapshot (WAL), so a reader
//! never waits for a writer and never sees half a batch.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use efema_proto::wire::{Entry, Page, Written};
use efema_proto::{Cursor, Epoch, Hash, StreamId, StreamName, chain};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

/// The database file inside the data directory.
pub const DATABASE_FILE: &str = "relay.sqlite3";

/// The schema version this build writes and reads (`PRAGMA user_version`).
const SCHEMA_VERSION: i64 = 1;

/// Migrations, in order: index `i` takes the database from version `i` to
/// `i + 1`. An applied migration is never edited - a change is a new one.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE streams (
    key        INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE,
    id         BLOB    NOT NULL UNIQUE CHECK (length(id) = 16),
    epoch      INTEGER NOT NULL CHECK (epoch BETWEEN 0 AND 4294967295),
    head       INTEGER NOT NULL DEFAULT 0 CHECK (head >= 0),
    head_hash  BLOB    NOT NULL CHECK (length(head_hash) = 32),
    bytes      INTEGER NOT NULL DEFAULT 0 CHECK (bytes >= 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE entries (
    stream INTEGER NOT NULL REFERENCES streams (key),
    seq    INTEGER NOT NULL CHECK (seq >= 1),
    epoch  INTEGER NOT NULL,
    hash   BLOB    NOT NULL CHECK (length(hash) = 32),
    data   BLOB    NOT NULL,
    PRIMARY KEY (stream, seq)
) STRICT;

-- Positions follow the head with no gap, epochs never go back.
CREATE TRIGGER entries_follow_the_head
BEFORE INSERT ON entries
BEGIN
    SELECT RAISE(ABORT, 'an entry must take the position right after the head')
    WHERE NEW.seq <> (SELECT head FROM streams WHERE key = NEW.stream) + 1;
    SELECT RAISE(ABORT, 'an entry cannot be written in an epoch older than its stream')
    WHERE NEW.epoch < (SELECT epoch FROM streams WHERE key = NEW.stream);
END;

-- The head moves with every entry, in the same statement that writes it.
CREATE TRIGGER entries_move_the_head
AFTER INSERT ON entries
BEGIN
    UPDATE streams
    SET head = NEW.seq, head_hash = NEW.hash, epoch = NEW.epoch, bytes = bytes + length(NEW.data)
    WHERE key = NEW.stream;
END;

CREATE TRIGGER entries_are_immutable
BEFORE UPDATE ON entries
BEGIN
    SELECT RAISE(ABORT, 'an entry cannot be changed');
END;

CREATE TRIGGER entries_are_permanent
BEFORE DELETE ON entries
BEGIN
    SELECT RAISE(ABORT, 'an entry cannot be deleted');
END;
"#];

/// What can go wrong opening the database.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("cannot open the database {path}")]
    Sqlite {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "the database {path} has schema version {found}, and this efema-server knows up to {SCHEMA_VERSION}: it was written by a newer release"
    )]
    Newer { path: PathBuf, found: i64 },
    #[error("there is no relay database in {0}")]
    Missing(PathBuf),
}

/// Why a write was refused.
#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    #[error("the stream is at epoch {current}, and the batch was written in epoch {batch}")]
    EpochBehind { current: Epoch, batch: Epoch },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Why a read was refused.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("there is no stream named {0}")]
    NotFound(StreamName),
    #[error("the cursor belongs to stream {cursor}, and {name} is now stream {current}")]
    Replaced { name: StreamName, cursor: StreamId, current: StreamId },
    #[error("the cursor is at position {cursor}, past the stream's last position {head}")]
    Ahead { cursor: u64, head: u64 },
    #[error("the stream's history at position {seq} is not the one the cursor holds")]
    Diverged { seq: u64 },
    #[error("stream {name} has a gap: expected position {expected}, found {found}")]
    Gap { name: StreamName, expected: u64, found: u64 },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// How much one read may return.
#[derive(Clone, Copy, Debug)]
pub struct PageLimit {
    /// At most this many entries.
    pub entries: usize,
    /// At most this many bytes of entry data - except that a page always
    /// carries at least one entry if there is one, or a single entry larger
    /// than the limit could never be read.
    pub bytes: usize,
}

/// A stream as the relay's listing shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSummary {
    pub name: String,
    pub id: StreamId,
    pub epoch: Epoch,
    pub head: u64,
    pub bytes: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// An entry as the relay's listing shows it: everything but the data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntrySummary {
    pub seq: u64,
    pub epoch: Epoch,
    pub size: u64,
    pub hash: Hash,
}

/// A stream's identity and head: what a waiting reader compares against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamState {
    pub id: Option<StreamId>,
    pub head: u64,
}

pub struct Store {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
}

/// One row of `streams`, as the code above it needs it.
struct StreamRow {
    key: i64,
    id: StreamId,
    epoch: Epoch,
    head: u64,
    head_hash: Hash,
}

impl Store {
    /// Opens the database in `dir`, creating it and bringing its schema up to
    /// date. Only the serving process does this.
    pub fn open(dir: &Path) -> Result<Self, OpenError> {
        let path = dir.join(DATABASE_FILE);
        let sqlite = |source| OpenError::Sqlite { path: path.clone(), source };
        let mut writer = Connection::open(&path).map_err(sqlite)?;
        configure(&writer).map_err(sqlite)?;
        // A write acknowledged to a client is one the client stops resending:
        // it has to survive a power cut, not only a crash. FULL syncs the WAL
        // on every commit.
        writer.pragma_update(None, "synchronous", "FULL").map_err(sqlite)?;
        migrate(&mut writer, &path)?;
        Ok(Self { path, writer: Mutex::new(writer), readers: Mutex::new(Vec::new()) })
    }

    /// Opens an existing database read-only, for looking at a relay - running
    /// or not - without any chance of changing it.
    pub fn open_read_only(dir: &Path) -> Result<Self, OpenError> {
        let path = dir.join(DATABASE_FILE);
        if !path.exists() {
            return Err(OpenError::Missing(dir.to_path_buf()));
        }
        let reader = open_reader(&path).map_err(|source| OpenError::Sqlite { path: path.clone(), source })?;
        let version: i64 = reader
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|source| OpenError::Sqlite { path: path.clone(), source })?;
        if version > SCHEMA_VERSION {
            return Err(OpenError::Newer { path, found: version });
        }
        // The writer slot is never used on a read-only store; holding the
        // reader there too keeps one type for both.
        let spare = open_reader(&path).map_err(|source| OpenError::Sqlite { path: path.clone(), source })?;
        Ok(Self { path, writer: Mutex::new(spare), readers: Mutex::new(vec![reader]) })
    }

    /// Appends `entries` to the stream `name` in `epoch`, creating the stream
    /// if this is its first batch. `published` runs after the commit, while
    /// writes are still held back - so whoever it tells hears about heads in
    /// the order they were written.
    pub fn append(
        &self,
        name: &StreamName,
        epoch: Epoch,
        entries: &[Vec<u8>],
        published: impl FnOnce(StreamId, u64),
    ) -> Result<Written, AppendError> {
        let mut conn = lock(&self.writer);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = unix_now();

        let stream = match stream_row(&tx, name)? {
            Some(stream) => stream,
            None => {
                let id = StreamId::from_bytes(random_id());
                tx.execute(
                    "INSERT INTO streams (name, id, epoch, head_hash, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    params![name.as_str(), id.as_bytes(), epoch.0, chain::genesis(&id).as_bytes(), now],
                )?;
                StreamRow { key: tx.last_insert_rowid(), id, epoch, head: 0, head_hash: chain::genesis(&id) }
            }
        };
        if epoch < stream.epoch {
            return Err(AppendError::EpochBehind { current: stream.epoch, batch: epoch });
        }

        let mut seq = stream.head;
        let mut previous = stream.head_hash;
        {
            let mut insert =
                tx.prepare_cached("INSERT INTO entries (stream, seq, epoch, hash, data) VALUES (?1, ?2, ?3, ?4, ?5)")?;
            for data in entries {
                seq += 1;
                let hash = chain::link(&previous, seq, epoch, data);
                insert.execute(params![stream.key, to_sql(seq), epoch.0, hash.as_bytes(), data])?;
                previous = hash;
            }
        }
        tx.execute("UPDATE streams SET updated_at = ?1 WHERE key = ?2", params![now, stream.key])?;
        tx.commit()?;
        published(stream.id, seq);
        drop(conn);

        Ok(Written { stream: stream.id, epoch: epoch.max(stream.epoch), first: stream.head + 1, last: seq })
    }

    /// The entries of `name` after `after`, within `limit`.
    pub fn read(&self, name: &StreamName, after: Option<&Cursor>, limit: PageLimit) -> Result<Page, ReadError> {
        let mut conn = self.reader()?;
        // One read transaction: every statement below sees the same snapshot,
        // so the head, the cursor check and the entries agree with each other
        // even while a batch is being committed next door.
        let tx = conn.connection().transaction()?;
        let stream = stream_row(&tx, name)?.ok_or_else(|| ReadError::NotFound(name.clone()))?;

        let from = match after {
            None => 0,
            Some(cursor) => {
                if cursor.stream != stream.id {
                    return Err(ReadError::Replaced { name: name.clone(), cursor: cursor.stream, current: stream.id });
                }
                if cursor.seq > stream.head {
                    return Err(ReadError::Ahead { cursor: cursor.seq, head: stream.head });
                }
                let link =
                    if cursor.seq == 0 { Some(chain::genesis(&stream.id)) } else { hash_at(&tx, &stream, cursor.seq)? };
                if link != Some(cursor.hash) {
                    return Err(ReadError::Diverged { seq: cursor.seq });
                }
                cursor.seq
            }
        };

        let mut entries = Vec::new();
        let mut bytes = 0usize;
        {
            // ORDER BY is the contract, not a hint: two readers of one stream
            // must get one sequence, whatever plan SQLite picks.
            let mut select = tx.prepare_cached(
                "SELECT seq, epoch, hash, data FROM entries WHERE stream = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
            )?;
            let mut rows = select.query(params![stream.key, to_sql(from), limit.entries as i64])?;
            while let Some(row) = rows.next()? {
                let seq = from_sql(row.get(0)?);
                let data: Vec<u8> = row.get(3)?;
                if !entries.is_empty() && bytes + data.len() > limit.bytes {
                    break;
                }
                let expected = from + entries.len() as u64 + 1;
                if seq != expected {
                    return Err(ReadError::Gap { name: name.clone(), expected, found: seq });
                }
                bytes += data.len();
                entries.push(Entry { seq, epoch: Epoch(row.get(1)?), hash: hash_column(row.get(2)?)?, data });
            }
        }

        Ok(Page { stream: stream.id, epoch: stream.epoch, head: stream.head, entries })
    }

    /// The identity and head of `name`, or the empty state if there is no
    /// such stream.
    pub fn state(&self, name: &StreamName) -> Result<StreamState, rusqlite::Error> {
        let mut conn = self.reader()?;
        Ok(stream_row(conn.connection(), name)?
            .map(|stream| StreamState { id: Some(stream.id), head: stream.head })
            .unwrap_or_default())
    }

    /// Every stream, by name.
    pub fn streams(&self) -> Result<Vec<StreamSummary>, rusqlite::Error> {
        let mut conn = self.reader()?;
        let mut select = conn
            .connection()
            .prepare("SELECT name, id, epoch, head, bytes, created_at, updated_at FROM streams ORDER BY name")?;
        let rows = select.query_map([], |row| {
            Ok(StreamSummary {
                name: row.get(0)?,
                id: id_column(row.get(1)?)?,
                epoch: Epoch(row.get(2)?),
                head: from_sql(row.get(3)?),
                bytes: from_sql(row.get(4)?),
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// The entries of `name` without their data, by position; `None` if
    /// there is no such stream.
    pub fn entries(&self, name: &StreamName) -> Result<Option<Vec<EntrySummary>>, rusqlite::Error> {
        let mut conn = self.reader()?;
        let tx = conn.connection().transaction()?;
        let Some(stream) = stream_row(&tx, name)? else {
            return Ok(None);
        };
        let mut select =
            tx.prepare("SELECT seq, epoch, length(data), hash FROM entries WHERE stream = ?1 ORDER BY seq")?;
        let rows = select.query_map([stream.key], |row| {
            Ok(EntrySummary {
                seq: from_sql(row.get(0)?),
                epoch: Epoch(row.get(1)?),
                size: from_sql(row.get(2)?),
                hash: hash_column(row.get(3)?)?,
            })
        })?;
        rows.collect::<Result<_, _>>().map(Some)
    }

    fn reader(&self) -> Result<Reader<'_>, rusqlite::Error> {
        let pooled = lock(&self.readers).pop();
        let conn = match pooled {
            Some(conn) => conn,
            None => open_reader(&self.path)?,
        };
        Ok(Reader { store: self, conn: Some(conn) })
    }
}

/// A read connection on loan from the pool; it goes back when dropped.
struct Reader<'a> {
    store: &'a Store,
    conn: Option<Connection>,
}

impl Reader<'_> {
    fn connection(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("a reader holds its connection until dropped")
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            lock(&self.store.readers).push(conn);
        }
    }
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
}

fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

fn migrate(conn: &mut Connection, path: &Path) -> Result<(), OpenError> {
    let sqlite = |source| OpenError::Sqlite { path: path.to_path_buf(), source };
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(sqlite)?;
    if version > SCHEMA_VERSION {
        return Err(OpenError::Newer { path: path.to_path_buf(), found: version });
    }
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction().map_err(sqlite)?;
        tx.execute_batch(migration).map_err(sqlite)?;
        tx.pragma_update(None, "user_version", index as i64 + 1).map_err(sqlite)?;
        tx.commit().map_err(sqlite)?;
    }
    Ok(())
}

fn stream_row(conn: &Connection, name: &StreamName) -> rusqlite::Result<Option<StreamRow>> {
    conn.prepare_cached("SELECT key, id, epoch, head, head_hash FROM streams WHERE name = ?1")?
        .query_row([name.as_str()], |row| {
            Ok(StreamRow {
                key: row.get(0)?,
                id: id_column(row.get(1)?)?,
                epoch: Epoch(row.get(2)?),
                head: from_sql(row.get(3)?),
                head_hash: hash_column(row.get(4)?)?,
            })
        })
        .optional()
}

fn hash_at(conn: &Connection, stream: &StreamRow, seq: u64) -> rusqlite::Result<Option<Hash>> {
    conn.prepare_cached("SELECT hash FROM entries WHERE stream = ?1 AND seq = ?2")?
        .query_row(params![stream.key, to_sql(seq)], |row| hash_column(row.get(0)?))
        .optional()
}

fn id_column(bytes: Vec<u8>) -> rusqlite::Result<StreamId> {
    <[u8; 16]>::try_from(bytes.as_slice())
        .map(StreamId::from_bytes)
        .map_err(|_| rusqlite::Error::InvalidColumnType(0, "id".into(), rusqlite::types::Type::Blob))
}

fn hash_column(bytes: Vec<u8>) -> rusqlite::Result<Hash> {
    <[u8; 32]>::try_from(bytes.as_slice())
        .map(Hash::from_bytes)
        .map_err(|_| rusqlite::Error::InvalidColumnType(0, "hash".into(), rusqlite::types::Type::Blob))
}

// Positions are u64 on the wire and INTEGER (i64) in SQLite. A stream would
// need 2^63 entries to tell the difference; the conversion is spelled out so
// that nobody wonders.
fn to_sql(value: u64) -> i64 {
    i64::try_from(value).expect("positions stay below 2^63")
}

fn from_sql(value: i64) -> u64 {
    u64::try_from(value).expect("the schema keeps positions and sizes non-negative")
}

fn random_id() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the operating system has a source of randomness");
    bytes
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A poisoned lock means a thread panicked while holding it; the connection
/// inside is still a valid SQLite handle, and SQLite rolled back whatever
/// transaction was open. Going on is safe; refusing every later request is
/// not better.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    fn name(text: &str) -> StreamName {
        text.parse().unwrap()
    }

    const ALL: PageLimit = PageLimit { entries: 1000, bytes: usize::MAX };

    #[test]
    fn the_schema_refuses_a_gap() {
        let (_dir, store) = store();
        store.append(&name("s"), Epoch(1), &[b"a".to_vec()], |_, _| {}).unwrap();
        let conn = lock(&store.writer);
        let error = conn
            .execute("INSERT INTO entries (stream, seq, epoch, hash, data) VALUES (1, 3, 1, zeroblob(32), x'00')", [])
            .unwrap_err();
        assert!(error.to_string().contains("right after the head"), "{error}");
    }

    #[test]
    fn the_schema_refuses_an_older_epoch() {
        let (_dir, store) = store();
        store.append(&name("s"), Epoch(2), &[b"a".to_vec()], |_, _| {}).unwrap();
        let conn = lock(&store.writer);
        let error = conn
            .execute("INSERT INTO entries (stream, seq, epoch, hash, data) VALUES (1, 2, 1, zeroblob(32), x'00')", [])
            .unwrap_err();
        assert!(error.to_string().contains("older than its stream"), "{error}");
    }

    #[test]
    fn the_schema_refuses_to_change_or_delete_an_entry() {
        let (_dir, store) = store();
        store.append(&name("s"), Epoch(1), &[b"a".to_vec()], |_, _| {}).unwrap();
        let conn = lock(&store.writer);
        let error = conn.execute("UPDATE entries SET data = x'01'", []).unwrap_err();
        assert!(error.to_string().contains("cannot be changed"), "{error}");
        let error = conn.execute("DELETE FROM entries", []).unwrap_err();
        assert!(error.to_string().contains("cannot be deleted"), "{error}");
    }

    #[test]
    fn the_head_moves_with_the_entries() {
        let (_dir, store) = store();
        let written = store.append(&name("s"), Epoch(1), &[b"ab".to_vec(), b"cde".to_vec()], |_, _| {}).unwrap();
        assert_eq!((written.first, written.last), (1, 2));
        let summary = &store.streams().unwrap()[0];
        assert_eq!((summary.head, summary.bytes), (2, 5));
        let page = store.read(&name("s"), None, ALL).unwrap();
        assert_eq!(page.head, 2);
        assert_eq!(page.cursor().unwrap().hash, page.entries[1].hash);
    }

    #[test]
    fn a_page_keeps_to_its_byte_budget_but_never_comes_back_empty() {
        let (_dir, store) = store();
        let big = vec![0u8; 100];
        store.append(&name("s"), Epoch(1), &[big.clone(), big.clone(), big], |_, _| {}).unwrap();
        let page = store.read(&name("s"), None, PageLimit { entries: 10, bytes: 250 }).unwrap();
        assert_eq!(page.entries.len(), 2);
        let page = store.read(&name("s"), None, PageLimit { entries: 10, bytes: 10 }).unwrap();
        assert_eq!(page.entries.len(), 1);
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_misread() {
        let dir = tempfile::tempdir().unwrap();
        drop(Store::open(dir.path()).unwrap());
        let conn = Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        drop(conn);
        assert!(matches!(Store::open(dir.path()), Err(OpenError::Newer { .. })));
        assert!(matches!(Store::open_read_only(dir.path()), Err(OpenError::Newer { .. })));
    }

    #[test]
    fn reading_a_relay_that_is_not_there_says_so() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(Store::open_read_only(dir.path()), Err(OpenError::Missing(_))));
        assert!(!dir.path().join(DATABASE_FILE).exists(), "looking must not create a database");
    }
}
