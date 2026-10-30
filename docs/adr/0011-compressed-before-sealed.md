# ADR 0011: Items are compressed before they are sealed, with a codec byte of their own

- Status: accepted; amended by [ADR 0021](0021-the-format-is-a-contract-of-versioned-parts.md) -
  `NewerEntryFormat` and `NewerCodec` are one `NewerFormat` naming the part, and
  entry format 1 is no longer read
- Date: 2026-10-09

## Context

The first consumers sync text - notes, markdown, JSON - which zstd shrinks to
around a third. Every byte an entry carries goes over the network to the
relay, sits in its database and its backups, and comes back to every other
device; from 0.7 it sits in a bucket someone pays for. Ciphertext does not
compress, so the relay cannot do it, and should not want to: it does not know
what entries hold. Only the client can, and only before it seals.

Compression before encryption has a known cost. An entry's length then
depends on how much of the item repeats, and the relay sees lengths. Where one
item mixes a secret with text an attacker chose, and the attacker sees the
lengths, the secret can be learned a guess at a time - the attacks on TLS
called CRIME and BREACH.

## Decision

- The sealed plaintext gets a new layout, **entry format 2**:
  `2 || device (16) || codec: u8 || body`. Codec `0`: the body is the item.
  Codec `1`: the body is one zstd frame of the item, with the item's size in
  the frame header.
- The codec is a byte of its own, not a new format number per method: the
  format is the layout, the codec how the body is packed. A zstd dictionary
  later is codec `2`, not format `3`. A reader that meets an unknown codec
  stops in front of the entry with `NewerCodec` - the app needs updating - as
  it does for an unknown format.
- The writer compresses at zstd's default level, 3, and keeps the frame only
  if it is shorter than the item. No threshold to tune: a short item does not
  shrink, and goes with codec `0`.
- The reader expands a frame only if the frame header gives the item's size,
  and the size is at most 16 MiB - one request's body, the longest item any
  client writes - and only into exactly that much. A frame that claims more
  is refused before anything is allocated for it. Only a device holding the
  key can write an entry that opens, so this guards against a broken device,
  not the relay; it is cheap enough to have anyway.
- How large an item may be does not change: it has to fit in a request
  uncompressed. Whether a push is refused never depends on what the item
  says.
- Compression is on by default and can be turned off per client
  (`Compression::Off`), for apps that put secrets and someone else's text in
  one item. The threat model names the reason.
- Format `1`, written by 0.2, is still read, and no longer written.
- The `zstd` crate, a binding to the reference C library, rather than a pure
  Rust port: every consumer builds with a C toolchain already (Tauri, CLI
  tools on the same machines), and the reference implementation is the one
  every other zstd is measured against. The frame format is the standard one,
  so another language reads entries with its own zstd.

## Consequences

- Text costs a third of what it did on the wire, on the relay and in backups.
- An entry that does not compress costs 68 bytes over its item, one more than
  in 0.2 - the codec byte.
- An efema 0.2 client stops in front of the first entry a 0.3 client writes,
  with `NewerEntryFormat`, rather than misread it.
- Lengths say a little more than before about what is inside; the threat
  model says how much, and when to turn compression off.
- The frames in the tests are written by hand from RFC 8878, not by the
  library, so the reader is held to the format rather than to whatever the
  library happens to write.
