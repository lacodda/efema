# ADR 0020: A stream in a bucket, kept in order by creating only what is not there

- Status: accepted
- Date: 2026-10-23

## Context

A relay is a server someone has to run. Many people who want their devices in
sync have no server, and do have, or can get in a minute, an S3 bucket - from
Amazon, from one of the services that speak its protocol, or a MinIO of their
own. The client was built for this from the start: a [`Transport`] appends
batches, reads pages and waits, and everything else - sealing, cursors, keys,
the queue - lives above it (ADR 0010).

What the relay does in one SQLite transaction, a bucket has to do with no one
in between: give a batch the next positions, all of them or none; never two
batches in one place; know a batch sent again; tell a reader which history its
cursor is in. Object storage has no transactions. It has one atomic thing: a
`PUT` with `If-None-Match: *` creates the object only if there is none under
its key, and every other such `PUT` gets `412 Precondition Failed`. Amazon S3
has done it since 2024, MinIO does; services that speak S3 vary, and some take
the header and ignore it.

## Decision

- **`efema::Bucket`, a second `Transport`, in the client crate.** Not a second
  client, not a feature flag: the protocol over another medium, with no new
  dependency. Requests are signed with Signature Version 4 - HMAC-SHA256 over
  `sha2`, already in the tree - and the four kinds of request a bucket needs
  (`GET`, `HEAD`, `PUT`, `ListObjectsV2`) go through the same progress guard as
  the relay's (ADR 0019). An SDK would bring a second HTTP stack and requests
  the guard cannot see into.
- **A batch is one object, under the position of its first entry, created
  only if nothing is there.** Two batches can never take one place; the next
  place is always the one after the last batch's last entry, so a stream has
  no gaps and no batch in half. A writer that loses a place reads the head
  again and tries the next one.
- **The position's digits are written from nines** (`9 - d` for each digit,
  twenty of them), so a listing - which S3 returns in key order - starts with
  the newest batch. The newest batch is the first key of one listing; the
  batch holding any position is the first key of a listing that starts after
  the key of the position after it. A reader whose cursor stopped inside a
  batch costs one request to place, not a scan.
- **An offer before every attempt.** Before a writer tries a place, it writes
  an empty object `o/<batch key>/<position>`. A batch can only be where one of
  its offers says, so a batch sent again - its answer lost - is found by its
  key and answered with where it landed. It is looked for after the head is
  read, and again after every place lost: a batch at or below the head a
  writer saw was created after its offer, so the offer is there to be seen.
  Looked for before the head, two sendings of one batch could each miss the
  other and write it twice - a mutation found exactly that.
- **The batch's object holds the link before it**, and its metadata its last
  position, epoch, last link and key. Readers compute every link from the one
  before, so a cursor is checked against the history as on a relay; the
  metadata lets the head, a cursor at a batch's end and a repeat be found with
  a `HEAD`. A batch whose metadata disagrees with what it holds is not read.
- **The stream's identity is an object of its own**, `<stream>/stream`,
  created once the same way, and every batch lives under it. Two devices
  starting one stream at once start one.
- **A bucket is checked before it is written to.** Each `Bucket` creates the
  object `.efema-probe` - twice if it was not there - and refuses to write if
  the second creation succeeds: a service that ignores the condition would
  fork a stream without a word. The same request tells whether the
  credentials write (`403` means they only read).
- **A wait looks.** Nothing in a bucket wakes a waiting device; a wait asks
  every five seconds - `Bucket::POLL` - with two of the cheapest requests S3
  has: the stream's record and a `HEAD` of the next place.
- **Limits are announced, not enforced by anyone.** A bucket announces the
  protocol's defaults, no quota (`u64::MAX`) and nothing counted (`used: 0`);
  the client cuts its batches to them as for a relay.

## Consequences

- A device with an S3 bucket and its credentials syncs with no server of its
  own, with the same seal, the same order, the same cursors and the same
  exactly-once writing as through a relay. Tests against a real MinIO hold
  each of those, and every key property is broken once per release to see a
  test fail.
- A bucket keeps more objects than a relay keeps rows: one per batch, one
  empty offer per attempt, and the record. Offers stay; at a few bytes each
  they cost requests, not storage.
- A wait is a poll: a device learns of a write up to five seconds late, and a
  device that waits all day makes some 35,000 cheap requests per stream.
  Waking at once is what a relay is for.
- The bucket's owner sees what a relay's operator sees - names, sizes, times -
  and can do what a dishonest relay can: withhold, delay, delete. Nothing in
  a bucket can be read without the stream's key.
- The layout is a contract between devices, like the wire format: an object
  one release writes, every other reads. It carries a format number in the
  stream's record, and is frozen with the wire format at 1.0.

[`Transport`]: https://docs.rs/efema/latest/efema/trait.Transport.html
