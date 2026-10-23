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

A relay, an access key to its space `home` for each device, and a folder
synced between a laptop and a desktop by `mini-vault`, the example app. Both
edit `plan.md` before hearing from each other, and both versions survive:

```console
$ efema-server keys add home write --label laptop --data ./relay
Key ff3c99f8 reads and writes space home. This is the only time it is shown:
efema_2a619d3c0e80422cc6f162b9b76ed80927cabb6c1e31c26988a82bc610c8f55a
$ mini-vault --relay http://127.0.0.1:2451 --folder ./laptop --state ./laptop-state sync --once
applied 0 change(s), sent 3
$ mini-vault --relay http://127.0.0.1:2451 --folder ./desktop --state ./desktop-state sync --once
applied 3 change(s), sent 0
$ mini-vault --relay http://127.0.0.1:2451 --folder ./desktop --state ./desktop-state sync --once
applied 0 change(s), sent 1
$ mini-vault --relay http://127.0.0.1:2451 --folder ./laptop --state ./laptop-state sync --once
applied 0 change(s), sent 2
conflict: both versions kept, the other one as notes/plan (conflict 64b8f145).md
$ mini-vault --relay http://127.0.0.1:2451 --folder ./laptop --state ./laptop-state doctor
relay    http://127.0.0.1:2451 - answered in 15 ms
access   reads and writes - 47.8 KiB of 8.0 GiB in the space
stream   vault - stream aec8e1aa, epoch 1, 7 entries
cursor   at 5 - 2 entries to read
queue    empty
key      164324ac6cbe7a06 - locked 2026-10-14 (today), argon2id 64 MiB × 3 × 4
device   985605d0
epoch    1 - written, and read up to
```

What the relay's operator sees is positions, sizes and links - the first entry
is the stream's key, locked under the passphrase; the rest is ciphertext:

```console
$ efema-server streams home/vault --data ./relay
SEQ  EPOCH      SIZE  HASH
  1  1         172 B  78d045f3
  2  1          98 B  b0f7e1a6
  3  1         105 B  f3f50392
  4  1      47.0 KiB  72c6d156
  5  1         152 B  cabe44d5
  6  1         138 B  ab167db7
  7  1         151 B  4e116ce4
```

## What you get

- **Sealed on the device.** A client cannot be made without the stream's
  passphrase or key, and has no way to send an entry unsealed. The relay
  cannot read, forge or alter an entry, nor tell which device wrote it.
- **Safe to leave reachable.** Every request shows an access key to one space
  of the relay - one that reads, or one that reads and writes. Each key is
  held to limits the relay announces, and plain HTTP from another machine is
  refused: the relay answers only through a proxy that takes HTTPS.
- **One order for everyone.** Batches land whole, on consecutive positions,
  and every device reads the same sequence however many wrote at once.
- **Cursors that cannot lie.** After a restore from an old backup, a device is
  told its copy and the relay's have parted - and never writes into a stream
  that is not the one it knew.
- **Epochs.** An app that changed its format moves the stream on; older
  versions stop in front of the first newer entry instead of misreading it.
- **Delivery that survives a broken network.** What a device pushes waits on
  it, sealed, until the relay has it - through a torn connection, a relay
  that is down, the app closing. A batch sent again after a lost answer lands
  once. A device's cursor moves only when the app says it applied what it
  read.
- **Or no server at all.** An S3 bucket - Amazon's, a MinIO of your own, or
  another service that speaks S3 - takes the relay's place: the devices keep
  the stream there themselves, in the same one order, under the same seal.
- **One `doctor`** for every app built on it.

## Install

The relay, on Linux (x86_64 and ARM64) and macOS:

```console
$ curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/install.sh | sh
```

On Windows:

```console
> irm https://raw.githubusercontent.com/lacodda/efema/main/tools/install.ps1 | iex
```

Or `cargo install efema-server`. As a service on a Linux server - its own
user, systemd, upgrades that copy the data aside first - see
[On a server](https://lacodda.github.io/efema/guides/on-a-server/).

The client, in a Rust app:

```console
$ cargo add efema
```

Then [Getting started](https://lacodda.github.io/efema/getting-started/) for
the relay, [Syncing an app](https://lacodda.github.io/efema/guides/syncing-an-app/)
for the client, and [Without a server](https://lacodda.github.io/efema/guides/without-a-server/)
for a bucket in the relay's place.

## Documentation

The model, sealing, the threat model, every command and the protocol:
[lacodda.github.io/efema](https://lacodda.github.io/efema/). The API:
[docs.rs/efema](https://docs.rs/efema).

## Status

0.7.0 is the relay and the Rust client library, which syncs through a relay
or through an S3 bucket with nothing to run: the relay safe to leave behind an
HTTPS proxy, set up as a service with two commands, and through a torn, silent
or slow network a push lands once - in a bucket as on a relay. The wire format
and the bucket's layout may still change before 1.0.
Changes: [CHANGELOG](https://github.com/lacodda/efema/blob/main/CHANGELOG.md).

## Contributing

The gate, the layout and how commits are written:
[CONTRIBUTING.md](https://github.com/lacodda/efema/blob/main/CONTRIBUTING.md).

## License

MIT (c) [Kirill Lakhtachev](https://lacodda.com)
