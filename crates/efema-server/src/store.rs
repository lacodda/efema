//! The relay's storage: one SQLite database in the data directory.
//!
//! Five tables. `spaces` holds the spaces the operator made, and
//! `access_keys` the keys that open them - by digest, never the key itself.
//! `streams` holds one row per stream - its space, name, identity, epoch and
//! head; `entries` holds the entries, keyed by stream and position; `batches`
//! holds the key of every batch written and where it landed, so a batch sent
//! again is answered rather than written twice. The schema itself enforces
//! what the relay promises, so a bug in the code above it fails a write
//! instead of corrupting a stream:
//!
//! - an entry is inserted only at the position right after the head, so a
//!   stream has no gaps and no position twice;
//! - an entry's epoch is never below the stream's;
//! - an entry, once written, cannot be changed or deleted;
//! - a batch is recorded once per stream, ending at the head it moved, after
//!   every batch before it - and, once recorded, stays as it is;
//! - a stream belongs to one space, and its name is its own within it.
//!
//! A stream's identity is not unique on a relay: a stream copied from
//! elsewhere keeps the one it had, and may be copied twice.
//!
//! Writes go through one connection, one transaction per batch; reads use
//! their own connections and see a consistent snapshot (WAL), so a reader
//! never waits for a writer and never sees half a batch.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use efema_proto::wire::{Batch, Entry, Page, Written};
use efema_proto::{AccessKey, Cursor, Epoch, Hash, StreamId, StreamName, access_id, chain};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

use crate::space::{Access, SpaceName};

/// The database file inside the data directory.
pub const DATABASE_FILE: &str = "relay.sqlite3";

/// The space that the streams of a relay older than spaces move into.
pub const DEFAULT_SPACE: &str = "default";

/// The schema version this build writes and reads (`PRAGMA user_version`).
const SCHEMA_VERSION: i64 = 4;

/// Migrations, in order: index `i` takes the database from version `i` to
/// `i + 1`. An applied migration is never edited - a change is a new one.
/// They run with foreign keys off, and each is checked against them before
/// it commits: a migration that rebuilds a table others refer to is
/// half-done in between.
const MIGRATIONS: &[&str] = &[
    r#"
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
"#,
    r#"
-- Every batch written since this migration, by its key. A batch sent again -
-- its answer lost on the way back - is answered with where it landed. The
-- primary key keeps a stream from holding one batch twice even if the code
-- above forgot to look.
CREATE TABLE batches (
    stream INTEGER NOT NULL REFERENCES streams (key),
    key    BLOB    NOT NULL CHECK (length(key) = 32),
    first  INTEGER NOT NULL CHECK (first >= 1),
    last   INTEGER NOT NULL CHECK (last >= first),
    PRIMARY KEY (stream, key)
) STRICT;

-- A batch is recorded in the transaction that wrote it, once its entries are
-- in: it ends at the head, and after every batch before it.
CREATE TRIGGER batches_end_at_the_head
BEFORE INSERT ON batches
BEGIN
    SELECT RAISE(ABORT, 'a batch must end at the head of its stream')
    WHERE NEW.last <> (SELECT head FROM streams WHERE key = NEW.stream);
    SELECT RAISE(ABORT, 'a batch cannot overlap the batch before it')
    WHERE NEW.first <= (SELECT coalesce(max(last), 0) FROM batches WHERE stream = NEW.stream);
END;

CREATE TRIGGER batches_are_immutable
BEFORE UPDATE ON batches
BEGIN
    SELECT RAISE(ABORT, 'a batch cannot be changed');
END;

CREATE TRIGGER batches_are_permanent
BEFORE DELETE ON batches
BEGIN
    SELECT RAISE(ABORT, 'a batch cannot be deleted');
END;
"#,
    r#"
-- Spaces: what an access key opens. A stream lives in one space, under a
-- name that is its own there - two spaces may each have a `notes`.
CREATE TABLE spaces (
    key        INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
) STRICT;

-- The keys the operator handed out, by digest: the database gives no key
-- away. A key reads its space, or reads and writes it. A revoked key is gone.
CREATE TABLE access_keys (
    hash       BLOB    PRIMARY KEY CHECK (length(hash) = 32),
    space      INTEGER NOT NULL REFERENCES spaces (key),
    access     TEXT    NOT NULL CHECK (access IN ('read', 'write')),
    label      TEXT    NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    used_at    INTEGER
) STRICT;

CREATE INDEX access_keys_by_space ON access_keys (space);

-- The streams of a relay older than spaces move into one named `default`;
-- the operator hands out keys to it to reach them.
INSERT INTO spaces (name, created_at)
SELECT 'default', unixepoch() WHERE EXISTS (SELECT 1 FROM streams);

-- A stream's name is unique within its space now, not on the relay: the
-- table is rebuilt, keys kept, so entries and batches still point at their
-- streams. The triggers that read `streams` go and come back unchanged
-- around it.
DROP TRIGGER entries_follow_the_head;
DROP TRIGGER entries_move_the_head;
DROP TRIGGER batches_end_at_the_head;

CREATE TABLE streams_in_spaces (
    key        INTEGER PRIMARY KEY,
    space      INTEGER NOT NULL REFERENCES spaces (key),
    name       TEXT    NOT NULL,
    id         BLOB    NOT NULL UNIQUE CHECK (length(id) = 16),
    epoch      INTEGER NOT NULL CHECK (epoch BETWEEN 0 AND 4294967295),
    head       INTEGER NOT NULL DEFAULT 0 CHECK (head >= 0),
    head_hash  BLOB    NOT NULL CHECK (length(head_hash) = 32),
    bytes      INTEGER NOT NULL DEFAULT 0 CHECK (bytes >= 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (space, name)
) STRICT;

INSERT INTO streams_in_spaces (key, space, name, id, epoch, head, head_hash, bytes, created_at, updated_at)
SELECT key, (SELECT key FROM spaces WHERE name = 'default'), name, id, epoch, head, head_hash, bytes,
       created_at, updated_at
FROM streams;

DROP TABLE streams;
ALTER TABLE streams_in_spaces RENAME TO streams;

CREATE TRIGGER entries_follow_the_head
BEFORE INSERT ON entries
BEGIN
    SELECT RAISE(ABORT, 'an entry must take the position right after the head')
    WHERE NEW.seq <> (SELECT head FROM streams WHERE key = NEW.stream) + 1;
    SELECT RAISE(ABORT, 'an entry cannot be written in an epoch older than its stream')
    WHERE NEW.epoch < (SELECT epoch FROM streams WHERE key = NEW.stream);
END;

CREATE TRIGGER entries_move_the_head
AFTER INSERT ON entries
BEGIN
    UPDATE streams
    SET head = NEW.seq, head_hash = NEW.hash, epoch = NEW.epoch, bytes = bytes + length(NEW.data)
    WHERE key = NEW.stream;
END;

CREATE TRIGGER batches_end_at_the_head
BEFORE INSERT ON batches
BEGIN
    SELECT RAISE(ABORT, 'a batch must end at the head of its stream')
    WHERE NEW.last <> (SELECT head FROM streams WHERE key = NEW.stream);
    SELECT RAISE(ABORT, 'a batch cannot overlap the batch before it')
    WHERE NEW.first <= (SELECT coalesce(max(last), 0) FROM batches WHERE stream = NEW.stream);
END;

-- A stream stays in the space it was created in.
CREATE TRIGGER streams_stay_in_their_space
BEFORE UPDATE OF space, name ON streams
BEGIN
    SELECT RAISE(ABORT, 'a stream cannot move to another space or name');
END;
"#,
    r#"
-- A stream copied from elsewhere keeps its identity, so two streams of one
-- relay may share one: a copy in another space, or under another name. The
-- name stays unique within its space. The table is rebuilt without the
-- uniqueness of the identity, keys kept; the triggers that read it go and
-- come back unchanged around it.
DROP TRIGGER entries_follow_the_head;
DROP TRIGGER entries_move_the_head;
DROP TRIGGER batches_end_at_the_head;
DROP TRIGGER streams_stay_in_their_space;

CREATE TABLE streams_by_name (
    key        INTEGER PRIMARY KEY,
    space      INTEGER NOT NULL REFERENCES spaces (key),
    name       TEXT    NOT NULL,
    id         BLOB    NOT NULL CHECK (length(id) = 16),
    epoch      INTEGER NOT NULL CHECK (epoch BETWEEN 0 AND 4294967295),
    head       INTEGER NOT NULL DEFAULT 0 CHECK (head >= 0),
    head_hash  BLOB    NOT NULL CHECK (length(head_hash) = 32),
    bytes      INTEGER NOT NULL DEFAULT 0 CHECK (bytes >= 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (space, name)
) STRICT;

INSERT INTO streams_by_name (key, space, name, id, epoch, head, head_hash, bytes, created_at, updated_at)
SELECT key, space, name, id, epoch, head, head_hash, bytes, created_at, updated_at
FROM streams;

DROP TABLE streams;
ALTER TABLE streams_by_name RENAME TO streams;

CREATE TRIGGER entries_follow_the_head
BEFORE INSERT ON entries
BEGIN
    SELECT RAISE(ABORT, 'an entry must take the position right after the head')
    WHERE NEW.seq <> (SELECT head FROM streams WHERE key = NEW.stream) + 1;
    SELECT RAISE(ABORT, 'an entry cannot be written in an epoch older than its stream')
    WHERE NEW.epoch < (SELECT epoch FROM streams WHERE key = NEW.stream);
END;

CREATE TRIGGER entries_move_the_head
AFTER INSERT ON entries
BEGIN
    UPDATE streams
    SET head = NEW.seq, head_hash = NEW.hash, epoch = NEW.epoch, bytes = bytes + length(NEW.data)
    WHERE key = NEW.stream;
END;

CREATE TRIGGER batches_end_at_the_head
BEFORE INSERT ON batches
BEGIN
    SELECT RAISE(ABORT, 'a batch must end at the head of its stream')
    WHERE NEW.last <> (SELECT head FROM streams WHERE key = NEW.stream);
    SELECT RAISE(ABORT, 'a batch cannot overlap the batch before it')
    WHERE NEW.first <= (SELECT coalesce(max(last), 0) FROM batches WHERE stream = NEW.stream);
END;

CREATE TRIGGER streams_stay_in_their_space
BEFORE UPDATE OF space, name ON streams
BEGIN
    SELECT RAISE(ABORT, 'a stream cannot move to another space or name');
END;

-- The batches of a stream by where they end: a page says which of its
-- entries end one, and the trigger above finds the last.
CREATE INDEX batches_by_end ON batches (stream, last);
"#,
];

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
    #[error(
        "the database {path} has schema version {found}, older than this efema-server's {SCHEMA_VERSION}: start `efema-server serve` once to bring it up to date"
    )]
    Older { path: PathBuf, found: i64 },
    #[error("bringing the database {path} to schema version {version} left rows that refer to nothing")]
    Dangling { path: PathBuf, version: i64 },
}

