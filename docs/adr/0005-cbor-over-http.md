# ADR 0005: CBOR over HTTP/1.1, versioned by path

- Status: accepted
- Date: 2026-10-08

## Context

Entries are opaque bytes - sealed, so incompressible - and they are most of
what crosses the wire: a first sync of a five-thousand-file vault, a phone on a
mobile network. The format of the messages around them freezes at 1.0, and an
S3 transport later stores the same records as objects.

JSON would make every entry base64, a third larger, on every request and in
every stored object. A custom binary framing would be compact and a second
parser to get right in every client language, with versioning done by hand.

## Decision

- Bodies are **CBOR** (RFC 8949): maps with small integer keys, byte strings
  for entries. Decoders ignore unknown keys and treat added fields as optional,
  so the format grows without breaking older peers; required fields are
  forever. A body is exactly one message - trailing bytes are refused.
- The Rust side derives encoders with `minicbor`, whose derive implements
  exactly those compatibility rules.
- **HTTP/1.1**, paths under **`/v1`**. A later major version gets its own
  prefix, so one relay can serve both while clients move.
- Errors are CBOR too, with a stable `code` and a human `message`; an unknown
  code still decodes.
- Query strings use text forms with one spelling each (cursor
  `<seq>.<identity>.<link>`), percent-decoded, and unknown parameters are
  refused rather than ignored.

## Consequences

- No growth on entry data, and one parser per language from a standard every
  ecosystem has.
- Bodies are not readable with `curl` alone. The relay's own listing (`streams`)
  and its logs show what an operator needs, and the protocol page documents
  each message with its keys.
- The health endpoint stays plain text: it is for proxies and people, not
  clients.
