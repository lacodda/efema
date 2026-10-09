# Changelog

All notable changes to efema are documented here, generated from the commit
history by [git-cliff](https://git-cliff.org).

The protocol, the relay's database and the client's state file are versioned
on their own: the wire format is protocol version 1 (paths under `/v1`), and
each database carries a schema version checked before reading. Any change that
breaks one of them - or the cursor's hash chain, or the sealing formats every
stream is written in - gets a "Breaking Changes" section here, with the way
across or a plain statement that there is none.

`lacodda-seal` keeps a version of its own (0.1.0 since efema 0.2.0); its
changes are listed under the efema release they shipped with.

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



