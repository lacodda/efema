---
title: Sealing
description: How the client seals entries and keeps a stream's key - the keys, the two formats byte by byte, and what each part protects.
---

Every entry is sealed on the device that writes it, before it leaves. The
relay stores ciphertext, and so does every backup of the relay. This page is
how, in enough detail to read and write the same bytes in another language.

The sealing itself is [`lacodda-seal`](https://docs.rs/lacodda-seal), a crate
of its own: the line's sealing, shared with other apps. What efema adds on top
is small - which key, which context, and what goes inside.

## Keys

A stream has **one key**: 32 random bytes, made by the first device that opens
the stream. Nobody types it. What people type is the stream's **passphrase**.

The first device **locks** the key under the passphrase - Argon2id stretches
the passphrase into a key-encryption key, which seals the stream's key - and
writes the lock to the stream as its first entry. Every other device reads the
first lock in the stream and unlocks it with the same passphrase. If two
devices create a stream at the same moment, both locks land; the first one in
the stream is the key, and the other device adopts it before it seals anything
with its own.

A device keeps the **lock**, not the key, in its sync state. It opens the
stream again without the network, and the state file opens nothing on its own.
An app that wants to spare its user the passphrase on every start can ask the
client for the key and keep it where it keeps its most private data - an OS
keyring, its own encrypted store - and open with the key instead.

The default Argon2id parameters are 64 MiB, 3 passes, 4 lanes: a fraction of a
second on a laptop, per unlock, and per guess for anyone guessing.

## Two kinds of entries

Each entry of a stream is one of two `lacodda-seal` formats, told apart by its
first byte:

| First byte | Kind | What it holds |
| --- | --- | --- |
| `L` (`0x4c`) | a locked key | the stream's key, locked under the passphrase |
| `S` (`0x53`) | a sealed blob | one item the app pushed |

An entry that starts with anything else was not written by an efema client,
and a client refuses it.

## A sealed entry

```
sealed = 'S' || 1 || key id (8) || nonce (24) || ciphertext || tag (16)
```

| Part | Length | Meaning |
| --- | --- | --- |
| kind | 1 | `S` |
| version | 1 | the format's version, `1` |
| key id | 8 | which key sealed it: the first eight bytes of `SHA-256("lacodda-seal/v1/key-id" \|\| key)` |
| nonce | 24 | random, for XChaCha20-Poly1305 |
| ciphertext and tag | the plaintext's length, plus 16 | XChaCha20-Poly1305 |

The associated data - authenticated, not encrypted - is:

```
"lacodda-seal/v1/sealed" || header (the first 34 bytes) || context
```

So every byte of the header is covered: a changed key id, version or nonce
fails to open, like a changed ciphertext. Sealing adds
<!-- SEALED_OVERHEAD -->50 bytes to what it seals.

For an efema entry the context and the plaintext are:

```
context   = "efema/entry/v1" || epoch: u32, big-endian
plaintext = format: u8 (1) || device: 16 bytes || data
```

- The **epoch** is the one the entry is written in - the same number the relay
  stores next to it in the clear. Because it is in the context, a relay that
  relabels an entry's epoch makes the entry fail to open, instead of making an
  app read it as another format.
- The **device** is the writing device's identity, drawn once per device. It is
  inside the seal: the relay cannot tell which device wrote what.
- **data** is the item as the app pushed it.

The stream's name and identity are not in the context, on purpose: a stream
copied to another relay - under its name or another - stays readable.

An entry is <!-- ENTRY_OVERHEAD -->67 bytes longer than the item in it.

## A locked key

```
locked = 'L' || 1 || memory (u32) || passes (u32) || lanes (u32) || locked at (u64)
         || salt (16) || key id (8) || nonce (24) || wrapped key (32) || tag (16)
```

| Part | Length | Meaning |
| --- | --- | --- |
| kind | 1 | `L` |
| version | 1 | the format's version, `1` |
| memory | 4 | Argon2id's memory, in KiB, big-endian |
| passes | 4 | Argon2id's passes |
| lanes | 4 | Argon2id's lanes |
| locked at | 8 | when the lock was made, in seconds since 1970, by the locking device's clock |
| salt | 16 | random, for Argon2id |
| key id | 8 | the identity of the key inside, as above |
| nonce | 24 | random, for XChaCha20-Poly1305 |
| wrapped key and tag | 48 | the key, sealed |

To unlock: Argon2id (version 0x13) over the passphrase and the salt, with the
lock's parameters, gives 32 bytes - the key-encryption key. XChaCha20-Poly1305
under it opens the wrapped key, with the associated data

```
"lacodda-seal/v1/locked" || header (the first 70 bytes) || context
```

and the context `"efema/stream-key/v1"`. A lock is <!-- LOCKED_LEN -->118 bytes.

A reader holds the parameters to bounds before spending anything on them -
from 19 MiB, 2 passes, 1 lane to 1 GiB, 16 passes, 16 lanes - so a planted
lock asking for a terabyte of memory is refused, not attempted.

## What this protects

- **The relay cannot read an item**, nor tell which device wrote it, nor see
  the key.
- **The relay cannot change an item or make one up.** Any changed byte, and
  any entry not sealed under the stream's key, fails to open on the device. It
  can still withhold entries - see the
  [threat model](/efema/concepts/threat-model/).
- **The relay cannot move an item to another epoch.**

And what it does not:

- **The passphrase can be guessed offline** by whoever holds the lock - the
  relay's operator, or anyone with a copy of its database. Argon2id makes each
  guess cost what an unlock costs; nothing else stands in the way. A long
  passphrase - several random words - is the protection.
- **Sizes and times are visible.** The relay sees how long each entry is and
  when it was written.
- **There is no forward secrecy.** Whoever learns the key - or the passphrase
  and a lock - reads the whole stream, past entries included.

## Related

- [Threat model](/efema/concepts/threat-model/) - the relay's position, and
  what sealing does and does not change about it.
- [Syncing an app](/efema/guides/syncing-an-app/) - opening a stream with a
  passphrase or a key.
- [ADR 0009](https://github.com/lacodda/efema/blob/main/docs/adr/0009-a-stream-key-locked-in-the-stream.md) -
  why one random key per stream, kept in the stream.
