---
title: Moving a stream
description: Copy a stream to another relay, another space or a bucket - and back - with every device going on from where it stood, nothing read twice and nothing written twice.
---

A stream can move: from a relay to a bucket, from a bucket to a relay, from
one relay to another, or into another space of the same relay. `efema::copy`
writes every batch of it to the new place at the position it has, so what
each device holds on to holds there too:

- **its cursor** - the stream keeps its identity, and every entry its position,
  epoch and bytes, so every link of the chain is the same;
- **what it was still sending** - each batch is copied as it was written, so a
  batch whose answer was lost on the way back from the old place is known at
  the new one, and not written twice.

Nothing is opened on the way. The entries go sealed, byte for byte: a copy
takes no key of the stream, only an access key that reads the old place - or
credentials that read the bucket - and one that writes the new.

## In an app

```rust
let stream: efema::StreamName = "notes".parse()?;
let copied = efema::copy(&relay, &bucket, &stream).await?;
println!("{} batches over; the two agree up to {}", copied.batches, copied.cursor.seq);
```

Then each device opens the stream through the new place, with the state it
has - the same `State`, the same stream - and goes on: `pull` reads from its
cursor, `send` sends what waited.

## The order of a move

1. **Copy.** Devices may go on writing to the old place meanwhile: the copy
   reads the stream up to its end as it gets there.
2. **Point the devices at the new place**, as each app learns of it.
3. **Copy again**, once no device writes to the old place: it brings over
   what was written there since - by a device that had not moved yet - and
   says when there was nothing. Then the old place can go.

A copy is safe to run again at any time. It reads the stream from the start,
checks that what the new place has is this stream's history, writes only the
rest, and checks the new place's link at the end. A copy that stopped - a torn
connection, a relay that wants fewer requests a minute - goes on from where
the new place ends.

What a copy cannot mend is a stream that went two ways: a device writing to
the old place after another one wrote to the new. The second copy stops with
`CopyError::Parted` and the position where the two part. The device that
stayed behind is told the same when it moves - `Error::CursorAhead` or
`Error::CursorDiverged` - and does what a device does after a relay restored
from an older copy: `State::forget`, join the stream at the new place, and send
what its own data has that the stream lacks
([When something goes wrong](/efema/guides/when-something-goes-wrong/#restored-from-an-older-copy)).

## With the example app

`mini-vault copy` copies the folder's stream to `--to-relay` (with the key in
`MINI_VAULT_TO_KEY`) or to `--to-bucket` (`--to-region`, `--to-prefix`). Here
a folder synced by three devices moves from a relay into a bucket, under the
prefix `apps/vault`; the phone moves last, and writes to the relay in
between:

```console
$ mini-vault --relay http://127.0.0.1:2451 copy --to-bucket http://127.0.0.1:9000/notes --to-prefix apps/vault
copied 2 batch(es), 3 entries: vault is the same at http://127.0.0.1:9000/notes/apps/vault up to position 3
$ mini-vault --bucket http://127.0.0.1:9000/notes --prefix apps/vault --folder ./desktop --state ./desktop-state sync --once
$ mini-vault --relay http://127.0.0.1:2451 --folder ./phone --state ./phone-state sync --once
applied 0 change(s), sent 1
$ mini-vault --relay http://127.0.0.1:2451 copy --to-bucket http://127.0.0.1:9000/notes --to-prefix apps/vault
copied 1 batch(es), 1 entries: vault is the same at http://127.0.0.1:9000/notes/apps/vault up to position 4
$ mini-vault --bucket http://127.0.0.1:9000/notes --prefix apps/vault --folder ./phone --state ./phone-state sync --once
$ mini-vault --bucket http://127.0.0.1:9000/notes --prefix apps/vault --folder ./desktop --state ./desktop-state sync --once
applied 1 change(s), sent 0
$ mini-vault --bucket http://127.0.0.1:9000/notes --prefix apps/vault --folder ./desktop --state ./desktop-state doctor
relay    http://127.0.0.1:9000/notes/apps/vault - answered in 7 ms
access   reads and writes - no quota
stream   vault - stream 9f79245c, epoch 1, 5 entries
cursor   at 5 - up to date
queue    empty
key      5ed3d90399239b99 - locked 2026-10-31 (today), argon2id 64 MiB × 3 × 4
device   5f1715f4
epoch    1 - written, and read up to
```

The desktop, moved first, has nothing to apply: it had read all of it on the
relay. The phone's change made on the relay reaches the bucket with the second
copy, and the phone, moved last, reads nothing it has.

## When a copy stops

| `CopyError` | What happened | What to do |
| --- | --- | --- |
| `Source`, `Target` | the old place or the new one could not be reached, or refused - its reason inside: a key that only reads, a full space, `rate_limited` with the seconds to wait | mend that, or wait, and copy again: it goes on |
| `Taken` | the new place has another stream under the name | copy to another name, or another space or prefix |
| `Parted` | the new place has another history of this stream from a position on: something else was written there | see above: the stream went two ways |
| `Unmarked` | the old place does not say where its batches end: a relay older than efema 0.9 | update it, and copy |
| `OldTarget` | the new place does not take a copy: a relay older than efema 0.9 | update it, and copy |
| `BrokenChain` | the stream as the old place sent it does not hold together | nothing was written past the entry that broke; the old place's data is damaged |

A copy writes as many requests as the stream has batches. A relay holds each
key to its requests a minute: a long stream is copied in several runs, each
going on from the last.

A batch larger than the new place takes cannot be copied whole, and is not
cut: the new place's limits have to be at least the old one's. A device cuts
its batches with room for the place a copy adds, so between two relays with
the same limits every batch fits.

## Related

- [When something goes wrong](/efema/guides/when-something-goes-wrong/#the-relay-moves) -
  moving a relay with its data directory, and what devices see after a
  restore.
- [Without a server](/efema/guides/without-a-server/) - syncing through a
  bucket, under a prefix of it.
- [Protocol](/efema/reference/protocol/#a-batch-in-its-place) - the batch that
  says where it goes, and the entry that says where its batch ends.
