---
title: When something goes wrong
description: What to do when the connection tears, a key changes, a space fills up, or the relay moves - what a device sees, and what the app and the operator do about it.
---

Each case below says what happened, what a device sees, and what to do. One
thing holds through all of them: a change a device pushed waits on it, sealed,
until it can go. Nothing here loses one - at worst it waits, or the app sends
it again from its own data.

## The connection tears

A phone on a train, a laptop closing its lid, a relay restarting, a proxy that
stops answering.

| A device sees | What happened |
| --- | --- |
| `Error::Unsent`, its source an `Error::Transport` | a push or a send did not get through; what was pushed waits on the device |
| `Error::Transport` from `pull` or `wait` | the read did not get through; the cursor did not move |
| `TransportError::Stalled` inside either | nothing moved on the connection for `efema::STALL` (30 seconds) |
| `TransportError::Refused` with `rate_limited` | the device asked more often than its key allows; `retry` says in how many seconds to ask again |

**The app:** send again when it chooses - at its next start, when the
operating system says the network is back, on a timer. A batch whose answer
was lost goes again byte for byte and lands once; a page read and not
acknowledged is read again. [Syncing an app](/efema/guides/syncing-an-app/#when-the-network-breaks)
has the code.

**The operator:** nothing. A relay that stops halfway through a batch has
written none of it, and one acknowledged is on disk.

## A key changes

### The passphrase was changed on another device

| A device sees | What to do |
| --- | --- |
| `Error::WrongPassphrase` from its next start | ask the person for the new passphrase |
| `Error::PassphraseRace` | two devices changed it at once; the change that came first holds - ask for that one |

A device that opens with the stream's key itself (`Secret::Key`) is not
affected.

### The passphrase is forgotten

A device that has the stream open, or opens it with the key the app kept
(`Secret::Key`), sets a new one - `change_passphrase` needs the key, not the
old passphrase. Without such a device, nobody can open the stream again: start
a new stream, push the app's data into it, and `State::forget` the old one on
every device.

### The passphrase leaked

Changing it does not help: the old lock stays in the stream, which only
grows, and whoever has a copy of the stream and the old passphrase unlocks the
key. Start a new stream under a new passphrase, push the app's data into it,
and `State::forget` the old one on every device. The old stream stays on the
relay - a relay deletes no stream; to drop it, the operator
[starts the relay over](/efema/guides/backups/#starting-over) once every
device has moved.

### An access key is revoked, or replaced

A device whose key the relay no longer takes sees `Error::Unauthorized`; what
it pushed waits. The operator hands out a key to the same space, and the app
opens with it:

```console
$ efema-server keys add home write --label laptop
```

```rust
let relay = efema::Relay::new("https://sync.example.com", new_key)?;
```

The device's state does not change with the key: its cursor, its lock and
what waits to be sent go on as they were. To replace a key without a pause,
hand out the new one first, then revoke the old:
`efema-server keys revoke <ID>`.

### A device is lost

Revoke its access key, so it reaches nothing from now on - one key per device
makes that possible without touching the others. What it holds of the stream
is a lock, which opens with the passphrase. If the app kept the stream's key
on it (`Client::key`) somewhere not protected by the device itself, treat the
key as leaked: a new stream, as above. Through a bucket, change the
credentials the device had.

## The space fills up

| A device sees | What happened | What to do |
| --- | --- | --- |
| `Error::QuotaExceeded` (inside `Unsent`) | the space holds all its quota allows | the operator raises it - `serve --space-quota` - and restarts the relay; what was pushed waits on the device, and goes with the next send |
| `Error::ItemTooLarge` | one item is larger than the relay takes | the app cuts the item into parts, or leaves it out and says so; the operator may raise `serve --max-entry`, up to the request body |
| `Error::Transport` with `internal` | the relay could not write - its disk is full, typically | the operator frees space; a batch is committed whole or not at all, so nothing half-written stays |

A stream only grows: a relay keeps every entry it was given. A stream that has
grown heavier than it is worth starts over light - a new stream, the app's
current state as its first items, the old one forgotten on every device.

Through a bucket there is no quota and nothing to raise: what the bucket holds
is the service's to bill.

## The relay moves

### To another machine

1. Stop the relay, and copy its data directory - or take a
   [backup](/efema/guides/backups/#taking-a-backup) while it runs.
2. Install the relay on the new machine - this release or a newer one
   ([On a server](/efema/guides/on-a-server/)) - and put the copy in its data
   directory.
3. Start it, and point the door at it: the proxy's upstream, or the name the
   devices use. If the address changes, the apps open with the new one.

Devices notice nothing. The streams keep their identities, so cursors hold;
the database keeps the digests of the access keys, so every key handed out
works on the new machine. Batches written to the old relay after the copy was
taken are not on the new one: stop the old relay first, or the devices that
read them are told so, as below.

### Restored from an older copy

| A device sees | What happened |
| --- | --- |
| `Error::CursorAhead` | the relay has fewer entries than the device read |
| `Error::CursorDiverged` | the relay's history at the device's cursor went another way after the restore |

The device writes nothing to it. `State::forget(&stream)` lets it start over:
it joins the stream as the relay has it now and reads from the beginning, and
the app sends what the stream lacks from its own data.
[Backups](/efema/guides/backups/#restoring) has the details.

### To a new, empty relay - or one that lost its data

| A device sees | What happened |
| --- | --- |
| `Error::StreamGone` | the relay has no such stream |
| `Error::StreamReplaced` | the name belongs to a stream started over since |

The first device to `State::forget` and open again creates the stream anew -
with a new key, under the passphrase it is given - and pushes the app's data.
The others forget and join it.

### From a bucket to another bucket

Copy the stream's objects with their metadata - MinIO's `mc mirror` does, and
so does S3's own copy. Then the apps open with the new bucket. The stream
keeps its identity, and the devices read on from their cursors. A copy that
lost an object's metadata is not read as if it had not: a batch whose metadata
does not match its body is refused.

```console
$ mc mirror old/notes-bucket/vault new/notes-bucket/vault
```

### Between a relay and a bucket

No tool moves a stream between them. Start a new stream on the other side, and
push the app's data into it; forget the old one on every device.

## Related

- [Syncing an app](/efema/guides/syncing-an-app/) - every error above, where
  an app meets it.
- [Backups](/efema/guides/backups/) - taking a copy of a relay, and restoring
  one.
- [Threat model](/efema/concepts/threat-model/) - what a relay that misbehaves
  on purpose can do, beside the accidents here.
