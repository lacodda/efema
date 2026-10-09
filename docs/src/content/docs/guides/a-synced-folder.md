---
title: A synced folder
description: mini-vault, the example app - a folder of files kept the same on several devices through a relay, merging included.
---

`mini-vault` is a small, complete app on top of the client library: it keeps a
folder of files the same on every device that syncs it. It lives in the
repository, in
[`examples/mini-vault`](https://github.com/lacodda/efema/tree/main/examples/mini-vault),
and is worth reading before writing your own sync - it is where the app's half
of the work is, written out.

## Run it

A relay, and two folders that stand for two devices:

```console
$ efema-server serve --data ./relay
$ export MINI_VAULT_PASSPHRASE='several random words, not these'
$ cargo run -p mini-vault -- --relay http://127.0.0.1:2451 --folder ./laptop --state ./laptop-state sync
$ cargo run -p mini-vault -- --relay http://127.0.0.1:2451 --folder ./desktop --state ./desktop-state sync
```

Each keeps its folder in sync until stopped: it sends what changed in the
folder and applies what other devices sent, woken by the relay as soon as
another device writes. `sync --once` syncs once and exits; `doctor` prints the
client's report.

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
conflict: both versions kept, the other one as notes/plan (conflict 975f1b04).md
$ mini-vault --relay http://127.0.0.1:2451 --folder ./desktop --state ./desktop-state sync --once
applied 2 change(s), sent 0
conflict: both versions kept, the other one as notes/plan (conflict 975f1b04).md
$ ls laptop/notes
2026  plan (conflict 975f1b04).md  plan.md
```

The laptop's edit reached the stream second and was made without seeing the
desktop's, so it keeps the name, and the desktop's version - written by device
`975f1b04` - sits beside it on both devices. Lines from the relay's log of the
same run, timestamps left out - positions and sizes, nothing more:

```console
INFO vault: wrote 1 entry (118 bytes) at 1, epoch 1
INFO vault: wrote 3 entries (48354 bytes) at 2..4, epoch 1
INFO vault: read 4 entries after 0
INFO vault: wrote 1 entry (184 bytes) at 5, epoch 1
INFO vault: read 4 entries after 1
INFO vault: wrote 2 entries (342 bytes) at 6..7, epoch 1
```

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

## What it does not do

It is an example, and stops where an example should: no file larger than one
request (16 MiB less a little), no renames (a rename is a removal and a new
file), no watching the folder for changes (it looks every few seconds, and
whenever the relay says another device wrote).

## Related

- [Syncing an app](/efema/guides/syncing-an-app/) - the library it is built
  on.
