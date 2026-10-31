---
title: Getting Started
description: Install the efema relay, hand out a key, run it, and look at what it keeps.
---

## Install

On Linux - x86_64 or 64-bit ARM, a Raspberry Pi among them - and on macOS on
Apple silicon:

```console
$ curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/install.sh | sh
```

On Windows, in PowerShell:

```console
> irm https://raw.githubusercontent.com/lacodda/efema/main/tools/install.ps1 | iex
```

Either puts `efema-server` from the latest release in your user's programs
(`~/.local/bin`, `%LOCALAPPDATA%\Programs\efema`); `EFEMA_VERSION=v0.9.0` pins a
release and `EFEMA_INSTALL_DIR` picks the directory. With a Rust toolchain,
`cargo install efema-server` builds it instead, and the archives are on the
[latest release](https://github.com/lacodda/efema/releases/latest).

To run it as a service on a Linux server, see
[On a server](/efema/guides/on-a-server/).

## Hand out a key

Devices reach the relay with access keys. A key opens one **space** - a set of
streams - to read it, or to read and write it:

```console
$ efema-server keys add home write --label laptop
Key ff3c99f8 reads and writes space home. This is the only time it is shown:
efema_2a619d3c0e80422cc6f162b9b76ed80927cabb6c1e31c26988a82bc610c8f55a
```

The relay keeps only a digest of the key: give the key to the device now.
[`keys`](/efema/reference/keys/) lists the keys and revokes them, on a running
relay too.

## Run it

```console
$ efema-server serve
2026-10-23T17:36:12.936616Z  INFO efema-server 0.9.0 listening on http://127.0.0.1:2451, data in /home/you/.local/share/efema
2026-10-23T17:36:12.936914Z  INFO answering this machine and HTTPS forwarded by 127.0.0.0/8, ::1/128; batches up to 16777216 bytes, entries up to 8388608, 8589934592 bytes a space, 600 requests a minute a key
```

The relay listens on the loopback interface, port 2451, and keeps its data in
your local data directory. Both have flags ([`serve`](/efema/reference/serve/)),
and so do the limits it holds each key to:

```console
$ efema-server serve --listen 127.0.0.1:8080 --data /var/lib/efema --space-quota 2GiB
```

The relay does not speak TLS. It answers this machine, and requests that a
reverse proxy took over HTTPS - plain HTTP from anywhere else is refused. To
let other devices reach it, put it
[behind a reverse proxy](/efema/guides/behind-a-reverse-proxy/), and read the
[threat model](/efema/concepts/threat-model/).

## Check it

```console
$ curl http://127.0.0.1:2451/health
ok
```

Once devices have written to it, `streams` shows what the relay keeps - names,
positions, epochs and sizes, never contents:

```console
$ efema-server streams
STREAM      ID        EPOCH  ENTRIES      SIZE  UPDATED (UTC)
home/vault  aec8e1aa  1            7  47.8 KiB  2026-10-14 09:39
```

## What talks to it

Apps talk to it through the Rust client library, [`efema`](https://docs.rs/efema):
it seals every entry on the device - there is no way to send one unsealed -
keeps the device's cursor and waits for news.
[Syncing an app](/efema/guides/syncing-an-app/) shows how;
[A synced folder](/efema/guides/a-synced-folder/) is a complete example to run
against the relay you just started.

Anything else that speaks [the protocol](/efema/reference/protocol/) - CBOR
over HTTP, a handful of endpoints - is a client too, if it seals the way
[Sealing](/efema/concepts/sealing/) describes.

## Next

- [Streams and cursors](/efema/concepts/streams-and-cursors/) - the model the
  relay keeps.
- [Threat model](/efema/concepts/threat-model/) - what the relay can see, and
  what it cannot.
- [Syncing an app](/efema/guides/syncing-an-app/) - the client library.
- [When something goes wrong](/efema/guides/when-something-goes-wrong/) - a
  torn connection, a changed key, a full space, a relay that moves.
- [Commands](/efema/reference/commands/) - the data directory, the logs and
  every flag.
