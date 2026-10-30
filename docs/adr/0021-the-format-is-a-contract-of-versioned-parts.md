# ADR 0021: The format is a contract of parts, each with a version, held by vectors two implementations write

- Status: accepted
- Date: 2026-10-30

## Context

Everything a device writes is read by other devices: other releases of this
library, and one day clients in other languages. Up to 0.7 the format grew a
part at a time - the protocol (ADR 0005), the chain (ADR 0004), sealing
(ADR 0008, 0009), compression (ADR 0011), changed passphrases (ADR 0012), the
bucket (ADR 0020) - and each part was frozen on its own, by a test with values
computed once outside Rust. Four things were missing for the whole to be a
contract:

- **No page named the parts together**, nor said which of them carries its
  version where, nor what may change inside a version and what may not.
- **A reader did not always say which part was newer.** A seal of a newer
  version reached the app as `NewerEntryFormat { version: 2 }` - "entry format
  2, newer than this library reads", while 2 is the entry format it reads. A
  newer lock reached it as a sealing error, and a stream a newer release laid
  out in a bucket as `NotProtocol`, whose advice is to try again later.
- **The frozen values were checked once.** A Python script computed them when
  each test was written; nothing ran it again, and a client in another
  language had no cases to check itself against.
- **The 0.2 entry format was still read.** Nobody has a stream of it, and after
  1.0 dropping it would take a major version.

The plan said v0.8 freezes the format. But no app has lived on it yet: scheda
and sefy, the two that 1.0 waits for, connect after this release.

## Decision

- **The format is five parts, and each carries its version in its own
  bytes.** The protocol - `/v1` in the path, and the `v1` in the domains of
  the chain, the batch key and the access key's digest; `lacodda-seal`'s
  sealed blob and lock - their second byte; an entry - the first byte inside
  its seal, `2`, with a codec byte of its own; a key entry - the domains
  `efema/stream-key/v1` and `efema/key-entry/v1`; a stream in a bucket - its
  record's `format`, `1`. The reference page **Format** names them, the
  version each is at, where each says so, and the rules below.
- **Inside a version, only what an older reader passes over may be added:** a
  field under a key a message has not used, a refusal code, a codec. Any other
  change - a byte of a formula, a field's meaning, type or key, a layout - is
  the next version of that part.
- **A reader stops in front of a version it does not read, and names it.** One
  error, `Error::NewerFormat { stream, seq, part, version }`, with
  `Part::{Layout, Seal, Lock, Entry, Codec}`; `seq` is `None` for the layout,
  which is the whole stream's. It replaces `NewerEntryFormat` and
  `NewerCodec`. A bucket transport says `TransportError::NewerFormat`, which
  becomes the same error.
- **Entry format 1 is no longer read, and its number stays taken.** Format 2
  is not renumbered to 1: a 0.3-0.7 client would read a new "format 1" entry
  as 0.2's layout, its codec byte as the first byte of the item - a silent
  misreading between releases.
- **The vectors are written by a second implementation.**
  `tools/format-vectors.py` implements every part from the pages - hashlib,
  the `cryptography` package for Argon2id and ChaCha20-Poly1305, HChaCha20,
  CBOR and the zstd frames written out by hand - and writes
  `docs/public/format-vectors.txt`, served next to the pages. The `vectors`
  module of the efema crate reads every case back and checks what the code
  reads, and what it writes where the bytes do not depend on chance; a
  section the module does not know fails. CI runs the script with `--check`
  on every change. The file is plain `name = value` lines in sections - read
  in twenty lines in any language, with no parser to depend on.
- **From 0.8 any change to a part is a new version of it; from 1.0 none is
  made without efema 2.** The step between is the two apps living on the
  format before it is closed for good.

## Consequences

- A client in another language is written from the pages and checked against
  the vectors, case by case; agreeing on every case is speaking the format.
- Two implementations agree on every byte on every change, so a page and the
  code cannot drift apart unnoticed, and neither can the frozen values.
- An app has one error to handle for "update to read on", and its message
  names the right part.
- Each part's version can move independently: a new codec does not touch the
  seal, a new layout does not touch the protocol.
- The repository carries a Python script, and CI a Python step. The script
  needs `cryptography` 44 or later and nothing else.

## Rejected

- **One version number for the whole format.** Nothing on the wire would
  carry it; it would be a second truth beside the five that are there.
- **Protecting against a relay that writes an old entry again** with a
  per-device counter inside the seal, checked by every reader. A device
  restored from a backup, or an app folder copied to a new machine, would
  write counters it had used, and every other device would refuse its writes;
  avoiding that means reading the stream to its head before every push, which
  ends pushing offline. Merging stays the app's (the relay knows no domain): a
  merge that is right when two devices change one thing at once is right when
  an old change comes again. The threat model says so.
- **JSON vectors.** Readable everywhere, but a parser - a new dependency for
  the tests here, and a line of code more in every other language - for a
  file that is a list of names and values.
- **Freezing at 0.8.** No app has written a stream yet; a freeze before one
  has is a bet.
