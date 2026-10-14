---
title: keys
description: List, hand out and revoke the access keys that open the relay's spaces.
---

```console
$ efema-server keys [--data <DIR>]
$ efema-server keys add <SPACE> <read|write> [--label <TEXT>] [--data <DIR>]
$ efema-server keys revoke <ID> [--data <DIR>]
```

Every request a device makes shows an access key, and the key decides what
the device reaches: one **space** - a set of streams, under names of their own
- to read, or to read and write. A device never names its space; its key does.
Two spaces may each have a stream called `notes`, and neither reaches the
other's.

All three work on a running relay as on a stopped one. A key handed out is
good from the next request; a revoked key opens nothing from its next request
on.

## Hand out a key

```console
$ efema-server keys add home write --label laptop --data ./relay
Key ff3c99f8 reads and writes space home. This is the only time it is shown:
efema_2a619d3c0e80422cc6f162b9b76ed80927cabb6c1e31c26988a82bc610c8f55a
```

The space is made the first time a key is handed out to it. The key is the
only thing on standard output, so a script can take it:

```console
$ export MINI_VAULT_KEY="$(efema-server keys add home write --data ./relay)"
```

The relay keeps only a digest of the key, and shows the key this once: give it
to the device now. A key that is lost is revoked and handed out again.

| Access | What a device with the key can do |
| --- | --- |
| `write` | read the space's streams, wait on them, and append - which creates a stream the first time |
| `read` | read and wait, nothing else: it cannot append, create a stream or raise an epoch |

There is no key that only writes: a device that writes reads too - it finds
the stream's key in the stream.

## List the keys

```console
$ efema-server keys --data ./relay
ID        SPACE  ACCESS  LABEL    CREATED (UTC)     LAST USED (UTC)
ff3c99f8  home   write   laptop   2026-10-14 09:39  2026-10-14 09:39
84190b03  home   write   desktop  2026-10-14 09:39  2026-10-14 09:39
9fb86030  home   read    phone    2026-10-14 09:39  2026-10-14 09:39
```

| Column | Meaning |
| --- | --- |
| `ID` | the first eight hex digits of the key's digest - what `revoke` takes; never the key |
| `SPACE` | the space it opens |
| `ACCESS` | `read` or `write` |
| `LABEL` | the note given with `--label` |
| `CREATED (UTC)` | when it was handed out |
| `LAST USED (UTC)` | when a device last showed it, to the minute; `never` until then |

Listing opens the database read-only, and on a directory with no relay in it
says so instead of creating one.

## Revoke a key

```console
$ efema-server keys revoke 9fb86030 --data ./relay
Revoked key 9fb86030 (phone) to space home.
```

From its next request on, the key is refused with `unauthorized`, and the
device's app says the relay does not take its key:

```console
$ mini-vault --relay http://127.0.0.1:2451 --folder ./phone --state ./phone-state sync --once
error: http://127.0.0.1:2451 does not take the access key: it was revoked, or never handed out there
```

The space and its streams stay, and so do the other keys to it.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `SPACE` | none | the space the key opens: one to 64 lowercase letters, digits, `-`, `_` and `.`, starting with a letter or a digit |
| `read`, `write` | none | what the key may do there |
| `--label <TEXT>` | empty | a note to tell the key by in the list - the device it is for |
| `ID` | none | the key to revoke, as the list shows it |
| `--data <DIR>` | the local data directory | the relay - see [Commands](/efema/reference/commands/#the-data-directory) |

## Related

- [Threat model](/efema/concepts/threat-model/) - what a key protects, and what
  it does not.
- [Protocol](/efema/reference/protocol/#access) - how a key travels.
- [`serve`](/efema/reference/serve/) - the limits each key is held to.
