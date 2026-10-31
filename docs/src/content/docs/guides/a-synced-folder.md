---
title: A synced folder
description: mini-vault, the example app - a folder of files kept the same on several devices through a relay or a bucket, merging included.
---

`mini-vault` is a small, complete app on top of the client library: it keeps a
folder of files the same on every device that syncs it. It lives in the
repository, in
[`examples/mini-vault`](https://github.com/lacodda/efema/tree/main/examples/mini-vault),
and is worth reading before writing your own sync - it is where the app's half
of the work is, written out.

## Run it

A relay, a key to one of its spaces, and two folders that stand for two
devices:

```console
$ export MINI_VAULT_KEY="$(efema-server keys add home write --data ./relay)"
$ efema-server serve --data ./relay
$ export MINI_VAULT_PASSPHRASE='several random words, not these'
$ cargo run -p mini-vault -- --relay http://127.0.0.1:2451 --folder ./laptop --state ./laptop-state sync
$ cargo run -p mini-vault -- --relay http://127.0.0.1:2451 --folder ./desktop --state ./desktop-state sync
```

The key goes in `MINI_VAULT_KEY` (or `--key`), the passphrase in
`MINI_VAULT_PASSPHRASE`. Two devices may share a key or have one each - a key
each lets one be revoked without the other.

Through an S3 bucket instead of a relay, `--bucket` and `--region` take the
place of `--relay` - with `--prefix` for a stream under a prefix of the
bucket - and the credentials come from `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY`. The same run through a bucket is on
[Without a server](/efema/guides/without-a-server/#a-folder-through-a-bucket).

Each keeps its folder in sync until stopped: it sends what changed in the
folder and applies what other devices sent, woken by the relay as soon as
another device writes. `sync --once` syncs once and exits; `doctor` prints the
client's report; `copy` copies the folder's stream to another relay or a
bucket, where each device goes on with the state it has -
[Moving a stream](/efema/guides/moving-a-stream/#with-the-example-app) shows
it.

Here the laptop syncs a folder first, the desktop picks it up, and then both
edit `notes/plan.md` before hearing from each other - the desktop sends first:

```console
$ mini-vault --relay http://127.0.0.1:2451 --folder ./laptop --state ./laptop-state sync --once
applied 0 change(s), sent 3
$ mini-vault --relay http://127.0.0.1:2451 --folder ./desktop --state ./desktop-state sync --once
applied 3 change(s), sent 0
$ mini-vault --relay http://127.0.0.1:2451 --folder ./desktop --state ./desktop-state sync --once
applied 0 change(s), sent 1
$ mini-vault --relay http://127.0.0.1:2451 --folder ./laptop --state ./laptop-state sync --once
applied 0 change(s), sent 2
conflict: both versions kept, the other one as notes/plan (conflict 64b8f145).md
$ mini-vault --relay http://127.0.0.1:2451 --folder ./desktop --state ./desktop-state sync --once
applied 2 change(s), sent 0
conflict: both versions kept, the other one as notes/plan (conflict 64b8f145).md
$ ls laptop/notes
2026  plan (conflict 64b8f145).md  plan.md
```

The laptop's edit reached the stream second and was made without seeing the
desktop's, so it keeps the name, and the desktop's version - written by device
`64b8f145` - sits beside it on both devices. Lines from the relay's log of the
same run, timestamps left out - the stream with its space, positions and
sizes, nothing more:

```console
INFO home/vault: wrote 1 entry (172 bytes) at 1, epoch 1
INFO home/vault: wrote 3 entries (48287 bytes) at 2..4, epoch 1
INFO home/vault: read 4 entries after 0
INFO home/vault: wrote 1 entry (152 bytes) at 5, epoch 1
INFO home/vault: read 4 entries after 1
INFO home/vault: wrote 2 entries (289 bytes) at 6..7, epoch 1
```

A third device with a key that only reads gets the folder, and nothing it
changes goes anywhere: the relay will not take it, and the client does not
keep it to send.

## What the app does

efema moves sealed items in one order for everyone. Everything else is the
app's, and mini-vault does it the way a real app has to:

- **An item is a change to one file**: its path, its new content (or none, for
  a removal), and the version it replaces - its *base*. Encoded as CBOR.
- **An index says what the stream holds for each file.** A file that differs
  from it was changed here. A change sent but not yet read back is remembered
  as on its way, so it is neither sent twice nor undone by an older version
  arriving first.
- **Its own changes are applied too**, in their place in the order. Two devices
  can change one file before either hears from the other; only the stream's
  order says which came last.
- **Paths from other devices are checked** before anything is written: no
  `..`, nothing absolute, nothing a filesystem reads as special.
- **Files are written whole** - to a temporary name, then renamed - so a crash
  never leaves half a file.
- **The cursor moves after the folder and the index do**, so a crash in
  between applies the same changes again, harmlessly.
- **A change the client kept is on its way**, sent or not: when sending stops
  partway - `Error::Unsent` - the changes are already in the client's queue,
  so the index counts them as sent, and the next scan does not send them a
  second time. Each sync starts by sending what an earlier one left waiting.
- **A file larger than the relay takes is left out, and named** - `not sent:`
  with its size and the limit, once for each version of it - while the rest
  of the folder goes. One push carries every change; a single database or
  video in the folder would otherwise hold back every note beside it. The
  limit is the relay's (`Client::max_item_len`, 8 MiB less a little by
  default), so a file that fits one relay may not fit another.
- **The network comes and goes**: a sync that stops is reported and tried
  again a few seconds later, instead of ending the app.

## Merging

When two devices change one file without seeing each other's change, both
versions are kept. The rule depends on the stream alone, so every device makes
the same decision and the folders end up the same:

- A change whose base is not the file's current version in the stream was made
  without seeing that version.
- The change wins - it is later in the order - and the version it overlooked
  is kept beside it as `name (conflict <device>).ext`, named after the device
  that wrote it.
- An edit wins over a removal it did not see: the file comes back.

The copy is an ordinary new file from then on, sent like any other. Every
device that had the overlooked version makes the same copy, and the copies
meet in the stream as one file.

## How long the first sync takes

The first sync of a vault is the heaviest a relay sees: every file at once,
to an empty stream. `first-sync` times it - the whole vault up to the relay,
then down to a second device's empty folder - and checks the two folders hold
the same bytes:

```console
$ cargo run --release -p mini-vault --example first-sync -- --limit 30
vault  5080 files, 101.6 MiB (made up, seed 1)
relay  http://127.0.0.1:5612, stream first-sync-7797a70c
up     5080 files in 6.5 s - the stream's key made in 0.5 s
down   5080 files in 11.6 s - the key unlocked in 0.2 s
check  both folders hold the same bytes
limit  30 s up: within
```

The vault is made up, shaped like a working one: 5080 notes and attachments,
half of them under 4 KB, one in a hundred over 240 KB, the largest a few MB.
`--folder` syncs a folder of your own instead - it is only read - and
`--relay` with `FIRST_SYNC_KEY` goes through a relay of yours rather than one
started for the run; `--bucket` with `--region` and the bucket's credentials
in `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` goes through an S3 bucket.
Each run seals with a passphrase of its own that is never shown, so what it
leaves on a relay or in a bucket can be opened by no one.

Through a bucket - a MinIO on the same machine - the same vault takes about as
long:

```console
$ cargo run --release -p mini-vault --example first-sync -- --bucket http://127.0.0.1:29000/efema-test --region us-east-1 --limit 30
vault  5080 files, 101.6 MiB (made up, seed 1)
bucket http://127.0.0.1:29000/efema-test, stream first-sync-a212bb50
up     5080 files in 6.7 s - the stream's key made in 0.4 s
down   5080 files in 12.7 s - the key unlocked in 0.1 s
check  both folders hold the same bytes
limit  30 s up: within
```

The way up is the library's and the relay's - reading, compressing, sealing,
keeping and sending every file, and the relay writing it - and `--limit`
holds it: CI runs it on Linux and Windows on every change, and through a
MinIO bucket on Linux, and fails over 30 seconds. The way down is mostly the app's: reading the stream takes under a
second, and writing thousands of new files is the operating system's slowest
part of it.

## What it does not do

It is an example, and stops where an example should: no file larger than one
entry - such a file is left out, not cut into pieces - no renames (a rename is
a removal and a new file), no watching the folder for changes (it looks every
few seconds, and whenever the relay says another device wrote - a bucket says
nothing, and is looked at every few seconds).

## Related

- [Syncing an app](/efema/guides/syncing-an-app/) - the library it is built
  on.
