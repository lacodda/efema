# Changelog

All notable changes to efema are documented here, generated from the commit
history by [git-cliff](https://git-cliff.org).

The protocol, the relay's database and the client's state file are versioned
on their own: the wire format is protocol version 1 (paths under `/v1`), and
each database carries a schema version checked before reading. Any change that
breaks one of them - or the cursor's hash chain, or the sealing formats every
stream is written in - gets a "Breaking Changes" section here, with the way
across or a plain statement that there is none.

`lacodda-seal` and `lacodda-tear` keep versions of their own (lacodda-seal
0.1.0 since efema 0.2.0, lacodda-tear 0.1.0 since efema 0.4.0 and 0.1.1 since 0.6.0); their changes
are listed under the efema release they shipped with.

## [0.6.0] - 2026-10-19

### Features

- Pass bytes slowly, as on a network that keeps moving
- Give up on a connection gone silent, never on a slow one
- Time the first sync of a vault, up and down
- Install the relay with one command, and run it as a service on Linux

### Bug Fixes

- Leave out a file larger than the relay takes, not the whole folder
- Ask git from inside the repository, not by its path

### Documentation

- Hold a request to progress, not to a clock
- Install with one command, and run the relay on a server

### Testing

- Hold a long wait apart from a stall, and bound the stall test


## [0.5.0] - 2026-10-15

### Breaking Changes

- **Reach a relay only with an access key, within its limits**: every request under /v1 carries an access key, and
the client's Relay::new takes one with the address. Relay::open takes
a Config. The relay's commands name a stream as space/name. The
client's Limits is the record the relay announces.

### Features

- Reach a relay only with an access key, within its limits

### Bug Fixes

- Take the entry limit down with a lowered body
- Take rustls 0.23.46 past four advisories

### Testing

- Hold a key's access and the kept limits to the relay's
- Hold plain http to this machine in a unit test


## [0.4.0] - 2026-10-11

### Breaking Changes

- **Know a batch by what it holds, and write it once**: equal batches are one batch: the same bytes written
twice to a stream land once. `Store::append` takes a `Batch`.
- **Keep what is pushed on the device until the relay has it**: push returns `Sent` instead of `Pushed` with
positions; `Error::Interrupted` is replaced by `Error::Unsent`, which
also wraps any reason sending stopped after the items were kept.

### Features

- Add lacodda-tear, a proxy that tears connections for tests
- Know a batch by what it holds, and write it once
- Keep what is pushed on the device until the relay has it

### Bug Fixes

- Send each queued batch at most once per send

### Testing

- Show what an older app left waiting refused until forgotten
- Hold the queue to answers that do not fit and to an outage


## [0.3.0] - 2026-10-09

### Breaking Changes

- **Compress entries and change passphrases without resealing**: key entries carry the key's seal, and entries are
written in format 2; streams written by 0.2 are not read.

### Features

- Compress entries and change passphrases without resealing

### Testing

- Show that the relay keeps and logs nothing readable
- Start an empty relay rather than delete a directory Windows holds


## [0.2.0] - 2026-10-09

### Features

- Let a batch name the stream it is meant for
- Add lacodda-seal, the line's sealing
- Add the efema client library, sealed from its first release
- Add mini-vault, a folder synced through the relay

### Documentation

- Document sealing, the client and the example app

### Testing

- Hold the sealing page and the four crates to the code
- Refuse an entry no client wrote rather than skip it
- Check the sealing page from the client's own tests

### CI

- Publish each crate at its own version, in dependency order


## [0.1.0] - 2026-10-08

### Features

- Define the wire format of protocol version 1
- Keep streams in SQLite and serve them over HTTP

### Documentation

- Add the documentation site, the decisions and the README

### Testing

- Hold the published face of the project to the code

### Build

- Lay out the repository on the line's standard

### CI

- Ask for a crates.io token only when there is something to publish



