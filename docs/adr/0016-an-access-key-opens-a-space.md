# ADR 0016: An access key opens a space; a key reads, or reads and writes

- Status: accepted
- Date: 2026-10-14

## Context

Until 0.5 anyone who reached the relay's port could read and write every
stream. Sealing kept them from reading what the entries say - but a stranger
could download every lock of every stream key and try passphrases offline,
fill a stream with entries the devices refuse, raise its epoch so no device
could write, or start a stream under a name its owner meant to use. A relay
had to stay on loopback or a trusted network.

Three things were asked of access control. Separate keys for reading and for
writing, so a device that only reads cannot harm the stream. Several streams
behind one key, each with a name - scheda syncs a vault per stream, kilna a
stream of its own - so a new stream does not need the operator. And names that
belong to their owner: two people on one relay each have a `notes`.

## Decision

- **A space** is a set of streams, named by the operator. A stream lives in one
  space, under a name that is unique there and nowhere else; the database
  holds it to that (`UNIQUE (space, name)`, and a trigger that keeps a stream
  in its space).
- **An access key opens one space.** The space comes from the key, not from the
  URL: `/v1/streams/{name}` is unchanged, a device never names its space, and
  the protocol stays version 1.
- **Two kinds of key: `read`, and `write`, which reads too.** A device that
  writes has to read - it finds the stream's key in the stream - so a key that
  only writes would be one no client could use. A key that reads cannot
  append, which also means it cannot create a stream or raise an epoch.
- **Every request under `/v1` shows a key**, as `Authorization: Bearer <key>`.
  There is no mode without keys: an open relay by oversight is the same class
  of mistake as a relay without sealing, and efema has neither.
- **The key is `efema_` and 32 random bytes in hex.** The relay keeps only
  `SHA-256("efema/access/v1" || bytes)`: its database gives no key away, and a
  key of 32 random bytes is not found by trying. A key is named by the first
  eight hex digits of that digest - in the listing, in a revocation.
- **The operator hands keys out and revokes them** with `efema-server keys`,
  on a running relay too: the relay looks a key up in its database on every
  request, so a revoked key opens nothing from its next request on. Revoking
  deletes the row.
- **Streams of a relay older than spaces** move into a space named `default`
  when 0.5 first opens its database; the operator hands out keys to it.

## Consequences

- A relay can be left reachable by others: without a key nothing under `/v1`
  answers, and a key reaches one space.
- The client takes the key with the relay's address (`Relay::new(url, key)`),
  and says `Unauthorized` and `ReadOnly` as errors of their own.
- When a key was last used is written down at most once a minute per key - for
  the listing and the metrics - so a device that syncs does not turn every read
  into a write on the relay's disk.
- An access key is not the stream's key. It decides who may reach the sealed
  entries; the passphrase still decides who can open them.
