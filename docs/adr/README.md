# Architecture decisions

One file per decision, numbered, newest last. A decision that replaces an
earlier one says so; the earlier one is marked superseded rather than deleted.

| # | Decision | Status |
| --- | --- | --- |
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-dependabot-is-off.md) | Dependabot is off on purpose | Accepted |
| [0003](0003-three-crates.md) | Three crates - the wire format, the relay, the client | Accepted |
| [0004](0004-positions-a-chain-and-an-identity.md) | Positions, a hash chain and an identity per stream | Accepted |
| [0005](0005-cbor-over-http.md) | CBOR over HTTP/1.1, versioned by path | Accepted |
| [0006](0006-sqlite-with-invariants-in-the-schema.md) | SQLite, with the invariants in the schema | Accepted |
| [0007](0007-waiting-is-a-long-poll.md) | Waiting is a long poll that carries positions only | Accepted |
| [0008](0008-lacodda-seal-in-this-workspace.md) | lacodda-seal lives here, with a version of its own | Accepted |
| [0009](0009-a-stream-key-locked-in-the-stream.md) | One random key per stream, locked under the passphrase, kept in the stream | Accepted |
| [0010](0010-the-client-library.md) | The client - async, at least once, and a writer names its stream | Accepted |
| [0011](0011-compressed-before-sealed.md) | Items are compressed before they are sealed, with a codec byte of their own | Accepted |
| [0012](0012-a-changed-passphrase-is-a-new-lock.md) | A changed passphrase is a new lock of the same key, sealed by the key | Accepted |
