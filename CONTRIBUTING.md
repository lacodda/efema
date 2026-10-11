# Contributing to efema

## The gate

```console
$ cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

The same commands CI runs, so a green terminal means a green pull request.
CI also builds the API docs with warnings as errors, builds on the declared
MSRV, and builds the documentation site.

The tests talk to a real relay over a real socket - an in-process one in
`crates/efema-server/tests/relay.rs` and `crates/efema/tests/client.rs`, the
built binary in `tests/binary.rs` - never to a mock. A test that passes against
a mock says the mock works. Where a relay has to misbehave, a transport stands
between the client and the real relay and changes what it said.

The sealing formats are frozen by a test whose values were computed outside
Rust (Python's `cryptography`); a change there is a new format version.

`tests/release_consistency.rs` holds the published face of the project to the
code: one version everywhere, one README, absolute links, every command with
its reference page, every refusal code and every limit in the protocol page as
the code has them.

`rust-version` is a promise to anyone running `cargo install`. A CI job builds
on exactly that version, so the number cannot quietly rot.

## Layout

| Path | What lives there |
| --- | --- |
| `crates/efema-proto/` | The wire format: names, identities, cursors, epochs, the hash chain, the CBOR messages |
| `crates/efema-server/` | The relay: SQLite storage, the HTTP surface, waiting, and the `efema-server` binary |
| `crates/efema-server/tests/` | The relay over a real socket, the binary as an operator runs it, and the published face held to the code |
| `crates/efema/` | The client library: the transport, the device's state and its queue of what waits to be sent, sealing entries, pull and push, `doctor` |
| `crates/efema/tests/` | The client against a real relay - behind a transport that misbehaves on purpose, and behind a proxy that tears connections |
| `crates/lacodda-seal/` | The line's sealing: a key, a sealed blob, a key locked under a passphrase |
| `crates/lacodda-tear/` | The line's test bench for broken networks: a TCP proxy that tears connections on cue |
| `examples/mini-vault/` | An example app: a folder synced between devices, with merging; tested as two processes |
| `assets/` | The mark's master SVGs and the rasters drawn from them |
| `docs/` | The documentation site (Starlight) and `docs/adr/`, the architecture decisions |

The efema crates share one version, set in `[workspace.package]`, and are
released together ([ADR 0003](docs/adr/0003-three-crates.md)). `lacodda-seal`
and `lacodda-tear` carry versions of their own, which move only when they
change ([ADR 0008](docs/adr/0008-lacodda-seal-in-this-workspace.md),
[ADR 0015](docs/adr/0015-lacodda-tear-in-this-workspace.md)); `mini-vault` is
not published.

Two rules the code keeps, and every change has to keep too:

- **The relay does not look inside an entry.** It stores bytes and returns
  them. The client seals every entry; it has no path that sends one unsealed,
  and none may be added. Anything that would need to decode an entry on the relay - to merge,
  to index, to validate - belongs to the app, not here.
- **Merging and conflicts belong to the app.** The relay gives every reader the
  same facts in the same order and has no opinion on which version is right.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/), in English:
`feat:`, `fix:`, `docs:`, `chore:` and the rest. The changelog is generated
from them (`cliff.toml`), so a commit message is the line the release notes
will carry.

## Decisions

An architectural decision gets a record in [`docs/adr/`](docs/adr/README.md):
one file, numbered, newest last. A decision that replaces an earlier one says
so, and the earlier one is marked superseded rather than deleted.

## Dependencies

Updated by hand, at most once a week, in a commit of their own - Dependabot is
off on purpose ([ADR 0002](docs/adr/0002-dependabot-is-off.md)). The audit
workflows fail on a published advisory, and on a dependency whose license is
not on the accepted list.

## Assistant files

Instructions for coding assistants - `CLAUDE.md`, `AGENTS.md`, `.claude/`,
`.cursor/` and the like - stay on the machine they were written for, and
`.gitignore` keeps them out of the repository. What anyone needs to know about
the project, human or agent, is in this file and in [`llms.txt`](llms.txt).

## Conduct and security

Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md). A
vulnerability is reported privately, as [SECURITY.md](SECURITY.md) describes -
never in a public issue.

## License

By contributing, you agree that your contributions are licensed under the
project's [MIT license](LICENSE).
