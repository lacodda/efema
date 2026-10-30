---
title: Format
description: What efema promises about the bytes it writes - the five parts of the format, the version each is at and where it says so, what may change within a version, and the vectors a client in another language checks itself against.
---

Everything a device writes, other devices read: other releases of efema, and
clients written in other languages. That makes the format a contract. This
page names its parts, the version each is at and where it says so, and what
may change. The pages linked from each part give the bytes.

## The parts

| Part | Version | Where it says so | Bytes |
| --- | --- | --- | --- |
| The protocol: endpoints, messages, refusals, cursors, the chain, the batch key, the access key's digest | <!-- PROTOCOL -->1 | the path, `/v1`; the domains `efema/chain/v1/genesis`, `efema/batch/v1` and `efema/access/v1` | [Protocol](/efema/reference/protocol/) |
| A sealed blob and a lock, of `lacodda-seal` | <!-- SEAL -->1 | the second byte of each | [Sealing](/efema/concepts/sealing/#a-sealed-entry) |
| An entry: what a sealed entry holds | <!-- ENTRY -->2 | the first byte inside the seal; the codec, the third part of it, has numbers of its own | [Sealing](/efema/concepts/sealing/#a-sealed-entry) |
| A key entry: the stream's key, locked, and the key's seal over the lock | <!-- KEY_ENTRY -->1 | the domains `efema/stream-key/v1` and `efema/key-entry/v1` | [Sealing](/efema/concepts/sealing/#a-key-entry) |
| A stream in a bucket | <!-- LAYOUT -->1 | the stream's record, `format` | [A stream in a bucket](/efema/reference/bucket/) |

Entry format `1` was efema 0.2's. Its number stays taken: an entry in it does
not open.

## What may change within a version

Only what a reader of the same version passes over:

- **a field in a message**, under a key the message has not used. A reader
  ignores keys it does not know; a field that is there stays for the life of
  the version.
- **a refusal**, under a new `code`. A client keeps codes it does not know.
- **a codec**, under a new number. A reader that meets one it does not know
  stops in front of the entry, as below.

Anything else is the next version of its part: a byte of a formula - the
chain, the batch key, the digest, a key's identity, any associated data; a
field's meaning, type or key, or a field made required; a layout - a header,
a lock, what an entry holds, a key in a bucket.

## A version a reader does not know

A reader stops in front of it and says which part and which version, rather
than read it as anything else. Everything before it is read; the cursor stays
in front of it.

| Part | What the efema client says |
| --- | --- |
| an entry, its codec, a sealed blob, a lock | `Error::NewerFormat`, with the part, the version and the entry's position |
| a stream in a bucket | `Error::NewerFormat`, with `Part::Layout` and no position: the layout is the whole stream's |
| the protocol | a relay that speaks only a newer one answers `/v1` with `not_found` |

A version lower than any a reader knows is not one: an entry, a blob or a
lock in one does not open.

## Until 1.0, and after

From efema 0.8, a change to any part is a new version of that part, named in
the changelog of the release that makes it. Until 1.0 a part may still move:
1.0 waits for two apps to have lived on these formats. From 1.0, no part
moves without efema 2.

## The vectors

[`format-vectors.txt`](/efema/format-vectors.txt) has every part, case by
case: what goes in - keys, nonces, salts, identities, fixed where the format
draws them at random - and the bytes that come out. A client in another
language that reproduces every case speaks the format.

The file is written by
[`tools/format-vectors.py`](https://github.com/lacodda/efema/blob/main/tools/format-vectors.py),
a second implementation of the format, in Python, apart from the Rust code.
The tests of the efema crate read every case back, and check that the code
writes the same bytes where they do not depend on chance; CI runs the script
again on every change and fails if the file is not what it writes. Two
implementations agree on every byte, or nothing is released.

```text
[link]
# previous: the link before. seq, epoch: numbers. data: the entry. link: its link.

previous = 78e6c7beebfabda54547131dd021d12e587a91f113db763dc44cc0170ae18482
seq = 1
epoch = 1
data = 68656c6c6f
link = 1338b06d59cf4930e7d0d151251285b1f1b2f824f29dff4b036d7f38f0c42a20
```

A section is one part; its comment names the fields. A blank line ends a
case, and a field given twice is a list. Bytes are lowercase hexadecimal,
numbers decimal, and a field the comment calls text is the text itself.

## What is not part of it

- **A device's state file** - the library's own, in the app's directory. Each
  release carries an older one forward; a newer one is refused
  (`Error::StateNewer`).
- **A relay's database** - the operator's. A new release of the relay
  upgrades it in place; [Backups](/efema/guides/backups/) says how to keep a
  copy first.
- **What an item means** - the app's. An app says which version of its own
  format it writes with the stream's
  [epoch](/efema/concepts/streams-and-cursors/#epochs).

## Related

- [Protocol](/efema/reference/protocol/), [Sealing](/efema/concepts/sealing/)
  and [A stream in a bucket](/efema/reference/bucket/) - the bytes of each
  part.
- [ADR 0021](https://github.com/lacodda/efema/blob/main/docs/adr/0021-the-format-is-a-contract-of-versioned-parts.md) -
  why the parts carry versions of their own, and why the vectors come from a
  second implementation.
