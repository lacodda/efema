<p align="center"><img src="https://github.com/lacodda/efema/raw/main/assets/banner.svg" alt="efema - a sync relay for local-first apps" width="720"></p>

> Your devices leave their changes for each other in one place: efema keeps them in order, hands each device what it missed, and cannot read any of it.

<p align="center">
  <a href="https://crates.io/crates/efema"><img src="https://img.shields.io/crates/v/efema?style=flat-square" alt="crates.io"></a>
  <a href="https://docs.rs/efema"><img src="https://img.shields.io/docsrs/efema?style=flat-square&label=docs.rs" alt="docs.rs"></a>
  <a href="https://github.com/lacodda/efema/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/lacodda/efema/ci.yml?branch=main&style=flat-square" alt="CI"></a>
  <a href="https://github.com/lacodda/efema/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/efema?style=flat-square" alt="MIT"></a>
</p>

## Why

A local-first app keeps your data on your device and works without a network.
Then you install it on a second device, and the two need somewhere to leave
their changes for each other. The usual answers are a sync service that reads
everything it carries, or a database server that has to understand your data
to store it.

efema is the third answer: a relay that knows nothing about your data, and a
client library that seals every change on the device before it leaves. The
relay keeps sealed entries in one order for everyone and hands each device
what came after its cursor. It never merges, never decides who is right, and
cannot read what it stores.

## A day in the life

A relay, and a folder synced between a laptop and a desktop by `mini-vault`,
the example app. Both edit `plan.md` before hearing from each other, and both
versions survive:

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
$ mini-vault --relay http://127.0.0.1:2451 --folder ./laptop --state ./laptop-state doctor
relay    http://127.0.0.1:2451 - answered in 2 ms
stream   vault - stream 650365e8, epoch 1, 7 entries
cursor   at 7 - up to date
key      c154b5aad4ec9958 - locked 2026-10-09 (today), argon2id 64 MiB × 3 × 4
device   234d67dc
epoch    1 - written, and read up to
```

What the relay's operator sees is positions, sizes and links - the first entry
is the stream's key, locked under the passphrase; the rest is ciphertext:

```console
$ efema-server streams vault --data ./relay
SEQ  EPOCH      SIZE  HASH
  1  1         118 B  552122ad
  2  1         140 B  d7c6acb3
  3  1         131 B  a7d91dae
  4  1      47.0 KiB  f22285cd
  5  1         184 B  9244d195
  6  1         170 B  dd30f6be
  7  1         172 B  4a90e820
```

## What you get

- **Sealed on the device.** A client cannot be made without the stream's
  passphrase or key, and has no way to send an entry unsealed. The relay
  cannot read, forge or alter an entry, nor tell which device wrote it.
- **One order for everyone.** Batches land whole, on consecutive positions,
  and every device reads the same sequence however many wrote at once.
- **Cursors that cannot lie.** After a restore from an old backup, a device is
  told its copy and the relay's have parted - and never writes into a stream
  that is not the one it knew.
- **Epochs.** An app that changed its format moves the stream on; older
  versions stop in front of the first newer entry instead of misreading it.
- **Delivery that survives a crash.** A device's cursor moves only when the app
  says it applied what it read, and survives restarts.
- **One `doctor`** for every app built on it.

## Install

The relay:

```console
$ cargo install efema-server
```

Or download the archive for your platform - Linux on x86_64 and ARM64,
Windows, macOS on Apple silicon - from the
[latest release](https://github.com/lacodda/efema/releases/latest).

The client, in a Rust app:

```console
$ cargo add efema
```

Then [Getting started](https://lacodda.github.io/efema/getting-started/) for
the relay, and [Syncing an app](https://lacodda.github.io/efema/guides/syncing-an-app/)
for the client.

## Documentation

The model, sealing, the threat model, every command and the protocol:
[lacodda.github.io/efema](https://lacodda.github.io/efema/). The API:
[docs.rs/efema](https://docs.rs/efema).

## Status

0.2.0 is the relay, the client library with sealing built in, and
[`lacodda-seal`](https://docs.rs/lacodda-seal), the sealing on its own. The
relay has no access control and no TLS of its own yet: run it on loopback or a
network you trust, behind a proxy. Changes:
[CHANGELOG](https://github.com/lacodda/efema/blob/main/CHANGELOG.md).

## Contributing

The gate, the layout and how commits are written:
[CONTRIBUTING.md](https://github.com/lacodda/efema/blob/main/CONTRIBUTING.md).

## License

MIT (c) [Kirill Lakhtachev](https://lacodda.com)
