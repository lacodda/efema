# ADR 0003: Three crates - the wire format, the relay, the client

- Status: accepted
- Date: 2026-10-08

## Context

efema is two programs that must agree: a relay that keeps streams, and a
client library that apps link to talk to it. They agree on one thing - the
protocol - and each has dependencies the other must not carry: the relay a
web server and SQLite, the client an HTTP client and the sealing code. The
repository was started from the line's `workspace` form, which assumes one
library and one command-line tool over it; that shape does not fit.

## Decision

Three crates, one version, published together:

- **`efema-proto`** - the wire format: names, identities, cursors, epochs, the
  hash chain and every message with its CBOR encoding. No I/O. Both other
  crates depend on it, so there is one definition of each message and the two
  sides cannot drift apart.
- **`efema-server`** - the relay, as the `efema-server` binary and as a library
  for embedding and tests.
- **`efema`** - the client library, from 0.2.0. The short name goes to the
  crate most people will depend on.

The client crate is not published before it seals. A 0.1.0 `efema` that sent
entries as they are would be exactly the unsealed path the product refuses to
have (the 2026-09-18 decision: the key is a required argument of the transport,
not an option of it), so the name waits for 0.2.0 rather than being taken by a
placeholder.

Publishing is ordered: `efema-proto` first, then a wait for the index, then
the crates that depend on it.

## Consequences

- The protocol is a crate of its own, and its version is the format's: a change
  there is visibly a change to the contract that freezes at 1.0.
- `efema-server`'s library API exists for the binary and for tests. It follows
  the version but is not a stable interface before 1.0.
- Every crate needs a trusted publisher on crates.io, and the first version of
  each is published by hand.
