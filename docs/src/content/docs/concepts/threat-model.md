---
title: Threat model
description: What an efema relay can see and do, what it cannot, which of that is shown by tests, and what this version does not protect yet.
---

A relay sits between your devices and holds everything they say to each other.
This page is the honest account of what that position allows. Its first
version was written before the code that seals entries, so the promise was on
record before the implementation. It now says what the code does - and
[which tests show it](#what-is-shown-not-claimed).

## What the relay sees

Whoever runs the relay - you, a friend, a hosting company - can see:

- **stream names**, which travel in the URL;
- for every entry: its **position**, **size**, **epoch** and **link**;
- **when** each batch was written and each read and wait happened;
- the **network address** of every device that talks to it;
- the stream's **key entries**: its key, locked under the passphrase - the
  first entry, and one more each time the passphrase changes - which anyone
  can try passphrases against (below).

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

Changing the passphrase locks the same key again, with today's Argon2id
parameters, and writes the new lock to the stream; every device moves to it as
it reads. It does not take the old lock away - the stream only grows - so
whoever knows the old passphrase and holds a copy of the stream can still
unlock the key. Change a passphrase that was weak; for one that leaked, start a
new stream.

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
  when it was written. Items are compressed before they are sealed, so a
  length also says how much of an item repeats. Where one item mixes a secret
  with text someone else chose, and that someone can see the relay, lengths
  are a known way to learn the secret a guess at a time (the attacks on TLS
  called CRIME and BREACH). An app that builds items like that turns
  compression off.
- **No forward secrecy.** Whoever learns the key - or the passphrase and a
  lock - reads the whole stream, past entries included. A changed passphrase
  re-locks the key; it does not replace it.

Run this version on loopback, or on a network where you would not mind
everyone in it seeing the names, sizes and times of your streams.

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
sealed in with the entry. Nor can it change the passphrase: a new lock counts
only under the key's own seal, and an old one written again counts for
nothing.

It can make joining slow: a device that joins tries the passphrase on the
locks in the stream, and each try costs what Argon2id costs - more for a lock
whose parameters the relay chose at the upper bound. That is one more way to
withhold the stream, not a way into it.

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

## What is shown, not claimed

Each promise on this page is held by a test that runs on every change, against
a real relay over a real socket - never a mock of one. Before every release
each property is also broken on purpose, one at a time, to see its test fail:
a test that stays green with the property gone proves nothing.

| Promise | Shown by |
| --- | --- |
| The relay's database, its write-ahead log and everything logged in the process at every level hold no item, passphrase, key or device identity in the clear - after every way a client writes: a new stream, pushes compressed and not, a changed passphrase, a second device joining and writing. Compression is off for the marker, so what hides it has to be the seal. A device's own state holds none of them either. | `nothing_the_relay_keeps_or_logs_is_plaintext` ([crates/efema/tests/client.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/client.rs)) |
| The relay, run as an operator runs it, logs no entry's bytes - as text, in hex or as numbers - at its most verbose level, refusals included. | `the_relay_never_logs_what_it_keeps` ([crates/efema-server/tests/binary.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/binary.rs)) |
| An altered entry is refused, whether the relay leaves the chain as it was or computes it anew. | `a_relay_cannot_alter_an_entry_unnoticed` ([crates/efema/tests/client.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/client.rs)) |
| An entry no client wrote is refused, not skipped. | `an_entry_no_client_wrote_is_refused_not_skipped` ([crates/efema/tests/client.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/client.rs)) |
| A relabelled epoch does not open. | `an_entry_opens_in_its_epoch_only` ([crates/efema/src/envelope.rs](https://github.com/lacodda/efema/blob/main/crates/efema/src/envelope.rs)) |
| A changed passphrase holds on every device, and a joining device opens with it. | `a_changed_passphrase_holds_on_every_device` ([crates/efema/tests/client.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/client.rs)) |
| A lock the key did not seal is refused, and an old one written again moves no device back. | `a_lock_the_key_did_not_seal_moves_nothing` ([crates/efema/tests/client.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/client.rs)) |
| A compressed frame that claims more than any item may be is refused before it is expanded. | `a_frame_that_lies_about_its_size_is_refused` ([crates/efema/src/envelope.rs](https://github.com/lacodda/efema/blob/main/crates/efema/src/envelope.rs)) |
| The formats are the ones on the [Sealing](/efema/concepts/sealing/) page, frozen against bytes computed outside the code. | `the_formats_are_frozen` ([crates/lacodda-seal/src/lib.rs](https://github.com/lacodda/efema/blob/main/crates/lacodda-seal/src/lib.rs)), `the_sealing_page_gives_the_formats_the_code_writes` ([crates/efema/tests/sealing_page.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/sealing_page.rs)) |

What no test here shows: that an acknowledged write survives a power cut
(SQLite's `synchronous=FULL` is set; showing it takes pulling a plug), and
what a relay does with a connection torn in the middle of a batch - the
transaction keeps a batch whole, and tests against torn connections come
with offline queues in 0.4.0.