/// Why a write was refused.
#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    #[error("the stream is at epoch {current}, and the batch was written in epoch {batch}")]
    EpochBehind { current: Epoch, batch: Epoch },
    #[error("there is no stream named {name}, and the batch expects stream {expected}")]
    NotFound { name: StreamName, expected: StreamId },
    #[error("the batch expects stream {expected}, and {name} is now stream {current}")]
    Replaced { name: StreamName, expected: StreamId, current: StreamId },
    #[error("the batch is {size} bytes, and this relay takes at most {limit} in one request")]
    BodyTooLarge { size: u64, limit: u64 },
    #[error("an entry of the batch is {size} bytes, and this relay takes entries of at most {limit}")]
    EntryTooLarge { size: u64, limit: u64 },
    #[error("the space holds {used} bytes and may hold {quota}: the batch's {size} more do not fit")]
    QuotaExceeded { used: u64, size: u64, quota: u64 },
    #[error("the batch is to start at position {first}, and the stream's next place is {}", .head + 1)]
    OutOfPlace { first: u64, head: u64 },
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

/// Why a key was not revoked.
#[derive(Debug, thiserror::Error)]
pub enum RevokeError {
    #[error("`{0}` is not a key's id: a key is named by the 8 hexadecimal digits `efema-server keys` lists")]
    BadId(String),
    #[error("no key has the id {0}")]
    NotFound(String),
    #[error("{count} keys have the id {id}; revoke them by making a new space instead")]
    Ambiguous { id: String, count: usize },
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

/// What one write may carry, and what its space may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteLimits {
    /// Bytes of the request body - the batch as it came.
    pub body: u64,
    /// Bytes of one entry.
    pub entry: u64,
    /// Bytes of entry data the space may hold, all its streams together.
    pub quota: u64,
}

/// What an access key opens, as the relay found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The key's digest.
    pub digest: [u8; 32],
    /// The space it opens: the row's key, for the queries.
    pub space: i64,
    /// The space's name, for the log.
    pub space_name: SpaceName,
    /// What it may do there.
    pub access: Access,
}

/// A stream as the relay's listing shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSummary {
    pub space: String,
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

/// An access key as the relay's listing shows it: never the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySummary {
    /// The key's id: the first eight hexadecimal digits of its digest.
    pub id: String,
    pub space: String,
    pub access: Access,
    pub label: String,
    pub created_at: i64,
    /// When the key was last shown to the relay, to the minute; `None` if
    /// never.
    pub used_at: Option<i64>,
}

