---
title: Threat model
description: What an efema relay can see and do, what it cannot, and what this version does not protect yet.
---

A relay sits between your devices and holds everything they say to each other.
This page is the honest account of what that position allows. Its first
version was written before the code that seals entries, so the promise was on
record before the implementation; since 0.2.0 it says what the code does.

## What the relay sees

Whoever runs the relay - you, a friend, a hosting company - can see:

- **stream names**, which travel in the URL;
- for every entry: its **position**, **size**, **epoch** and **link**;
- **when** each batch was written and each read and wait happened;
- the **network address** of every device that talks to it;
- the stream's **locked key**: its first entry, which anyone can try
  passphrases against (below).

Pick stream names accordingly. `notes` says little; `bank-passwords` says what
it says to anyone who can read the relay's database or its logs.

## What it does not see

**The contents of entries.** The client library seals every entry on the
device, with XChaCha20-Poly1305 under a key that never leaves the devices
unlocked ([Sealing](/efema/concepts/sealing/)). The relay stores ciphertext,
and so does every backup of the relay. The library has no method that sends an
entry unsealed: a client cannot be made without the stream's passphrase or
key, and "without encryption" is not a mode efema has.

**Which device wrote what.** Each device has an identity, and it travels
inside the seal: the relay sees entries, not their authors.

**The key.** Devices keep the key locked under the passphrase; the relay has
only the lock.

## The passphrase is the key's last line

The lock is on the relay - so whoever holds the relay's database can try
passphrases against it, offline, as fast as their hardware runs Argon2id at
64 MiB and three passes. That cost per guess is the whole of the protection: a
short or common passphrase falls to a patient attacker, a long one does not.
Use several random words, not a word and a number.

A device's state file holds the same lock, and the same advice applies to
anyone who copies it.

## What this version does not protect yet

- **No access control.** Anyone who can reach the port can read and write every
  stream. They read ciphertext, and what they write does not open on your
  devices - but they can fill a stream with entries your devices refuse, or
  start a stream under a name you meant to use. Access keys - separate ones for
  reading and writing - arrive in 0.5.0.
- **No TLS.** The relay speaks plain HTTP. It listens on loopback by default;
  anything beyond this machine goes through a reverse proxy that terminates TLS
  ([guide](/efema/guides/behind-a-reverse-proxy/)). The relay will not grow TLS
  of its own: the proxy in front of it already does that well. The client
  speaks HTTPS, and trusts what the platform trusts.
- **Sizes and times.** Sealing hides what an entry says, not how long it is or
  when it was written.
- **No forward secrecy.** Whoever learns the key - or the passphrase and a
  lock - reads the whole stream, past entries included. Changing the passphrase
  without sealing the stream again arrives in 0.3.0; it re-locks the key, it
  does not replace it.

Run 0.2.0 on loopback, or on a network where you would not mind everyone in it
seeing the names, sizes and times of your streams.

## What a dishonest relay can do

Sealing keeps a relay from reading entries. It does not make the relay
trustworthy in every other way. A relay that wants to misbehave can:

- **withhold** entries - from everyone, or from some devices;
- **delay** them, or answer waits late;
- **drop** a stream, or the whole database;
- **show different histories to different devices.** The hash chain catches a
  history that changed under a cursor by accident, but a relay that lies on
  purpose can compute a chain for whatever history it likes.

It cannot read a sealed entry, and it cannot forge or change one: sealing
authenticates as well as encrypts, so an entry the relay made up or altered
fails to open on the device - and the client says which one, instead of
reading past it. It cannot move an entry to another epoch either: the epoch is
sealed in with the entry.

## What the relay protects against

Accidents, mostly - the kind that happen to honest operators:

- **A restore from an old backup**, or a disk that lost recent writes: cursors
  past the relay's head are refused with `cursor_ahead`, and cursors into a
  history that went another way with `cursor_diverged`.
- **A relay that was wiped and started again**: streams get new identities,
  old cursors are refused with `stream_replaced`, and a device that knew the
  old stream is refused when it writes, too - its next batch never becomes
  the first entry of a stream it did not start.
- **Two versions of an app writing different formats**: epochs keep an older
  version from mixing its entries in after a newer one moved the stream on.
- **A crash or a power cut mid-write**: a batch is committed whole, and an
  acknowledged write is on disk before the device hears about it.
- **A relay that hands out a broken history**: the client recomputes every
  link as it reads, and stops at the first one that does not follow.
