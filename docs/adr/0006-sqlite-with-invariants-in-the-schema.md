# ADR 0006: SQLite, with the invariants in the schema

- Status: accepted
- Date: 2026-10-08

## Context

The relay is meant to run on a small home server as well as anywhere else, by
people who do not want to operate a database. Its storage needs are narrow:
append batches atomically, read ranges in order, and never lose an
acknowledged write.

## Decision

- One **SQLite** database in the data directory, WAL mode, `synchronous = FULL`:
  a write the relay acknowledges has been synced to disk, because a client stops
  resending what was acknowledged. Readers use their own read-only connections
  and see a consistent snapshot; writes go through one connection, one
  `IMMEDIATE` transaction per batch.
- The promises are **enforced by the schema**, not only by the code above it:
  triggers refuse an entry that does not take the position right after the
  head, an entry in an epoch older than its stream, and any update or delete
  of an entry. The head, its link and the stream's size move in the same
  statement that inserts the entry.
- Reads use an explicit `ORDER BY` on the position and check that what comes
  back has no gaps; a gap is an internal error, not a page.
- The schema version is `PRAGMA user_version`; migrations are append-only. A
  database newer than the binary is refused, not misread.
- One serving process per directory, held by an OS lock on a lock file: two
  relays on one database would each keep their own waiters, and neither would
  wake the other's.

## Consequences

- A relay is a binary and a directory; a backup is one file, taken with
  SQLite's online backup.
- A bug in the code that would corrupt a stream fails the write instead.
  Each of these refusals is held by a test, and each test was checked by
  removing the rule it guards.
- Retention (cutting a stream at a snapshot, planned for 1.x) will have to lift
  the delete rule in a migration of its own - deliberately.
- Write throughput is one writer at a time, with an fsync per batch. For a
  relay of one person's devices that is far beyond need.
