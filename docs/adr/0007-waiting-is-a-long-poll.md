# ADR 0007: Waiting is a long poll that carries positions only

- Status: accepted
- Date: 2026-10-08

## Context

A device that is up to date should not ask the relay every few seconds whether
anything changed - least of all a phone. It needs to be told. The two usual
ways over HTTP are Server-Sent Events, a response that stays open and carries
events, and a long poll, a request the server holds until there is news or a
timeout.

## Decision

A **long poll**: `GET /v1/wait`, naming every stream the device watches with
its cursor in each, and a timeout of at most 60 seconds. The relay answers as
soon as any watched stream differs from the cursor - more entries, another
identity, fewer entries - or with an empty answer when the timeout runs out or
the relay is stopping. The answer carries positions, never entries.

Chosen over SSE because:

- **One encoding.** SSE is a text format; its events would be JSON beside CBOR
  everywhere else. A long poll answers in the same CBOR as every other endpoint.
- **Bounded requests.** Each wait ends within a minute, below the idle limits of
  common reverse proxies, with no keep-alive comments to tune.
- **Nothing to parse but a response.** A client needs no event-stream reader,
  and a mobile client woken by its operating system makes one request and gets
  one answer.

One wait watches many streams, so a device syncing several things holds one
connection, not one per stream.

## Consequences

- An active stream costs a request per change notification rather than an
  event on an open connection; with HTTP keep-alive that is a small request
  line, not a new connection.
- Waking is per stream: the relay keeps one watch channel per stream, updated
  after each commit in commit order, and a waiter wakes only for streams it
  named.
- Stopping the relay answers every wait at once, so shutdown never waits out a
  minute-long request.
