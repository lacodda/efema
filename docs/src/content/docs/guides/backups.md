---
title: Backups
description: Back up a running relay safely, and what devices see when a relay is restored from an older copy.
---

Everything a relay keeps is one SQLite database, `relay.sqlite3`, in its data
directory. Back up that file and you have backed up the relay.

## Taking a backup

Do not copy the file while the relay is running: the database is in WAL mode,
and a plain copy can catch it halfway through a commit. Use SQLite's own
backup, which takes a consistent snapshot of a live database:

```console
$ sqlite3 /var/lib/efema/relay.sqlite3 ".backup '/backups/relay-2026-10-08.sqlite3'"
```

Or stop the relay, copy the directory, and start it again.

The backup holds what the relay holds - sealed entries, and each stream's key
locked under its passphrase - so it is as safe to keep elsewhere as the relay
itself, and no safer: whoever has it can try passphrases against the locks
([threat model](/efema/concepts/threat-model/)).

## Restoring

Stop the relay, put the backup in place as `relay.sqlite3` (and remove any
`relay.sqlite3-wal` and `relay.sqlite3-shm` beside it), start the relay.

A backup is older than the relay was. Devices that read past the moment of the
backup now hold cursors the restored relay cannot honour, and they are told so
instead of being served a history that is not theirs:

- a device whose cursor is past the restored head gets `cursor_ahead`, with the
  head the relay has now;
- once other devices write to the restored relay, a device whose cursor points
  into the lost part gets `cursor_diverged`: the relay has *an* entry at that
  position, but not the one the device has.

Either way the device knows the relay lost what it had, and the app decides
what to do - typically, write its own changes back. Devices whose cursors are
older than the backup notice nothing.

## Starting over

To discard a relay entirely, stop it and delete its data directory. Every
stream written afterwards gets a new identity, and devices holding cursors from
before are told `stream_replaced` rather than reading positions that mean
something else now.
