# ADR 0008: lacodda-seal lives here, with a version of its own

- Status: accepted
- Date: 2026-10-09

## Context

The client seals every entry (the 2026-09-18 decision: no unsealed path), and
sefy - the line's password manager - seals its vault with the same primitives:
Argon2id to stretch a passphrase, XChaCha20-Poly1305 to seal. Two copies of
that code would drift apart, and in a sealing format a quiet difference is a
vulnerability. The line's rule is a shared crate once a second consumer is
visible, and sefy is that consumer.

The crate could live in a repository of its own or in this workspace. sefy had
no stage for it when efema needed it.

## Decision

`lacodda-seal` is a crate of this workspace, `crates/lacodda-seal`, published
to crates.io from here - the way `furca-core` lives in furca's repository and
rigger takes it from crates.io.

It carries **its own version**, not the workspace's. The formats it writes are
a contract that outlives any one efema release: a crate that did not change
must not make sefy move its dependency because efema shipped. Its version
moves when its API or its formats do.

## Consequences

- A repository of its own would need its own mark, documentation site and CI
  for some five hundred lines; this one has all three already. If the crate
  ever outgrows efema, moving it is a new trusted publisher on crates.io, not
  a change for its users.
- `publish.yml` publishes each crate at its own version and skips versions
  crates.io already has - an efema release with no change to the crate
  publishes nothing for it.
- The formats are documented on efema's site
  ([Sealing](https://lacodda.github.io/efema/concepts/sealing/)) and frozen by
  a test whose values were computed outside Rust.
- sefy's vault format (v1) is not touched: sefy moves to the crate when it
  writes a new format, or for its efema plugin.
