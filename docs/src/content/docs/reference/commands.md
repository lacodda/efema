---
title: Commands
description: What every efema-server command shares - the data directory, the environment, the logs and the exit status.
---

```console
$ efema-server <command> [options]
```

| Command | What it does |
| --- | --- |
| [`serve`](/efema/reference/serve/) | serve the relay over HTTP until stopped |
| [`streams`](/efema/reference/streams/) | list the streams a relay keeps, or the entries of one |

`efema-server --version` prints the version; `--help` after any command lists
its options.

## The data directory

Every command works on one data directory, given with `--data <DIR>` or the
`EFEMA_DATA` environment variable. Without either, it is the platform's local
data directory:

| Platform | Default |
| --- | --- |
| Linux | `~/.local/share/efema` |
| macOS | `~/Library/Application Support/efema` |
| Windows | `%LOCALAPPDATA%\efema` |

The directory holds the database, `relay.sqlite3` (with its `-wal` and `-shm`
files while it is open), and `efema-server.lock`, which a serving relay holds
locked. [Backups](/efema/guides/backups/) covers copying it.

## Environment

| Variable | Means the same as |
| --- | --- |
| `EFEMA_DATA` | `--data` |
| `EFEMA_LISTEN` | `serve --listen` |
| `EFEMA_LOG` | which log lines `serve` prints: `error`, `warn`, `info` (the default), `debug` |

A flag wins over its variable.

## Logs

`serve` logs to standard error, one line per event: a write with its size and
positions, a read with how many entries it returned, and refusals and waits at
`debug`. Entry data never appears in a log, at any level.

## Exit status

`0` when the command did what it was asked - for `serve`, a clean stop on
Ctrl+C or `SIGTERM`. `1` when it could not, with the reason on standard error.
`2` for options that do not parse.
