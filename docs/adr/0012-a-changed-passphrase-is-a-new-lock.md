# ADR 0012: A changed passphrase is a new lock of the same key, sealed by the key

- Status: accepted
- Date: 2026-10-09

## Context

A stream's key is locked under its passphrase and kept in the stream
([ADR 0009](0009-a-stream-key-locked-in-the-stream.md)). People change
passphrases: a first one chosen in a hurry, or one locked with Argon2id
parameters that today's default outgrew. Sealing the stream again under a new
key would mean every entry written again, and the plan says a change of
passphrase must not.

Writing a new lock of the same key into the stream is the obvious move, and it
raises three questions. Who may write one: a lock in the clear is something
anyone who can write to the relay can also write, and a device that took in
a lock nobody could unlock would stop opening with its passphrase. Which one
holds, when two devices change the passphrase at once, or when the relay
writes an old lock into the stream again. And what a device that joins does
when the passphrase it is given unlocks only a later lock.

## Decision

- A **key entry** is a lock followed by the key's own seal over it:
  `lock || seal(key, "efema/key-entry/v1" || lock, generation)`. The seal is a
  `lacodda-seal` sealed blob of four bytes, the lock's **generation** - `0` for
  the lock the stream was created with, one more for each change. Only a
  device that holds the key can write it; a device that holds the key checks
  it before taking the lock in.
- A device changes the passphrase by locking the key under the new one, with
  today's default parameters, and writing a key entry of the next generation.
  It reads the stream to its head first, so the generation follows the
  newest; and it reads up to its own entry afterwards, to know whether a
  change written at the same moment came first.
- Which lock holds is decided by the stream alone: in stream order, a key
  entry of the key holds when its generation is greater than that of the one
  before it. The first of two changes of the same generation holds; an old
  entry written again holds nothing. A key entry of the key whose seal does
  not open stops a reader in front of it, as an altered entry does.
- Devices take in the lock that holds as they read past it, in their state
  too, where the generation only goes up. From its next start a device asks
  for the new passphrase.
- A device that joins tries the passphrase on the first key entry; if it does
  not unlock, it reads the stream and tries the later locks of the same key,
  newest first. It keeps the lock that holds, not merely the one that opened.
  Joining with an old passphrase works - the old lock is in the stream - and
  the device moves to the newer lock as it reads, like every other device.

## Consequences

- Changing a passphrase writes one entry of 172 bytes; nothing else is
  sealed again.
- The relay cannot change a passphrase, slip in a lock, or take a stream back
  to a passphrase it moved on from.
- **A changed passphrase does not revoke the old one.** The stream only
  grows, so the old lock stays in it, and whoever knows the old passphrase
  and holds a copy of the stream can unlock the key. This is for a weak
  passphrase, not a leaked one; a leaked one needs a new stream. The docs say
  so wherever the change is offered. Rotating the key itself - a new key for
  what comes after - is a different change, and not this one.
- A joining device whose passphrase does not unlock the first lock reads the
  stream once to find the later ones, and spends Argon2id on each until one
  opens. A relay can stuff the stream with locks to make that slow; it can
  also withhold the stream outright, so this gives it nothing new.
- Streams written by efema 0.2 have a first lock without the key's seal, and
  are not read by 0.3. There are no such streams outside tests.
