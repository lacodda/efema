# Changelog

All notable changes to efema are documented here, generated from the commit
history by [git-cliff](https://git-cliff.org).

The protocol and the relay's database are versioned on their own: the wire
format is protocol version 1 (paths under `/v1`), and the database carries a
schema version a relay checks before reading. Any change that breaks either -
or the cursor's hash chain, which every client holds - gets a "Breaking
Changes" section here, with the way across or a plain statement that there is
none.

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



