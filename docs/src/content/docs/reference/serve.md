---
title: serve
description: Serve the relay over HTTP until stopped.
---

```console
$ efema-server serve [--listen <ADDRESS>] [--data <DIR>]
```

Opens the relay in the data directory - creating the directory and the database
on first use - and serves [the protocol](/efema/reference/protocol/) on the
address until stopped.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--listen <ADDRESS>` | `127.0.0.1:2451` | the address and port to listen on; port `0` lets the system choose |
| `--data <DIR>` | the local data directory | where the database lives - see [Commands](/efema/reference/commands/#the-data-directory) |

`EFEMA_LISTEN` and `EFEMA_DATA` set the same, and `EFEMA_LOG` sets how much it
logs.

The default address is loopback. Reaching the relay from other machines means a
reverse proxy in front of it ([guide](/efema/guides/behind-a-reverse-proxy/)),
not `--listen 0.0.0.0:2451`: the relay has neither TLS nor access control in
this version.

## What it prints

```console
$ efema-server serve --data ./relay
2026-10-08T18:36:09.749999Z  INFO efema-server 0.1.0 listening on http://127.0.0.1:2451, data in ./relay
2026-10-08T18:36:11.688941Z  INFO notes: wrote 2 entries (1200 bytes) at 1..2, epoch 1
2026-10-08T18:36:11.736746Z  INFO notes: read 2 entries after 0
2026-10-08T18:36:11.741075Z  INFO notes: wrote 1 entry (512 bytes) at 3, epoch 1
2026-10-08T18:36:11.743149Z  INFO notes: read 1 entry after 2
```

The first line names the address actually bound - with `--listen
127.0.0.1:0`, it is how to learn the port.

## Stopping

Ctrl+C, or `SIGTERM` from a service manager. The relay stops accepting
connections, lets requests in flight finish and answers every wait at once, so
no device is left hanging for a minute; then it exits with status 0.

## When it refuses to start

- **Another relay serves the directory.** One directory, one serving process:
  `another efema-server is already serving <DIR>`.
- **The address is taken:** `cannot listen on <ADDRESS>`.
- **The database is newer than this release.** A relay never reads a schema it
  does not know; upgrade `efema-server` instead.

## Related

- [`streams`](/efema/reference/streams/) - look at what the relay keeps, while
  it runs.
- [Protocol](/efema/reference/protocol/) - what it answers.
