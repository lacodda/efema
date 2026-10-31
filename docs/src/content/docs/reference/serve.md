---
title: serve
description: Serve the relay over HTTP until stopped - who it answers, the limits it holds each key to, and its metrics.
---

```console
$ efema-server serve [--listen <ADDRESS>] [--trust-proxy <ADDRESS>]... [--max-body <SIZE>]
                     [--max-entry <SIZE>] [--space-quota <SIZE>] [--rate <PER_MINUTE>] [--data <DIR>]
```

Opens the relay in the data directory - creating the directory and the database
on first use - and serves [the protocol](/efema/reference/protocol/) on the
address until stopped. Devices reach it with access keys handed out by
[`keys`](/efema/reference/keys/).

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--listen <ADDRESS>` | `127.0.0.1:2451` | the address and port to listen on; port `0` lets the system choose |
| `--trust-proxy <ADDRESS>` | none | an address or network - `10.0.0.2`, `172.18.0.0/16` - to trust as the reverse proxy, besides loopback; repeat for more |
| `--max-body <SIZE>` | `16MiB`, the most | the largest request - a batch - the relay takes |
| `--max-entry <SIZE>` | `8MiB`, or the body if that is less | the largest entry, at most the body |
| `--space-quota <SIZE>` | `8GiB` | the entry data one space may hold, all its streams together |
| `--rate <PER_MINUTE>` | `600` | requests one access key may make in a minute; as many may come at once |
| `--data <DIR>` | the local data directory | where the database lives - see [Commands](/efema/reference/commands/#the-data-directory) |

A size is a number of bytes, or a number and `KiB`, `MiB` or `GiB`: `512KiB`,
`1GiB`. `EFEMA_LISTEN`, `EFEMA_TRUST_PROXY` (comma-separated),
`EFEMA_MAX_BODY`, `EFEMA_MAX_ENTRY`, `EFEMA_SPACE_QUOTA`, `EFEMA_RATE` and
`EFEMA_DATA` set the same, and `EFEMA_LOG` sets how much it logs.

## Who it answers

The relay has no TLS of its own. It answers requests made on its own machine,
and requests a reverse proxy took over HTTPS and forwarded to it
([guide](/efema/guides/behind-a-reverse-proxy/)). Everything else - plain HTTP
from another machine, a proxy that let plain HTTP through or does not say how
a request reached it - is refused with `https_required` before the access key
is looked at, and logged:

```console
INFO refused POST /v1/streams/notes/entries: plain HTTP from 192.168.1.20: the relay answers only through a reverse proxy that takes requests over HTTPS
```

Its own machine is loopback, and so is the proxy, unless it runs in another
container or on another host: name it with `--trust-proxy`. Keep the default
address: the door is for a relay put on every interface by mistake, not a
reason to put it there.

## Limits

The limits hold each access key back: how large one request and one entry may
be, how much its space may hold, how often it may ask. A device learns them
from the relay ([`GET /v1/limits`](/efema/reference/protocol/#limits-get-v1limits))
and cuts what it sends to them; a request over them is refused, and says which
limit it went over. None can be set above the protocol's ceiling.

Lowering a limit on a relay that devices already sync with is safe: what they
had waiting, cut for the old limits, is cut again when the relay refuses it -
and a batch the relay already has is answered as before, whatever the limits
are now. A space at its quota takes nothing more until the quota is raised;
its devices keep what they push.

## What it prints

```console
$ efema-server serve --data ./relay
2026-10-23T17:36:12.936616Z  INFO efema-server 0.9.0 listening on http://127.0.0.1:2451, data in ./relay
2026-10-23T17:36:12.936914Z  INFO answering this machine and HTTPS forwarded by 127.0.0.0/8, ::1/128; batches up to 16777216 bytes, entries up to 8388608, 8589934592 bytes a space, 600 requests a minute a key
2026-10-14T09:39:39.244673Z  INFO home/vault: wrote 1 entry (172 bytes) at 1, epoch 1
2026-10-14T09:39:39.300767Z  INFO home/vault: read 1 entry after 0
2026-10-14T09:39:39.526735Z  INFO home/vault: wrote 3 entries (48287 bytes) at 2..4, epoch 1
```

The first line names the address actually bound - with `--listen
127.0.0.1:0`, it is how to learn the port. A stream is named with its space,
`<space>/<name>`.

## Metrics

```console
$ curl http://127.0.0.1:2451/metrics
# HELP efema_start_time_seconds When the relay started, in seconds since the Unix epoch.
# TYPE efema_start_time_seconds gauge
efema_start_time_seconds 1791970772
# HELP efema_spaces Spaces on the relay.
# TYPE efema_spaces gauge
efema_spaces 1
...
efema_streams{space="home"} 1
efema_entries{space="home"} 7
efema_bytes{space="home"} 48900
efema_keys{space="home"} 3
efema_last_write_seconds{space="home"} 1791970785
efema_last_use_seconds{space="home"} 1791970789
efema_info{version="0.9.0"} 1
```

Counts and times in the text format Prometheus reads: when the relay started,
how many spaces it has, and for each space its streams, entries, bytes and
keys, when one of its streams was last written and when one of its keys was
last used. Never a stream's name, never anything an entry holds.

The metrics answer only the relay's own machine, with no key - through the
proxy they are refused. Read them where the relay runs, or from an agent next
to it.

## Stopping

Ctrl+C, or `SIGTERM` from a service manager. The relay stops accepting
connections, lets requests in flight finish and answers every wait at once, so
no device is left hanging for a minute; then it exits with status 0.

## When it refuses to start

- **Another relay serves the directory.** One directory, one serving process:
  `another efema-server is already serving <DIR>`.
- **The address is taken:** `cannot listen on <ADDRESS>`.
- **A limit is out of range** - a body over 16 MiB, an entry over the body, a
  rate of 0 - and the message says which.
- **The database is newer than this release.** A relay never reads a schema it
  does not know; upgrade `efema-server` instead.

## Related

- [`keys`](/efema/reference/keys/) - the keys devices reach it with.
- [`streams`](/efema/reference/streams/) - look at what the relay keeps, while
  it runs.
- [Protocol](/efema/reference/protocol/) - what it answers.
