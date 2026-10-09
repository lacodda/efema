# ADR 0010: The client - async, at least once, and a writer names its stream

- Status: accepted
- Date: 2026-10-09

## Context

The client library is the part of efema that apps link to, and the vision puts
it above the relay in importance. It has to survive the network and restarts,
stay the same across every transport the plan has (the relay, S3, WebDAV), and
refuse the cases where carrying on would lose or mix data silently.

## Decision

- **Async**, on tokio, with one API. The apps of the line that sync are Tauri
  desktop apps with a runtime already; a blocking twin would be a second API
  to keep in step, for no consumer.
- **A `Transport` trait at the protocol's level** - append, read, wait,
  limits - over opaque entries. Sealing, keys and cursors live above it, in
  `Client`, so a second transport is the protocol over another medium, not a
  second client. The relay over HTTP is the first; S3 (0.7) is next.
- **A device's state is one SQLite file**: the device identity, and per stream
  the incarnation, the locked key and the acknowledged cursor. The outbox of
  0.4 goes into the same file, in the same transactions. One process holds it
  at a time, through a lock file next to it: two processes on one file would
  be one device writing twice.
- **Delivery is at least once.** `pull` reads from the acknowledged cursor and
  does not move it; `ack` does, after the app has applied the page. A crash in
  between hands the same entries over again, and apps apply idempotently.
- **A pull returns everything it can.** The first entry it cannot - a newer
  epoch, a broken link, an altered or foreign entry - ends the page and is the
  next pull's error, with the cursor in front of it.
- **The client checks the chain itself.** Every link is recomputed on reading,
  so a relay's accident is caught on the device - and a transport with no
  relay to compute links (S3) has the check already.
- **A writer names the stream it means.** A batch carries the incarnation the
  writer knows (`stream`, key 2, optional), and the relay refuses it with
  `stream_not_found` or `stream_replaced` instead of creating a fresh stream
  under the name. Without it, a relay that lost its data would take a device's
  next batch as the first entries of a new stream, with no key in front of
  them. Only a stream's first batch - its key - is written without it.
- **A push larger than one request is cut into batches** that each fit the
  transport's limits, measured exactly against the CBOR encoding. Each batch
  is whole; a push that stops partway says how many items landed.

## Consequences

- The limits of protocol version 1 moved from the relay to `efema-proto`, so
  the relay and the client read one definition. A relay that announces its own
  (0.5) answers the same `Transport::limits` call.
- An app keeps applying idempotent under redelivery - which a sync app needs
  anyway, since two devices can make the same change.
- `State::forget` is the way back from a stream the relay no longer has as the
  device knew it; what the app then does with its own data is the app's call.
