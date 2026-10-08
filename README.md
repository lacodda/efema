<p align="center"><img src="https://github.com/lacodda/efema/raw/main/assets/banner.svg" alt="efema - a sync relay for local-first apps" width="720"></p>

> Your devices leave their changes for each other in one place: efema keeps them in order, hands each device what it missed, and cannot read any of it.

<p align="center">
  <a href="https://crates.io/crates/efema-server"><img src="https://img.shields.io/crates/v/efema-server?style=flat-square" alt="crates.io"></a>
  <a href="https://docs.rs/efema-proto"><img src="https://img.shields.io/docsrs/efema-proto?style=flat-square&label=docs.rs" alt="docs.rs"></a>
  <a href="https://github.com/lacodda/efema/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/lacodda/efema/ci.yml?branch=main&style=flat-square" alt="CI"></a>
  <a href="https://github.com/lacodda/efema/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/efema-server?style=flat-square" alt="MIT"></a>
</p>

## Why

A local-first app keeps your data on your device and works without a network.
Then you install it on a second device, and the two need somewhere to leave
their changes for each other. The usual answers are a sync service that reads
everything it carries, or a database server that has to understand your data
to store it.

efema is the third answer: a relay that knows nothing about your data. Devices
append batches of opaque entries to a stream; the relay gives them positions,
links them into a hash chain and hands every device what came after its
cursor. It never merges, never decides who is right, and - once the client
library seals entries on the device - cannot read what it stores.

## A day in the life

A relay on a home server. A laptop writes two changes, a phone catches up and
writes one of its own, the laptop reads it - and a photo library, on a newer
format, syncs alongside:

```console
$ efema-server serve --data ./relay
2026-10-08T18:36:09.749999Z  INFO efema-server 0.1.0 listening on http://127.0.0.1:2451, data in ./relay
2026-10-08T18:36:11.688941Z  INFO notes: wrote 2 entries (1200 bytes) at 1..2, epoch 1
2026-10-08T18:36:11.736746Z  INFO notes: read 2 entries after 0
2026-10-08T18:36:11.741075Z  INFO notes: wrote 1 entry (512 bytes) at 3, epoch 1
2026-10-08T18:36:11.743149Z  INFO notes: read 1 entry after 2
2026-10-08T18:36:11.747426Z  INFO photos: wrote 3 entries (147456 bytes) at 1..3, epoch 2
```

What the relay keeps is what its operator can see - positions, epochs, sizes
and links, never contents:

```console
$ efema-server streams --data ./relay
STREAM  ID        EPOCH  ENTRIES       SIZE  UPDATED (UTC)
notes   11c3cd7e  1            3    1.7 KiB  2026-10-08 18:36
photos  6964d409  2            3  144.0 KiB  2026-10-08 18:36

$ efema-server streams notes --data ./relay
SEQ  EPOCH   SIZE  HASH
  1  1      640 B  0d72a730
  2  1      560 B  19cdc65a
  3  1      512 B  235f28b7
```

## What you get

- **One order for everyone.** Batches land whole, on consecutive positions,
  and every device reads the same sequence however many wrote at once.
- **Cursors that cannot lie.** A cursor carries the hash chain's link at its
  position: after a restore from an old backup, a device is told its copy and
  the relay's have parted, instead of silently reading past the gap.
- **Epochs.** When an app changes its format, an old version can no longer mix
  its entries in, and a reader stops before the first entry it cannot read.
- **Waiting, not polling.** One request sleeps until any of the streams it
  watches moves, and carries positions only.
- **A schema that refuses.** Gaps, older epochs and edited entries are refused
  by the database itself, not only by the code above it.

## Install

```console
$ cargo install efema-server
```

Or download the archive for your platform - Linux on x86_64 and ARM64,
Windows, macOS on Apple silicon - from the
[latest release](https://github.com/lacodda/efema/releases/latest).

Then `efema-server serve`, and put it behind a reverse proxy before anything
outside the machine talks to it:
[Getting started](https://lacodda.github.io/efema/getting-started/).

## Documentation

The model, the threat model, every command and the protocol:
[lacodda.github.io/efema](https://lacodda.github.io/efema/).

## Status

0.1.0 is the relay and its wire format, [`efema-proto`](https://docs.rs/efema-proto).
It has no access control and no TLS of its own yet: run it on loopback or a
network you trust. The Rust client library, `efema`, comes in 0.2.0 - with
sealing built in, so there is no way to send an entry unsealed. Changes:
[CHANGELOG](https://github.com/lacodda/efema/blob/main/CHANGELOG.md).

## Contributing

The gate, the layout and how commits are written:
[CONTRIBUTING.md](https://github.com/lacodda/efema/blob/main/CONTRIBUTING.md).

## License

MIT (c) [Kirill Lakhtachev](https://lacodda.com)
