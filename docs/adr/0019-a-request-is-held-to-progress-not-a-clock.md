# ADR 0019: A request is held to progress, not to a clock

- Status: accepted
- Date: 2026-10-19

## Context

Up to 0.5 the client gave every request 300 seconds as a whole, and a wait
its own time and 30 seconds more. Two failures hide in one number:

- **A connection that goes silent holds a push for five minutes.** A phone
  that drops off a mobile network mid-request, or a proxy whose upstream hung,
  leaves the connection open with nothing moving on it. TCP gives up on its
  own only after minutes; keepalive probes do not help against a proxy, whose
  own kernel answers them.
- **A large batch on a slow network is cut every time.** A full request is up
  to 16 MiB; at 50 kB/s that is more than 300 seconds. The client keeps the
  batch and sends it again - and is cut again.

A shorter clock fixes the first and makes the second worse. reqwest's
`read_timeout` looks like the answer and is not: until the answer's headers
arrive it is one timer started when the request is sent, which no byte of the
request going up resets. It cuts a slow upload just like the 300 seconds did.

## Decision

- **A request lives as long as it moves.** What moves is the request's body
  being taken by the connection, the answer's headers arriving, and each
  piece of the answer's body. Once nothing has moved for `STALL` - 30
  seconds, public as `efema::STALL` - the request stops with
  `TransportError::Stalled`, and a push keeps its batches like any other
  failure of the network. There is no limit on the request as a whole.
- **The body goes to the connection a piece at a time** - 16 KiB, through
  `reqwest::Body::wrap` over a body of the client's own - so each piece taken
  is progress the client sees.
- **Once the last piece is taken, the answer has `STALL` to begin, plus the
  time the end of the body may still spend going up.** The connection takes a
  body ahead of the network, into the HTTP client's buffer (some 400 KiB) and
  the socket's; up to 1 MiB of it may still be on its way. The client allows
  for that on the slowest network it waits for, 64 kbit/s: a few kilobytes
  add nothing, a large batch up to 128 seconds.
- **A wait is allowed its own time first**: the relay holds the answer until
  the stream moves or the time runs out, and that silence is the request
  working.
- **A slow network is a test, not a claim.** lacodda-tear passes bytes at a
  set rate (`Tear::throttle`), and a push and a pull through it, each longer
  than `STALL` with bytes moving all the while, must finish.

## Consequences

- A push caught by a silent connection is kept on the device within about
  half a minute, instead of five.
- A batch of any size goes up any network faster than 64 kbit/s, however long
  it takes. Below that, the end of a large batch may be taken for a stall
  while it is still in the buffers: it is kept, and goes again.
- A relay - or anything posing as one - can hold a request open by trickling
  bytes. That is the denial of service a dishonest relay can do anyway; the
  app can drop the future.
