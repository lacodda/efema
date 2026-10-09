---
title: Getting Started
description: Install the efema relay, run it, and look at what it keeps.
---

## Install

With a Rust toolchain:

```console
$ cargo install efema-server
```

Or download the archive for your platform from the
[latest release](https://github.com/lacodda/efema/releases/latest) - Linux on
x86_64 and on 64-bit ARM (a Raspberry Pi), Windows and macOS on Apple silicon -
and put `efema-server` somewhere on your `PATH`.

## Run it

```console
$ efema-server serve
2026-10-08T18:36:09.749999Z  INFO efema-server 0.1.0 listening on http://127.0.0.1:2451, data in /home/you/.local/share/efema
```

The relay listens on the loopback interface, port 2451, and keeps its data in
your local data directory. Both have flags ([`serve`](/efema/reference/serve/)):

```console
$ efema-server serve --listen 127.0.0.1:8080 --data /var/lib/efema
```

Loopback is the default on purpose. The relay does not speak TLS and does not
check who is asking yet - so before anything outside this machine reaches it,
put it [behind a reverse proxy](/efema/guides/behind-a-reverse-proxy/) and read
the [threat model](/efema/concepts/threat-model/).

## Check it

```console
$ curl http://127.0.0.1:2451/health
ok
```

Once devices have written to it, `streams` shows what the relay keeps - names,
positions, epochs and sizes, never contents:

```console
$ efema-server streams
STREAM  ID        EPOCH  ENTRIES       SIZE  UPDATED (UTC)
notes   11c3cd7e  1            3    1.7 KiB  2026-10-08 18:36
photos  6964d409  2            3  144.0 KiB  2026-10-08 18:36
```

## What talks to it

Apps talk to it through the Rust client library, [`efema`](https://docs.rs/efema):
it seals every entry on the device - there is no way to send one unsealed -
keeps the device's cursor and waits for news.
[Syncing an app](/efema/guides/syncing-an-app/) shows how;
[A synced folder](/efema/guides/a-synced-folder/) is a complete example to run
against the relay you just started.

Anything else that speaks [the protocol](/efema/reference/protocol/) - CBOR
over HTTP, four endpoints - is a client too, if it seals the way
[Sealing](/efema/concepts/sealing/) describes.

## Next

- [Streams and cursors](/efema/concepts/streams-and-cursors/) - the model the
  relay keeps.
- [Threat model](/efema/concepts/threat-model/) - what the relay can see, and
  what it cannot.
- [Syncing an app](/efema/guides/syncing-an-app/) - the client library.
- [Commands](/efema/reference/commands/) - the data directory, the logs and
  every flag.
