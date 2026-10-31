# ADR 0022: A stream is copied batch by batch, each batch in its place, through the protocol

- Status: accepted
- Date: 2026-10-31

## Context

A stream could move between relays by copying the data directory, and
between buckets by copying the objects with their metadata - both keep the
stream's identity, so every device reads on from its cursor. Between a relay
and a bucket nothing moved it: the way across was a new stream, a new key,
every device forgetting the old one and the app pushing its data again.

What a device holds on to, a move has to keep:

- **the stream's identity, each entry's position and epoch, and its bytes** -
  so every link of the chain is the same, and with it every cursor;
- **each batch as it was written** - a batch waiting on a device, whose answer
  was lost on the way back from the old place, is sent again to the new one;
  known there by its key, it is answered as a repeat, and not written twice.

The protocol had neither half of what a copy needs. A page did not say where
one batch ends and the next begins, and a batch could not say where it goes:
it went to the next place, and a stream it named had to be there already.

## Decision

**A copy goes through the protocol**, as `efema::copy(from, to, stream)` over
any two transports - relay to relay, relay to bucket and back, bucket to
bucket, and any transport to come. It reads the stream page by page and
writes each batch to the target with the stream's identity and the position
of its first entry. Nothing is opened: entries go sealed, byte for byte, and
a copy needs no key of the stream - an access key that reads the source and
one that writes the target.

Two fields and a refusal, added within protocol version 1:

- **`ends_batch` in an entry** - `true` on the last entry of the batch it was
  written in. A relay that holds entries from before it recorded batches
  (0.1 to 0.3) calls each of them a batch of its own.
- **`first` in a batch** - the position its first entry must take. Written
  there, or refused with `out_of_place` and the stream's `head`. With `first`
  1 and a `stream` the target does not have, the target creates the stream
  with that identity. A repeat is answered before the place is looked at, as
  every check of a write comes after the repeat (ADR 0017).

A copy is safe to run again, and to run while devices still write to the
source: it reads the stream from the start, checks that what the target has
is the stream's history - one read after the cursor where the target ends,
refused if the link there differs - writes the rest, and checks the target's
link at the end. A copy that stopped goes on; one that ran writes nothing;
the second copy after devices moved brings over what a straggler wrote to the
old place.

A stream's identity is no longer unique on a relay (schema 4): a copy into
another space, or under another name, takes the identity it copies.

A device cuts its batches leaving room for `first` - ten bytes, at the
largest position - so a batch cut to a relay's limits can be copied whole to
a relay with the same ones.

## Alternatives

- **A relay command over its database** (`streams export` / `import`). Only
  between relays, which copying the data directory already does; a bucket
  would need a second tool, and a future transport a third.
- **Checking where a batch landed after writing it**, with no `first`. A copy
  that met another writer on the target would have written its batch past the
  other one's before it knew - the target's history parted from the stream's
  by the copy itself. With `first` a wrong copy cannot be written. The check
  stays, for a target older than `first`, which places a batch wherever its
  next place is.
- **Cutting the stream into new batches** - fewer requests on a big stream.
  A batch waiting on a device would no longer be known at the target, and
  would land twice.
- **Retrying inside the copy** when a relay says `rate_limited`. The library
  does not retry on its own (ADR 0014, 0017): when to go on is the app's to
  decide. The copy stops; run again, it goes on from where the target ends.

## Consequences

- A copy needs a target of efema 0.9 or later: an older relay refuses the
  first batch of a stream it does not have (`CopyError::OldTarget`), and
  places any other wherever its next place is - caught by where it says it
  went (`CopyError::Parted`).
- A copy needs a source of 0.9 or later: an older relay's pages say nothing of
  batches (`CopyError::Unmarked`).
- A relay's operator, or anyone with a key that reads a space, can copy its
  streams elsewhere - as they could read them before; what they copy is
  sealed.
- Moving a stream between a relay and a bucket is three steps - copy, point
  the devices at the target, copy again - and the devices keep their state.
