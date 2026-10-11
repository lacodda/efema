# ADR 0013: A batch is known by what it holds, and written once

- Status: accepted
- Date: 2026-10-11

## Context

A write can fail at three points: before the relay has the batch, after it
has written it but before the answer arrives, or not at all. The writer
cannot tell the second from the first - in both, the connection broke and no
answer came. Sending again after the first is right; sending again after the
second, on a relay that does not know the batch, writes it twice: two copies
of a change in every device's history, forever.

The usual remedy is an idempotency key the client chooses and sends along. The
client then has to keep the key with the batch, a key reused by a bug or a
second device is a batch silently dropped, and the relay has to decide how
long a key is remembered.

## Decision

- **The relay computes the key from the batch itself**:
  `SHA-256("efema/batch/v1" || epoch: u32 || count: u64 || (length(data): u64 || data) for each entry)`.
  The stream a batch names is not in it. The writer keeps nothing beyond the
  batch, and sends it again byte for byte.
- **The relay keeps the key of every batch it writes** - in a `batches` table,
  with where the batch landed, in the transaction that writes its entries. The
  schema holds a batch once per stream, ending at the head it moved and after
  the batch before it, unchanged and never deleted (as ADR 0006 holds
  entries).
- **A batch the stream has is answered, not written**: the `written` record of
  where it landed, with `repeated: true` (key 4, present only when true).
- **For the life of the stream, not a window.** A window is a time after which
  the same retry writes a duplicate again; a phone that comes back online
  after a week is exactly the writer this is for.
- **The incarnation first, then the repeat, then the epoch.** A batch that
  names another incarnation is refused as before; a batch the stream has is
  answered even if the stream moved to a newer epoch since - it was written
  before that, and its writer is owed the answer it lost.
- The key is a contract of protocol version 1, frozen by a test with values
  computed outside Rust: relays keep it, and a transport without a relay (S3,
  0.7) will compute it itself.

## Consequences

- Equal batches are one batch. The protocol says so; for efema's own client it
  never matters, since every entry is sealed under a random nonce and no two
  batches it writes are equal. A raw writer that sends the same bytes twice on
  purpose gets them once.
- A batch cut differently - the same entries in two batches instead of one -
  is another batch. The client therefore cuts once, before it keeps the
  batch, and sends it again exactly as cut (ADR 0014).
- Thirty-two bytes and two positions per batch on the relay. Relays of 0.3
  have no keys for the batches they wrote before; those are not known as
  repeats - there are no such relays outside tests.
- Clients of 0.3 ignore key 4, as they ignore any key they do not know; they
  never send a batch twice anyway.
