---
title: Sealing
description: How the client compresses and seals entries and keeps a stream's key - the keys, the formats byte by byte, changing the passphrase, and what each part protects.
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
writes the lock to the stream as its first entry, a **key entry**. Every other
device reads the first key entry in the stream and unlocks it with the same
passphrase. If two devices create a stream at the same moment, both key
entries land; the first one in the stream is the key, and the other device
adopts it before it seals anything with its own.

A device keeps the **lock**, not the key, in its sync state. It opens the
stream again without the network, and the state file opens nothing on its own.
An app that wants to spare its user the passphrase on every start can ask the
client for the key and keep it where it keeps its most private data - an OS
keyring, its own encrypted store - and open with the key instead.

The default Argon2id parameters are 64 MiB, 3 passes, 4 lanes: a fraction of a
second on a laptop, per unlock, and per guess for anyone guessing.

## Two kinds of entries

Each entry of a stream is one of two kinds, told apart by its first byte - the
first byte of the `lacodda-seal` format it starts with:

| First byte | Kind | What it holds |
| --- | --- | --- |
| `L` (`0x4c`) | a key entry | the stream's key, locked under a passphrase, and the key's seal over the lock |
| `S` (`0x53`) | a sealed entry | one item the app pushed, compressed or not |

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
plaintext = format: u8 (2) || device: 16 bytes || codec: u8 || body
```

- The **epoch** is the one the entry is written in - the same number the relay
  stores next to it in the clear. Because it is in the context, a relay that
  relabels an entry's epoch makes the entry fail to open, instead of making an
  app read it as another format.
- The **format** is the layout of the plaintext. A reader that meets a format
  newer than it knows stops in front of the entry and says so, rather than
  read it as something else.
- The **device** is the writing device's identity, drawn once per device. It is
  inside the seal: the relay cannot tell which device wrote what.
- The **codec** says how the body holds the item:

| Codec | Body |
| --- | --- |
| `0` | the item, as the app pushed it |
| `1` | one [zstd](https://datatracker.ietf.org/doc/html/rfc8878) frame of the item, with the item's size in the frame header |

The stream's name and identity are not in the context, on purpose: a stream
copied to another relay - under its name or another - stays readable.

### Compression

The client compresses an item with zstd before it seals it - ciphertext does
not compress, so after would be too late - and keeps the frame only if it is
shorter than the item. Text shrinks to around a third: notes, JSON, markdown.
What is already compressed, or random, goes as it is, with codec `0`.

A reader expands a frame only if its header gives the item's size, and the
size is at most 16 MiB - what one request of the protocol carries, and so the
longest item any client writes. The frame has to hold exactly that much. A
frame that claims more is refused before anything is set aside for it.

How large an item may be does not depend on compression: an item has to fit in
a request uncompressed, so whether a push is refused never depends on what the
item says.

An entry that is not compressed is <!-- ENTRY_OVERHEAD -->68 bytes longer than
the item in it.

Format `1` was efema 0.2's, before the codec byte. Its number stays taken: no
entry is written in it again, and one that is does not open.

## A key entry

```
key entry = lock || seal(the stream's key, "efema/key-entry/v1" || lock, generation: u32, big-endian)
```

The **lock** is the stream's key locked under a passphrase, below. Behind it,
the key seals its own lock: a sealed blob, as above, of four bytes - the
lock's **generation** - with the lock in the context. Only a device that holds
the key can write that seal, so only such a device can write a key entry that
devices take in. A key entry is <!-- KEY_ENTRY_LEN -->172 bytes.

The generation is `0` for the lock the stream was created with, and one more
for each change of the passphrase.

## A lock

```
lock = 'L' || 1 || memory (u32) || passes (u32) || lanes (u32) || locked at (u64)
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

## Changing the passphrase

The key stays; the lock changes. A device that changes the passphrase locks
the same key under the new one - with today's Argon2id parameters - and
writes it to the stream as a key entry of the next generation. Nothing in the
stream is sealed again.

Which lock holds is decided by the stream alone, the same way on every device:

1. The first key entry in the stream names **the key**. Key entries of another
   key - from a device that lost the race to create the stream - are passed
   over.
2. A key entry of the key whose seal does not open under the key was not
   written by a device that has it. A reader stops in front of it, as in front
   of an altered entry.
3. Read in stream order, a key entry **holds** when its generation is greater
   than that of the one holding before it. Two devices that change the
   passphrase at the same moment write the same generation; the first in the
   stream holds, and the other device is told its change did not. An old key
   entry written into the stream again holds nothing, because its generation
   is not greater.

A device takes in the lock that holds as it reads past it, keeps it in its
state, and asks for the new passphrase from its next start. A device that
joins tries the passphrase on the first key entry, then - if it does not
unlock - on the later ones, the newest first, and keeps the lock that holds.

## What this protects

- **The relay cannot read an item**, nor tell which device wrote it, nor see
  the key.
- **The relay cannot change an item or make one up.** Any changed byte, and
  any entry not sealed under the stream's key, fails to open on the device. It
  can still withhold entries - see the
  [threat model](/efema/concepts/threat-model/).
- **The relay cannot move an item to another epoch.**
- **The relay cannot change the passphrase**, slip in a lock of its own, or
  take a stream back to a passphrase it moved on from: a key entry counts only
  under the key's seal, and only with a newer generation.

And what it does not:

- **The passphrase can be guessed offline** by whoever holds the lock - the
  relay's operator, or anyone with a copy of its database. Argon2id makes each
  guess cost what an unlock costs; nothing else stands in the way. A long
  passphrase - several random words - is the protection.
- **Sizes and times are visible.** The relay sees how long each entry is and
  when it was written. With compression, the length also depends on what is
  in the item - on how much of it repeats. Where one item mixes a secret with
  text someone else chose, and that someone sees the relay, lengths are a
  known way to learn the secret a guess at a time; an app that builds items
  like that turns compression off (`Compression::Off`).
- **Changing the passphrase does not lock anyone out.** Old locks stay in the
  stream, which only grows: whoever knows an old passphrase and holds a copy
  of the stream unlocks the key. A passphrase that leaked needs a new stream.
- **There is no forward secrecy.** Whoever learns the key - or the passphrase
  and a lock - reads the whole stream, past entries included.

## Related

- [Threat model](/efema/concepts/threat-model/) - the relay's position, and
  what sealing does and does not change about it.
- [Syncing an app](/efema/guides/syncing-an-app/) - opening a stream with a
  passphrase or a key.
- [ADR 0009](https://github.com/lacodda/efema/blob/main/docs/adr/0009-a-stream-key-locked-in-the-stream.md) -
  why one random key per stream, kept in the stream.
- [ADR 0011](https://github.com/lacodda/efema/blob/main/docs/adr/0011-compressed-before-sealed.md) -
  why zstd, before sealing, with a codec byte of its own.
- [ADR 0012](https://github.com/lacodda/efema/blob/main/docs/adr/0012-a-changed-passphrase-is-a-new-lock.md) -
  why a new passphrase is a new lock in the stream, and what that does not do.
