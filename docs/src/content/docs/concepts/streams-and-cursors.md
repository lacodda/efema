---
title: Streams and cursors
description: The model the relay keeps - streams, batches, positions, the hash chain, cursors and epochs - and what it leaves to the app.
---

The relay keeps one kind of thing: **streams of opaque entries, in order.**
Everything else - what an entry means, how two changes combine, which device
is right - belongs to the app.

## Streams

A stream has a **name**, which goes into the URL: one to 64 characters of
lowercase letters, digits, `-`, `_` and `.`, starting with a letter or a digit.
An app usually keeps one stream per thing it syncs - a notes vault, a
calendar.

A stream also has an **identity**: sixteen random bytes drawn when its first
batch arrives, never changed after. The name is how you address a stream; the
identity is how a reader knows it is still the same one. If a relay's data is
wiped and a device writes to `notes` again, that is a new stream with the old
name - and a reader holding a cursor from the old one is told so, rather than
reading position 40 of a stream that has nothing to do with its own
position 40.

A stream exists from its first batch on. There is no "create" call, and in
this version no delete.

## Batches and positions

A device writes a **batch**: one or more entries, each an arbitrary run of
bytes. The relay gives the entries the next positions - 1, 2, 3 and on, per
stream - in the order the batch lists them.

Two promises hold for every batch, whoever else is writing at the same time:

- **Whole or nothing.** A batch lands completely or not at all.
- **Together.** Its entries take consecutive positions; no other writer's
  entry lands between two entries of one batch.

And one for every stream: **one order for everyone.** Positions have no gaps
and no repeats, and every reader reads the same sequence - whatever page size
it asks for and however many writers raced.

Entries cannot be changed or deleted once written. That is not a policy of the
code above the database: the database itself refuses it.

## The hash chain

Every entry carries a **link**: SHA-256 over the link before it and the entry
itself - its position, its epoch, its length and its bytes. The chain starts
from a link derived from the stream's identity.

The point is the cursor. A position alone does not say *which* history it
belongs to: a relay restored from last night's backup will reach position 120
again once new entries arrive, but its entry 120 is not the one a reader
already has. Reading on from 120 would skip - silently, for good - everything
between the backup and the restore. With the link in the cursor the relay can
tell, and it refuses.

The chain guards against accidents: a restore, a bug, a disk that lied. It is
not a defence against a dishonest relay, which can compute links as well as
anyone - see the [threat model](/efema/concepts/threat-model/).

## Cursors

A **cursor** is where a reader stands: the stream's identity, the position of
the last entry the reader has, and the link at that position. Its text form is
`<position>.<identity>.<link>`:

```
2.11c3cd7e971ccb544d78c74ae8d814ba.19cdc65a0a991e8d40e19813aad37514cf0a734663686eb4433285f7d49190ed
```

A reader starts with no cursor and reads from the beginning. Each page of
entries says where it ends; after applying a page, the reader keeps the cursor
of its last entry. A cursor at position 0 - "I know this stream and have
nothing of it yet" - holds the chain's first link.

When a reader presents a cursor, the relay checks all three parts and answers
with exactly one of:

| The relay finds | It answers |
| --- | --- |
| the same identity, the same link at that position | the entries after it |
| another identity under that name | `stream_replaced` |
| a last position below the cursor's | `cursor_ahead`, with its head |
| a different link at that position | `cursor_diverged` |

Each of the three refusals means the same thing for the reader - its copy and
the relay's have parted - and each says how: the stream is new, the relay lost
entries, or the relay's history went another way. What to do about it is the
app's call; the relay's job is never to let it go unnoticed.

## Epochs

An app changes the format of what it writes from time to time. The **epoch**
is how it tells the stream: a number, written with every batch, that only goes
up.

- A batch in the stream's current epoch is appended.
- A batch in a higher epoch is appended and raises the stream to it.
- A batch in a lower epoch is refused with `epoch_behind`: an old version of
  the app cannot mix entries in a format the others have moved past.

Every entry keeps the epoch it was written in, so a reader that only knows
epoch 3 can read up to the first entry of epoch 4 and stop there - and ask for
an update - instead of misreading it.

The relay never knows what an epoch means. For it, epochs are numbers it
compares.

## Waiting

A device that is up to date does not poll on a timer: it asks the relay to
**wait** - one request, naming the streams it cares about and its cursor in
each. The relay answers as soon as any of them moves, or after a timeout with
nothing to report. The answer carries positions only, never entries; the
device then reads what it missed.

A wait wakes for anything that does not match what the waiter said it knew:
new entries, but also a stream with another identity or fewer entries than the
cursor says. Sleeping through those would leave the device waiting for a
stream that is never going to move.

## What the relay leaves to the app

- **What entries mean.** The relay stores bytes. The client library seals them
  before they leave the device; the relay could not read them even if it
  tried.
- **Merging and conflicts.** Two devices that edit the same note offline both
  write their batch, and both are kept, in order. Which edit wins, or how they
  combine, depends on what a note is - which only the app knows.
- **Echoes.** A device reads its own batches back along with everyone else's.
  It knows the positions it wrote at; skipping them is its decision.
