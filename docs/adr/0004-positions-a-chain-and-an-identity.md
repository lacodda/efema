# ADR 0004: Positions, a hash chain and an identity per stream

- Status: accepted
- Date: 2026-10-08

## Context

A reader syncs by remembering how far it has read and asking for what came
after. The obvious cursor is a position: "I have up to 120." It is correct
exactly as long as position 120 on the relay is the entry the reader has.

Two ordinary events break that. A relay restored from yesterday's backup
reaches position 120 again once new entries arrive - with a different entry
120. A relay whose data was wiped starts every stream at 1 again under the same
names. In both cases a position-only cursor reads on as if nothing happened and
silently, permanently skips what the relay lost. A transport whose promise is
"a batch arrives or honestly does not" cannot have that failure mode.

## Decision

- **Positions** are assigned by the relay, per stream, from 1, with no gaps.
  Batches take consecutive positions, whole or not at all.
- Every entry carries a **link**: SHA-256 over the previous link, the position,
  the epoch, the length and the data (fixed-width and length-prefixed, so no
  two different entries hash the same input). The first link is derived from
  the stream's identity under a domain-separation prefix.
- Every stream has an **identity**: sixteen random bytes drawn when it is
  created, never changed.
- A **cursor** is identity, position and the link at that position. The relay
  checks all three and refuses with a distinct code for each way they can
  fail: `stream_replaced`, `cursor_ahead`, `cursor_diverged`.

Writes do not return a cursor. A writer that has not read what others wrote
before its batch must not jump its read cursor past it; it learns its own
positions and reads its entries back like everyone else's.

## Consequences

- A restore, a wipe or a bug that rewrites history is detected by every reader
  at its next read, instead of by nobody.
- The chain is protection against accidents, not against a dishonest relay,
  which can compute links for any history. That limit is stated in the threat
  model.
- The chain's construction is part of the wire format: changing it invalidates
  every cursor held anywhere. Its values are frozen in a test computed outside
  Rust.
- 32 bytes per entry on the wire and on disk, and one SHA-256 per entry on
  write.