/// One space as the relay's metrics show it: counts and times, no contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceSummary {
    pub name: String,
    pub streams: u64,
    pub entries: u64,
    pub bytes: u64,
    pub keys: u64,
    /// The last write to any of its streams; `None` if none was written.
    pub written_at: Option<i64>,
    /// The last time any of its keys was used; `None` if none was.
    pub used_at: Option<i64>,
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
    /// date. Only the serving process, and the commands that hand out and
    /// revoke keys, do this.
    pub fn open(dir: &Path) -> Result<Self, OpenError> {
        let path = dir.join(DATABASE_FILE);
        let sqlite = |source| OpenError::Sqlite { path: path.clone(), source };
        let mut writer = Connection::open(&path).map_err(sqlite)?;
        writer.pragma_update(None, "journal_mode", "WAL").map_err(sqlite)?;
        writer.busy_timeout(std::time::Duration::from_secs(5)).map_err(sqlite)?;
        // A write acknowledged to a client is one the client stops resending:
        // it has to survive a power cut, not only a crash. FULL syncs the WAL
        // on every commit.
        writer.pragma_update(None, "synchronous", "FULL").map_err(sqlite)?;
        // Off while migrating - a rebuilt table is missing in between - and
        // checked by each migration before it commits; on for everything
        // after.
        writer.pragma_update(None, "foreign_keys", "OFF").map_err(sqlite)?;
        migrate(&mut writer, &path)?;
        writer.pragma_update(None, "foreign_keys", "ON").map_err(sqlite)?;
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
        // Looking never migrates: an older schema is read by an older
        // release, or brought up to date by serving it.
        if version < SCHEMA_VERSION {
            return Err(OpenError::Older { path, found: version });
        }
        // The writer slot is never used on a read-only store; holding the
        // reader there too keeps one type for both.
        let spare = open_reader(&path).map_err(|source| OpenError::Sqlite { path: path.clone(), source })?;
        Ok(Self { path, writer: Mutex::new(spare), readers: Mutex::new(vec![reader]) })
    }

    /// Appends a batch to the stream `name` of `space`. When the batch names
    /// a stream, only to that incarnation; when it does not, to whatever
    /// stream has the name, creating one if none does. When it says which
    /// position it starts at, only there - and a batch that starts at 1 and
    /// names a stream the space does not have creates it with that identity:
    /// a copy.
    ///
    /// A batch the stream has already is not written again: the answer is
    /// where it landed, with `repeated` - before the limits and the epoch are
    /// looked at, so a batch is answered the same way whatever changed
    /// since. A refusal for size or quota therefore means the stream does not
    /// have the batch, and its writer may cut it again.
    ///
    /// `body` is the size of the request the batch came in. `published` runs
    /// after the commit of a write, while writes are still held back - so
    /// whoever it tells hears about heads in the order they were written.
    pub fn append(
        &self,
        space: i64,
        name: &StreamName,
        batch: &Batch,
        body: u64,
        limits: WriteLimits,
        published: impl FnOnce(StreamId, u64),
    ) -> Result<Written, AppendError> {
        let key = batch.key();
        let epoch = batch.epoch;
        let mut conn = lock(&self.writer);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = unix_now();

        let stream = match (stream_row(&tx, space, name)?, batch.stream) {
            (Some(stream), Some(expected)) if stream.id != expected => {
                return Err(AppendError::Replaced { name: name.clone(), expected, current: stream.id });
            }
            (Some(stream), _) => stream,
            // A copy's first batch: the stream it copies, under its identity.
            (None, Some(expected)) if batch.first == Some(1) => create(&tx, space, name, expected, epoch, now)?,
            (None, Some(expected)) => return Err(AppendError::NotFound { name: name.clone(), expected }),
            (None, None) => create(&tx, space, name, StreamId::from_bytes(random_bytes()), epoch, now)?,
        };
        // Before everything else: a batch written before the stream moved on,
        // or before the operator lowered a limit, is still written, and its
        // writer is owed the answer it lost.
        if let Some((first, last)) = landed(&tx, &stream, &key)? {
            return Ok(Written { stream: stream.id, epoch: stream.epoch, first, last, repeated: true });
        }
        if let Some(first) = batch.first
            && first != stream.head + 1
        {
            return Err(AppendError::OutOfPlace { first, head: stream.head });
        }
        if body > limits.body {
            return Err(AppendError::BodyTooLarge { size: body, limit: limits.body });
        }
        if let Some(entry) = batch.entries.iter().find(|entry| entry.len() as u64 > limits.entry) {
            return Err(AppendError::EntryTooLarge { size: entry.len() as u64, limit: limits.entry });
        }
        if epoch < stream.epoch {
            return Err(AppendError::EpochBehind { current: stream.epoch, batch: epoch });
        }
        let size: u64 = batch.entries.iter().map(|entry| entry.len() as u64).sum();
        let used = used(&tx, space)?;
        if used.saturating_add(size) > limits.quota {
            return Err(AppendError::QuotaExceeded { used, size, quota: limits.quota });
        }

        let mut seq = stream.head;
        let mut previous = stream.head_hash;
        {
            let mut insert =
                tx.prepare_cached("INSERT INTO entries (stream, seq, epoch, hash, data) VALUES (?1, ?2, ?3, ?4, ?5)")?;
            for data in &batch.entries {
                seq += 1;
                let hash = chain::link(&previous, seq, epoch, data);
                insert.execute(params![stream.key, to_sql(seq), epoch.0, hash.as_bytes(), data])?;
                previous = hash;
            }
        }
        tx.execute(
            "INSERT INTO batches (stream, key, first, last) VALUES (?1, ?2, ?3, ?4)",
            params![stream.key, key.as_bytes(), to_sql(stream.head + 1), to_sql(seq)],
        )?;
        tx.execute("UPDATE streams SET updated_at = ?1 WHERE key = ?2", params![now, stream.key])?;
        tx.commit()?;
        published(stream.id, seq);
        drop(conn);

        Ok(Written {
            stream: stream.id,
            epoch: epoch.max(stream.epoch),
            first: stream.head + 1,
            last: seq,
            repeated: false,
        })
    }

    /// The entries of `name` in `space` after `after`, within `limit`.
    pub fn read(
        &self,
        space: i64,
        name: &StreamName,
        after: Option<&Cursor>,
        limit: PageLimit,
    ) -> Result<Page, ReadError> {
        let mut conn = self.reader()?;
        // One read transaction: every statement below sees the same snapshot,
        // so the head, the cursor check and the entries agree with each other
        // even while a batch is being committed next door.
        let tx = conn.connection().transaction()?;
        let stream = stream_row(&tx, space, name)?.ok_or_else(|| ReadError::NotFound(name.clone()))?;

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
                entries.push(Entry {
                    seq,
                    epoch: Epoch(row.get(1)?),
                    hash: hash_column(row.get(2)?)?,
                    data,
                    ends_batch: false,
                });
            }
        }
        ends_of_batches(&tx, &stream, &mut entries)?;

        Ok(Page { stream: stream.id, epoch: stream.epoch, head: stream.head, entries })
    }

    /// The identity and head of `name` in `space`, or the empty state if
    /// there is no such stream.
    pub fn state(&self, space: i64, name: &StreamName) -> Result<StreamState, rusqlite::Error> {
        let mut conn = self.reader()?;
        Ok(stream_row(conn.connection(), space, name)?
            .map(|stream| StreamState { id: Some(stream.id), head: stream.head })
            .unwrap_or_default())
    }

    /// Bytes of entry data `space` holds, all its streams together.
    pub fn used(&self, space: i64) -> Result<u64, rusqlite::Error> {
        let mut conn = self.reader()?;
        used(conn.connection(), space)
    }

    /// Every stream, by space and name.
    pub fn streams(&self) -> Result<Vec<StreamSummary>, rusqlite::Error> {
        let mut conn = self.reader()?;
        let mut select = conn.connection().prepare(
            "SELECT sp.name, s.name, s.id, s.epoch, s.head, s.bytes, s.created_at, s.updated_at
             FROM streams s JOIN spaces sp ON sp.key = s.space
             ORDER BY sp.name, s.name",
        )?;
        let rows = select.query_map([], |row| {
            Ok(StreamSummary {
                space: row.get(0)?,
                name: row.get(1)?,
                id: id_column(row.get(2)?)?,
                epoch: Epoch(row.get(3)?),
                head: from_sql(row.get(4)?),
                bytes: from_sql(row.get(5)?),
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;
        rows.collect()
    }

    /// The entries of `name` in the space named `space` without their data,
    /// by position; `None` if there is no such stream.
    pub fn entries(&self, space: &SpaceName, name: &StreamName) -> Result<Option<Vec<EntrySummary>>, rusqlite::Error> {
        let mut conn = self.reader()?;
        let tx = conn.connection().transaction()?;
        let Some(space) = space_key(&tx, space)? else {
            return Ok(None);
        };
        let Some(stream) = stream_row(&tx, space, name)? else {
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

    /// Hands out a new access key to `space` - creating the space the first
    /// time - and returns it. The relay keeps only its digest: this is the
    /// one moment the key exists in the clear.
    pub fn add_key(&self, space: &SpaceName, access: Access, label: &str) -> Result<AccessKey, rusqlite::Error> {
        let key = AccessKey::from_bytes(random_bytes());
        let mut conn = lock(&self.writer);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = unix_now();
        tx.execute("INSERT OR IGNORE INTO spaces (name, created_at) VALUES (?1, ?2)", params![space.as_str(), now])?;
        let space = space_key(&tx, space)?.expect("the space was just made, or was there");
        tx.execute(
            "INSERT INTO access_keys (hash, space, access, label, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key.digest(), space, access.as_str(), label, now],
        )?;
        tx.commit()?;
        Ok(key)
    }

    /// Every access key, by space and age.
    pub fn keys(&self) -> Result<Vec<KeySummary>, rusqlite::Error> {
        let mut conn = self.reader()?;
        let mut select = conn.connection().prepare(
            "SELECT k.hash, sp.name, k.access, k.label, k.created_at, k.used_at
             FROM access_keys k JOIN spaces sp ON sp.key = k.space
             ORDER BY sp.name, k.created_at, k.hash",
        )?;
        let rows = select.query_map([], |row| {
            Ok(KeySummary {
                id: access_id(&digest_column(row.get(0)?)?),
                space: row.get(1)?,
                access: access_column(row.get(2)?)?,
                label: row.get(3)?,
                created_at: row.get(4)?,
                used_at: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    /// Revokes the key whose id is `id`: it opens nothing from the next
    /// request on, on a relay that is running too. Its space and streams
    /// stay. What was revoked.
    pub fn revoke(&self, id: &str) -> Result<KeySummary, RevokeError> {
        let prefix = parse_id(id).ok_or_else(|| RevokeError::BadId(id.to_string()))?;
        let found: Vec<KeySummary> = self.keys()?.into_iter().filter(|key| key.id == id).collect();
        match found.len() {
            0 => return Err(RevokeError::NotFound(id.to_string())),
            1 => {}
            count => return Err(RevokeError::Ambiguous { id: id.to_string(), count }),
        }
        let conn = lock(&self.writer);
        conn.execute("DELETE FROM access_keys WHERE substr(hash, 1, 4) = ?1", [prefix.as_slice()])?;
        Ok(found.into_iter().next().expect("one key was found"))
    }

    /// What the key with `digest` opens; `None` if the relay never handed it
    /// out, or revoked it.
    pub fn grant(&self, digest: &[u8; 32]) -> Result<Option<Grant>, rusqlite::Error> {
        let mut conn = self.reader()?;
        conn.connection()
            .prepare_cached(
                "SELECT k.space, sp.name, k.access FROM access_keys k JOIN spaces sp ON sp.key = k.space
                 WHERE k.hash = ?1",
            )?
            .query_row([digest.as_slice()], |row| {
                let name: String = row.get(1)?;
                Ok(Grant {
                    digest: *digest,
                    space: row.get(0)?,
                    space_name: SpaceName::new(&name).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
                    })?,
                    access: access_column(row.get(2)?)?,
                })
            })
            .optional()
    }

    /// Records that the key with `digest` was used at `at` (seconds since
    /// the Unix epoch).
    pub fn touch(&self, digest: &[u8; 32], at: i64) -> Result<(), rusqlite::Error> {
        let conn = lock(&self.writer);
        conn.prepare_cached("UPDATE access_keys SET used_at = ?2 WHERE hash = ?1")?
            .execute(params![digest.as_slice(), at])
            .map(drop)
    }

    /// Every space with its counts: what the metrics report.
    pub fn spaces(&self) -> Result<Vec<SpaceSummary>, rusqlite::Error> {
        let mut conn = self.reader()?;
        let mut select = conn.connection().prepare(
            "SELECT sp.name,
                    (SELECT count(*) FROM streams s WHERE s.space = sp.key),
                    (SELECT coalesce(sum(s.head), 0) FROM streams s WHERE s.space = sp.key),
                    (SELECT coalesce(sum(s.bytes), 0) FROM streams s WHERE s.space = sp.key),
                    (SELECT count(*) FROM access_keys k WHERE k.space = sp.key),
                    (SELECT max(s.updated_at) FROM streams s WHERE s.space = sp.key),
                    (SELECT max(k.used_at) FROM access_keys k WHERE k.space = sp.key)
             FROM spaces sp ORDER BY sp.name",
        )?;
        let rows = select.query_map([], |row| {
            Ok(SpaceSummary {
                name: row.get(0)?,
                streams: from_sql(row.get(1)?),
                entries: from_sql(row.get(2)?),
                bytes: from_sql(row.get(3)?),
                keys: from_sql(row.get(4)?),
                written_at: row.get(5)?,
                used_at: row.get(6)?,
            })
        })?;
        rows.collect()
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
        let to = index as i64 + 1;
        let tx = conn.transaction().map_err(sqlite)?;
        tx.execute_batch(migration).map_err(sqlite)?;
        let dangling = tx.prepare("PRAGMA foreign_key_check").and_then(|mut check| check.exists([])).map_err(sqlite)?;
        if dangling {
            return Err(OpenError::Dangling { path: path.to_path_buf(), version: to });
        }
        tx.pragma_update(None, "user_version", to).map_err(sqlite)?;
        tx.commit().map_err(sqlite)?;
    }
    Ok(())
}

fn stream_row(conn: &Connection, space: i64, name: &StreamName) -> rusqlite::Result<Option<StreamRow>> {
    conn.prepare_cached("SELECT key, id, epoch, head, head_hash FROM streams WHERE space = ?1 AND name = ?2")?
        .query_row(params![space, name.as_str()], |row| {
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

/// Makes `name` in `space` a stream with identity `id`, starting at
/// `epoch`, with nothing in it yet.
fn create(
    conn: &Connection,
    space: i64,
    name: &StreamName,
    id: StreamId,
    epoch: Epoch,
    now: i64,
) -> rusqlite::Result<StreamRow> {
    conn.execute(
        "INSERT INTO streams (space, name, id, epoch, head_hash, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![space, name.as_str(), id.as_bytes(), epoch.0, chain::genesis(&id).as_bytes(), now],
    )?;
    Ok(StreamRow { key: conn.last_insert_rowid(), id, epoch, head: 0, head_hash: chain::genesis(&id) })
}

/// Marks the entries of a page that end the batch they were written in. An
/// entry in no recorded batch - kept from before the relay recorded them -
/// is a batch of its own.
fn ends_of_batches(conn: &Connection, stream: &StreamRow, entries: &mut [Entry]) -> rusqlite::Result<()> {
    let Some(low) = entries.first().map(|entry| entry.seq) else {
        return Ok(());
    };
    // Batches do not overlap, so no more of them reach into the page than
    // it has entries, and they come first in the order of their ends.
    let mut select =
        conn.prepare_cached("SELECT first, last FROM batches WHERE stream = ?1 AND last >= ?2 ORDER BY last LIMIT ?3")?;
    let batches = select
        .query_map(params![stream.key, to_sql(low), entries.len() as i64], |row| {
            Ok((from_sql(row.get(0)?), from_sql(row.get(1)?)))
        })?
        .collect::<rusqlite::Result<Vec<(u64, u64)>>>()?;
    let mut batches = batches.into_iter().peekable();
    for entry in entries {
        while batches.next_if(|&(_, last)| last < entry.seq).is_some() {}
        entry.ends_batch = match batches.peek() {
            Some(&(first, last)) if first <= entry.seq => last == entry.seq,
            _ => true,
        };
    }
    Ok(())
}

fn space_key(conn: &Connection, space: &SpaceName) -> rusqlite::Result<Option<i64>> {
    conn.prepare_cached("SELECT key FROM spaces WHERE name = ?1")?
        .query_row([space.as_str()], |row| row.get(0))
        .optional()
}

fn used(conn: &Connection, space: i64) -> rusqlite::Result<u64> {
    conn.prepare_cached("SELECT coalesce(sum(bytes), 0) FROM streams WHERE space = ?1")?
        .query_row([space], |row| Ok(from_sql(row.get(0)?)))
}

/// Where the batch with `key` landed in `stream`, if it is there.
fn landed(conn: &Connection, stream: &StreamRow, key: &Hash) -> rusqlite::Result<Option<(u64, u64)>> {
    conn.prepare_cached("SELECT first, last FROM batches WHERE stream = ?1 AND key = ?2")?
        .query_row(params![stream.key, key.as_bytes()], |row| Ok((from_sql(row.get(0)?), from_sql(row.get(1)?))))
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
    digest_column(bytes).map(Hash::from_bytes)
}

fn digest_column(bytes: Vec<u8>) -> rusqlite::Result<[u8; 32]> {
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| rusqlite::Error::InvalidColumnType(0, "hash".into(), rusqlite::types::Type::Blob))
}

fn access_column(text: String) -> rusqlite::Result<Access> {
    text.parse().map_err(|_| rusqlite::Error::InvalidColumnType(0, "access".into(), rusqlite::types::Type::Text))
}

/// The four bytes a key's id names, or `None` if `id` is not eight lowercase
/// hexadecimal digits.
fn parse_id(id: &str) -> Option<Vec<u8>> {
    if id.len() != 8 || !id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    (0..4).map(|i| u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).ok()).collect()
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

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the operating system has a source of randomness");
    bytes
}

/// Seconds since the Unix epoch.
pub(crate) fn unix_now() -> i64 {
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

    fn space(text: &str) -> SpaceName {
        text.parse().unwrap()
    }

    const ALL: PageLimit = PageLimit { entries: 1000, bytes: usize::MAX };
    const ANY: WriteLimits = WriteLimits { body: u64::MAX, entry: u64::MAX, quota: u64::MAX };

    fn batch(epoch: u32, entries: Vec<Vec<u8>>, stream: Option<StreamId>) -> Batch {
        Batch { epoch: Epoch(epoch), entries, stream, first: None }
    }

    /// The space a test writes in: made by handing it a key.
    fn in_space(store: &Store, text: &str) -> i64 {
        let key = store.add_key(&space(text), Access::Write, "").unwrap();
        store.grant(&key.digest()).unwrap().unwrap().space
    }

    fn append(store: &Store, at: i64, stream: &str, batch: &Batch) -> Result<Written, AppendError> {
        store.append(at, &name(stream), batch, 0, ANY, |_, _| {})
    }

    #[test]
    fn the_schema_refuses_a_gap() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        append(&store, s, "s", &batch(1, vec![b"a".to_vec()], None)).unwrap();
        let conn = lock(&store.writer);
        let error = conn
            .execute("INSERT INTO entries (stream, seq, epoch, hash, data) VALUES (1, 3, 1, zeroblob(32), x'00')", [])
            .unwrap_err();
        assert!(error.to_string().contains("right after the head"), "{error}");
    }

    #[test]
    fn the_schema_refuses_an_older_epoch() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        append(&store, s, "s", &batch(2, vec![b"a".to_vec()], None)).unwrap();
        let conn = lock(&store.writer);
        let error = conn
            .execute("INSERT INTO entries (stream, seq, epoch, hash, data) VALUES (1, 2, 1, zeroblob(32), x'00')", [])
            .unwrap_err();
        assert!(error.to_string().contains("older than its stream"), "{error}");
    }

    #[test]
    fn the_schema_refuses_to_change_or_delete_an_entry() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        append(&store, s, "s", &batch(1, vec![b"a".to_vec()], None)).unwrap();
        let conn = lock(&store.writer);
        let error = conn.execute("UPDATE entries SET data = x'01'", []).unwrap_err();
        assert!(error.to_string().contains("cannot be changed"), "{error}");
        let error = conn.execute("DELETE FROM entries", []).unwrap_err();
        assert!(error.to_string().contains("cannot be deleted"), "{error}");
    }

    #[test]
    fn the_schema_keeps_a_stream_in_its_space() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        let other = in_space(&store, "b");
        append(&store, s, "s", &batch(1, vec![b"a".to_vec()], None)).unwrap();
        append(&store, other, "s", &batch(1, vec![b"b".to_vec()], None)).unwrap();
        let conn = lock(&store.writer);
        let error = conn.execute("UPDATE streams SET space = ?1 WHERE space = ?2", [other, s]).unwrap_err();
        assert!(error.to_string().contains("cannot move"), "{error}");
        let error = conn.execute("UPDATE streams SET name = 't' WHERE space = ?1", [s]).unwrap_err();
        assert!(error.to_string().contains("cannot move"), "{error}");
        // What the trigger guards, the unique key would too: one name per
        // space.
        conn.execute("DROP TRIGGER streams_stay_in_their_space", []).unwrap();
        let error = conn.execute("UPDATE streams SET space = ?1 WHERE space = ?2", [other, s]).unwrap_err();
        assert!(error.to_string().contains("UNIQUE"), "{error}");
    }

    #[test]
    fn a_name_is_its_own_in_each_space() {
        let (_dir, store) = store();
        let a = in_space(&store, "a");
        let b = in_space(&store, "b");
        let in_a = append(&store, a, "notes", &batch(1, vec![b"for a".to_vec()], None)).unwrap();
        let in_b = append(&store, b, "notes", &batch(1, vec![b"for b".to_vec()], None)).unwrap();
        assert_ne!(in_a.stream, in_b.stream);
        assert_eq!(store.read(a, &name("notes"), None, ALL).unwrap().entries[0].data, b"for a");
        assert_eq!(store.read(b, &name("notes"), None, ALL).unwrap().entries[0].data, b"for b");
        assert!(matches!(store.read(a + b + 1, &name("notes"), None, ALL), Err(ReadError::NotFound(_))));
        // A batch naming a's stream, written in b, meets b's stream.
        let refused = append(&store, b, "notes", &batch(1, vec![b"x".to_vec()], Some(in_a.stream)));
        assert!(matches!(refused, Err(AppendError::Replaced { .. })), "{refused:?}");
        let listed: Vec<(String, String)> = store.streams().unwrap().into_iter().map(|s| (s.space, s.name)).collect();
        assert_eq!(listed, [("a".into(), "notes".into()), ("b".into(), "notes".into())]);
    }

    #[test]
    fn a_writer_that_expects_a_stream_never_creates_one() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        let expected = StreamId::from_bytes([1; 16]);
        let refused = append(&store, s, "s", &batch(1, vec![b"a".to_vec()], Some(expected)));
        assert!(matches!(refused, Err(AppendError::NotFound { .. })), "{refused:?}");
        assert!(store.streams().unwrap().is_empty(), "a refused batch created a stream");

        let created = append(&store, s, "s", &batch(1, vec![b"a".to_vec()], None)).unwrap();
        let refused = append(&store, s, "s", &batch(1, vec![b"b".to_vec()], Some(expected)));
        assert!(matches!(refused, Err(AppendError::Replaced { .. })), "{refused:?}");
        let written = append(&store, s, "s", &batch(1, vec![b"b".to_vec()], Some(created.stream))).unwrap();
        assert_eq!(written.first, 2);
    }

    #[test]
    fn the_head_moves_with_the_entries() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        let written = append(&store, s, "s", &batch(1, vec![b"ab".to_vec(), b"cde".to_vec()], None)).unwrap();
        assert_eq!((written.first, written.last), (1, 2));
        let summary = &store.streams().unwrap()[0];
        assert_eq!((summary.head, summary.bytes), (2, 5));
        assert_eq!(store.used(s).unwrap(), 5);
        let page = store.read(s, &name("s"), None, ALL).unwrap();
        assert_eq!(page.head, 2);
        assert_eq!(page.cursor().unwrap().hash, page.entries[1].hash);
    }

    #[test]
    fn a_page_keeps_to_its_byte_budget_but_never_comes_back_empty() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        let big = vec![0u8; 100];
        append(&store, s, "s", &batch(1, vec![big.clone(), big.clone(), big], None)).unwrap();
        let page = store.read(s, &name("s"), None, PageLimit { entries: 10, bytes: 250 }).unwrap();
        assert_eq!(page.entries.len(), 2);
        let page = store.read(s, &name("s"), None, PageLimit { entries: 10, bytes: 10 }).unwrap();
        assert_eq!(page.entries.len(), 1);
    }

    #[test]
    fn a_batch_sent_again_lands_once() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        let first = append(&store, s, "s", &batch(1, vec![b"a".to_vec(), b"b".to_vec()], None)).unwrap();
        assert!(!first.repeated);
        let mut published = false;
        let again = store.append(s, &name("s"), &batch(1, vec![b"a".to_vec(), b"b".to_vec()], None), 0, ANY, |_, _| {
            published = true
        });
        assert_eq!(again.unwrap(), Written { repeated: true, ..first });
        assert!(!published, "a repeat moved no head, and woke nobody");
        assert_eq!(store.streams().unwrap()[0].head, 2);

        // Moved on since: an epoch above it, other batches after it.
        append(&store, s, "s", &batch(3, vec![b"c".to_vec()], Some(first.stream))).unwrap();
        let late = append(&store, s, "s", &batch(1, vec![b"a".to_vec(), b"b".to_vec()], Some(first.stream)));
        assert_eq!(late.unwrap(), Written { epoch: Epoch(3), repeated: true, ..first });
        assert_eq!(store.streams().unwrap()[0].head, 3);
    }

    #[test]
    fn limits_hold_a_batch_back_and_a_repeat_is_answered_past_them() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        let ten = vec![7u8; 10];
        let first = batch(1, vec![ten.clone(), ten], None);
        let written = store.append(s, &name("s"), &first, 40, ANY, |_, _| {}).unwrap();

        // The operator lowers every limit below what the batch took, and the
        // space is full: the batch sent again is still answered...
        let tight = WriteLimits { body: 10, entry: 5, quota: 20 };
        let again = store.append(s, &name("s"), &first, 40, tight, |_, _| {}).unwrap();
        assert_eq!(again, Written { repeated: true, ..written });

        // ...and a new one is refused, each limit for itself, writing nothing.
        let roomy = WriteLimits { quota: u64::MAX, ..tight };
        let new = batch(1, vec![vec![1u8; 3]], Some(written.stream));
        let refused = store.append(s, &name("s"), &new, 11, roomy, |_, _| {});
        assert!(matches!(refused, Err(AppendError::BodyTooLarge { size: 11, limit: 10 })), "{refused:?}");
        let big = batch(1, vec![vec![1u8; 6]], Some(written.stream));
        let refused = store.append(s, &name("s"), &big, 9, roomy, |_, _| {});
        assert!(matches!(refused, Err(AppendError::EntryTooLarge { size: 6, limit: 5 })), "{refused:?}");
        let refused = store.append(s, &name("s"), &new, 9, tight, |_, _| {});
        assert!(matches!(refused, Err(AppendError::QuotaExceeded { used: 20, size: 3, quota: 20 })), "{refused:?}");
        assert_eq!(store.streams().unwrap()[0].head, 2, "a refused batch wrote something");

        // The quota is the space's, all its streams together.
        let room = WriteLimits { quota: 23, ..ANY };
        store.append(s, &name("t"), &batch(1, vec![vec![1u8; 3]], None), 9, room, |_, _| {}).unwrap();
        let refused = store.append(s, &name("u"), &batch(1, vec![vec![1u8; 1]], None), 9, room, |_, _| {});
        assert!(matches!(refused, Err(AppendError::QuotaExceeded { used: 23, .. })), "{refused:?}");
        assert_eq!(store.streams().unwrap().len(), 2, "a refused batch created its stream");
    }

    #[test]
    fn the_schema_holds_a_batch_once_where_it_landed() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        let written = append(&store, s, "s", &batch(1, vec![b"a".to_vec()], None)).unwrap();
        let key = batch(1, vec![b"a".to_vec()], None).key();
        let conn = lock(&store.writer);
        let insert = |key: &[u8], first: u64, last: u64| {
            conn.execute(
                "INSERT INTO batches (stream, key, first, last) VALUES (1, ?1, ?2, ?3)",
                params![key, to_sql(first), to_sql(last)],
            )
        };
        let error = insert(&[7; 32], written.first, written.last).unwrap_err();
        assert!(error.to_string().contains("overlap"), "{error}");
        // What code that forgot to look would do: write the batch's entries
        // again, then record it.
        conn.execute("INSERT INTO entries (stream, seq, epoch, hash, data) VALUES (1, 2, 1, zeroblob(32), x'61')", [])
            .unwrap();
        let error = insert(key.as_bytes(), 2, 2).unwrap_err();
        assert!(error.to_string().contains("UNIQUE"), "{error}");
        let error = insert(&[7; 32], 2, 3).unwrap_err();
        assert!(error.to_string().contains("end at the head"), "{error}");
        let error = conn.execute("UPDATE batches SET last = 5", []).unwrap_err();
        assert!(error.to_string().contains("cannot be changed"), "{error}");
        let error = conn.execute("DELETE FROM batches", []).unwrap_err();
        assert!(error.to_string().contains("cannot be deleted"), "{error}");
    }

    #[test]
    fn a_key_opens_its_space_until_it_is_revoked() {
        let (_dir, store) = store();
        let writer = store.add_key(&space("alice"), Access::Write, "laptop").unwrap();
        let reader = store.add_key(&space("alice"), Access::Read, "phone").unwrap();
        let other = store.add_key(&space("bob"), Access::Write, "").unwrap();

        let grant = store.grant(&writer.digest()).unwrap().unwrap();
        assert_eq!((grant.space_name.as_str(), grant.access), ("alice", Access::Write));
        let read = store.grant(&reader.digest()).unwrap().unwrap();
        assert_eq!((read.space, read.access), (grant.space, Access::Read));
        assert_ne!(store.grant(&other.digest()).unwrap().unwrap().space, grant.space);
        assert_eq!(store.grant(&AccessKey::from_bytes([9; 32]).digest()).unwrap(), None);

        let listed = store.keys().unwrap();
        assert_eq!(listed.len(), 3);
        let laptop = listed.iter().find(|key| key.id == writer.id()).unwrap();
        assert_eq!((laptop.space.as_str(), laptop.label.as_str(), laptop.used_at), ("alice", "laptop", None));
        store.touch(&writer.digest(), 1_700_000_000).unwrap();
        let laptop = store.keys().unwrap().into_iter().find(|key| key.id == writer.id()).unwrap();
        assert_eq!(laptop.used_at, Some(1_700_000_000));

        let revoked = store.revoke(&reader.id()).unwrap();
        assert_eq!(revoked.label, "phone");
        assert_eq!(store.grant(&reader.digest()).unwrap(), None);
        assert!(store.grant(&writer.digest()).unwrap().is_some(), "revoking one key revoked another");
        assert!(matches!(store.revoke(&reader.id()), Err(RevokeError::NotFound(_))));
        assert!(matches!(store.revoke("ABCDEF01"), Err(RevokeError::BadId(_))));
        assert!(matches!(store.revoke("abc"), Err(RevokeError::BadId(_))));
    }

    #[test]
    fn the_database_keeps_no_key() {
        let (dir, store) = store();
        let key = store.add_key(&space("alice"), Access::Write, "").unwrap();
        drop(store);
        let mut bytes = Vec::new();
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            bytes.extend(std::fs::read(entry.unwrap().path()).unwrap_or_default());
        }
        let secret = key.expose();
        let digits = &secret.as_bytes()[efema_proto::ACCESS_KEY_PREFIX.len()..];
        assert!(!bytes.windows(digits.len()).any(|w| w == digits), "the key's text is in the database");
        let raw = <[u8; 32]>::try_from(&efema_proto_hex(digits)[..]).unwrap();
        assert!(!bytes.windows(32).any(|w| w == raw), "the key's bytes are in the database");
        let digest = key.digest();
        assert!(bytes.windows(32).any(|w| w == digest), "the digest is not where the test looks");
    }

    fn efema_proto_hex(digits: &[u8]) -> Vec<u8> {
        digits.chunks(2).map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()).collect()
    }

    #[test]
    fn spaces_report_their_counts() {
        let (_dir, store) = store();
        let a = in_space(&store, "a");
        store.add_key(&space("a"), Access::Read, "").unwrap();
        in_space(&store, "b");
        append(&store, a, "s", &batch(1, vec![b"ab".to_vec(), b"c".to_vec()], None)).unwrap();
        append(&store, a, "t", &batch(1, vec![b"d".to_vec()], None)).unwrap();
        let spaces = store.spaces().unwrap();
        assert_eq!(spaces.len(), 2);
        assert_eq!((spaces[0].streams, spaces[0].entries, spaces[0].bytes, spaces[0].keys), (2, 3, 4, 2));
        assert!(spaces[0].written_at.is_some() && spaces[0].used_at.is_none());
        assert_eq!((spaces[1].streams, spaces[1].bytes, spaces[1].written_at), (0, 0, None));
    }

    fn placed(epoch: u32, entries: Vec<Vec<u8>>, stream: Option<StreamId>, first: u64) -> Batch {
        Batch { first: Some(first), ..batch(epoch, entries, stream) }
    }

    fn ends(page: &Page) -> Vec<(u64, bool)> {
        page.entries.iter().map(|entry| (entry.seq, entry.ends_batch)).collect()
    }

    #[test]
    fn a_batch_that_says_where_it_goes_lands_there_or_nowhere() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        let one = batch(1, vec![b"a".to_vec(), b"b".to_vec()], None);
        let id = append(&store, s, "s", &one).unwrap().stream;
        for wrong in [1, 2, 4, 9] {
            let error = append(&store, s, "s", &placed(1, vec![b"c".to_vec()], Some(id), wrong)).unwrap_err();
            assert!(matches!(error, AppendError::OutOfPlace { first, head: 2 } if first == wrong), "{error}");
        }
        assert_eq!(store.state(s, &name("s")).unwrap().head, 2, "a batch out of place wrote something");
        let written = append(&store, s, "s", &placed(1, vec![b"c".to_vec()], Some(id), 3)).unwrap();
        assert_eq!((written.first, written.last, written.repeated), (3, 3, false));
        // A batch the stream has is answered where it is, wherever it asked
        // to go: the answer its writer lost comes before the place.
        let again = append(&store, s, "s", &Batch { first: Some(8), ..one }).unwrap();
        assert_eq!((again.first, again.last, again.repeated), (1, 2, true));
        // A new stream's next place is 1.
        let error = append(&store, s, "t", &placed(1, vec![b"x".to_vec()], None, 2)).unwrap_err();
        assert!(matches!(error, AppendError::OutOfPlace { first: 2, head: 0 }), "{error}");
        assert_eq!(store.state(s, &name("t")).unwrap(), StreamState::default(), "a refused batch made a stream");
    }

    #[test]
    fn a_copy_takes_the_identity_of_the_stream_it_copies() {
        let (_dir, store) = store();
        let a = in_space(&store, "a");
        let b = in_space(&store, "b");
        let first = batch(1, vec![b"a".to_vec(), b"b".to_vec()], None);
        let id = append(&store, a, "s", &first).unwrap().stream;
        let second = batch(2, vec![b"c".to_vec()], Some(id));
        append(&store, a, "s", &second).unwrap();
        let original = store.read(a, &name("s"), None, ALL).unwrap();

        // Into another space, and under another name in the same one: the
        // same identity, the same links, the same batches.
        for (space, stream) in [(b, "s"), (a, "copy")] {
            let written = append(&store, space, stream, &Batch { stream: Some(id), first: Some(1), ..first.clone() });
            assert_eq!(written.unwrap().stream, id);
            append(&store, space, stream, &Batch { first: Some(3), ..second.clone() }).unwrap();
            let copy = store.read(space, &name(stream), None, ALL).unwrap();
            assert_eq!(copy, original, "the copy in {stream} is not the stream it copies");
            let again = append(&store, space, stream, &Batch { stream: Some(id), ..first.clone() }).unwrap();
            assert_eq!((again.first, again.repeated), (1, true), "the copy does not know its batches");
        }

        // Only a batch that starts the stream makes it.
        for wrong in [None, Some(2)] {
            let error = append(&store, b, "t", &Batch { stream: Some(id), first: wrong, ..first.clone() }).unwrap_err();
            assert!(matches!(error, AppendError::NotFound { .. }), "{error}");
        }
    }

    #[test]
    fn a_page_says_where_each_batch_ends() {
        let (_dir, store) = store();
        let s = in_space(&store, "a");
        let entries = |n: u8| (0..n).map(|i| vec![n, i]).collect::<Vec<_>>();
        let id = append(&store, s, "s", &batch(1, entries(2), None)).unwrap().stream;
        append(&store, s, "s", &batch(1, entries(1), Some(id))).unwrap();
        append(&store, s, "s", &batch(1, entries(3), Some(id))).unwrap();
        let all = store.read(s, &name("s"), None, ALL).unwrap();
        assert_eq!(ends(&all), [(1, false), (2, true), (3, true), (4, false), (5, false), (6, true)]);

        // A page that stops inside a batch, and one that starts inside one.
        let head = store.read(s, &name("s"), None, PageLimit { entries: 4, bytes: usize::MAX }).unwrap();
        assert_eq!(ends(&head), [(1, false), (2, true), (3, true), (4, false)]);
        let cursor = all.entries[0].cursor(id);
        let tail = store.read(s, &name("s"), Some(&cursor), PageLimit { entries: 1, bytes: usize::MAX }).unwrap();
        assert_eq!(ends(&tail), [(2, true)]);
        let cursor = all.entries[3].cursor(id);
        let tail = store.read(s, &name("s"), Some(&cursor), ALL).unwrap();
        assert_eq!(ends(&tail), [(5, false), (6, true)]);
    }

    #[test]
    fn an_entry_from_before_batches_were_recorded_is_a_batch_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let id = StreamId::from_bytes([6; 16]);
        {
            let mut conn = Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATIONS[0]).unwrap();
            tx.pragma_update(None, "user_version", 1).unwrap();
            tx.execute(
                "INSERT INTO streams (name, id, epoch, head_hash, created_at, updated_at) VALUES ('s', ?1, 1, ?2, 0, 0)",
                params![id.as_bytes(), chain::genesis(&id).as_bytes()],
            )
            .unwrap();
            let mut link = chain::genesis(&id);
            for seq in 1..=2u64 {
                link = chain::link(&link, seq, Epoch(1), b"old");
                tx.execute(
                    "INSERT INTO entries (stream, seq, epoch, hash, data) VALUES (1, ?1, 1, ?2, x'6f6c64')",
                    params![seq as i64, link.as_bytes()],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        let key = store.add_key(&space(DEFAULT_SPACE), Access::Write, "").unwrap();
        let s = store.grant(&key.digest()).unwrap().unwrap().space;
        append(&store, s, "s", &batch(1, vec![b"a".to_vec(), b"b".to_vec()], Some(id))).unwrap();
        let page = store.read(s, &name("s"), None, ALL).unwrap();
        assert_eq!(ends(&page), [(1, true), (2, true), (3, false), (4, true)]);
    }

    /// A database as the release before this one left it: schema 2, with a
    /// stream and the batch that wrote it.
    fn release_before(dir: &Path, with_a_stream: bool) -> StreamId {
        let id = StreamId::from_bytes([5; 16]);
        let link = chain::link(&chain::genesis(&id), 1, Epoch(1), b"old");
        let mut conn = Connection::open(dir.join(DATABASE_FILE)).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute_batch(MIGRATIONS[0]).unwrap();
        tx.execute_batch(MIGRATIONS[1]).unwrap();
        tx.pragma_update(None, "user_version", 2).unwrap();
        if with_a_stream {
            tx.execute(
                "INSERT INTO streams (name, id, epoch, head_hash, created_at, updated_at)
                 VALUES ('s', ?1, 1, ?2, 0, 0)",
                params![id.as_bytes(), chain::genesis(&id).as_bytes()],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO entries (stream, seq, epoch, hash, data) VALUES (1, 1, 1, ?1, x'6f6c64')",
                [link.as_bytes()],
            )
            .unwrap();
            let key = batch(1, vec![b"old".to_vec()], None).key();
            tx.execute("INSERT INTO batches (stream, key, first, last) VALUES (1, ?1, 1, 1)", [key.as_bytes()])
                .unwrap();
        }
        tx.commit().unwrap();
        id
    }

    #[test]
    fn a_relay_of_the_previous_release_is_carried_into_the_default_space() {
        let dir = tempfile::tempdir().unwrap();
        let id = release_before(dir.path(), true);
        let store = Store::open(dir.path()).unwrap();
        let listed = store.streams().unwrap();
        assert_eq!((listed[0].space.as_str(), listed[0].name.as_str(), listed[0].id), (DEFAULT_SPACE, "s", id));

        let key = store.add_key(&space(DEFAULT_SPACE), Access::Write, "").unwrap();
        let s = store.grant(&key.digest()).unwrap().unwrap().space;
        assert_eq!(store.read(s, &name("s"), None, ALL).unwrap().entries[0].data, b"old");
        // The batch written before is still known, and the triggers that came
        // back still hold the head.
        let again = append(&store, s, "s", &batch(1, vec![b"old".to_vec()], Some(id))).unwrap();
        assert_eq!((again.first, again.repeated), (1, true));
        let written = append(&store, s, "s", &batch(1, vec![b"new".to_vec()], Some(id))).unwrap();
        assert_eq!((written.first, written.last), (2, 2));
        // The identity is not unique any more: a copy into another space
        // takes it.
        let other = in_space(&store, "other");
        let copy = append(&store, other, "s", &placed(1, vec![b"old".to_vec()], Some(id), 1)).unwrap();
        assert_eq!(copy.stream, id);
        let conn = lock(&store.writer);
        let error = conn
            .execute("INSERT INTO entries (stream, seq, epoch, hash, data) VALUES (1, 9, 1, zeroblob(32), x'00')", [])
            .unwrap_err();
        assert!(error.to_string().contains("right after the head"), "{error}");
        let enforced: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0)).unwrap();
        assert_eq!(enforced, 1, "foreign keys stayed off after the migration");
    }

    #[test]
    fn an_empty_relay_of_the_previous_release_gets_no_default_space() {
        let dir = tempfile::tempdir().unwrap();
        release_before(dir.path(), false);
        let store = Store::open(dir.path()).unwrap();
        assert!(store.spaces().unwrap().is_empty());
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
