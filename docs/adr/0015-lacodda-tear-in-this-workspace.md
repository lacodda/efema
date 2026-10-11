# ADR 0015: lacodda-tear - torn connections for tests - lives here, with a version of its own

- Status: accepted
- Date: 2026-10-11

## Context

0.4 promises that a push survives the network breaking. A promise about
networks shown against a mock of the relay shows nothing: the mock never
breaks the way a socket does - partway through a request, after the server has
written but before its answer arrives, all connections at once when a phone
drops off. The tests need the real relay on a real socket, and something
between it and the client that breaks connections on cue.

turnout and kasl talk to servers over networks that break too, and would
write the same tool for their tests. The line's rule is a shared crate once a
second consumer is visible.

## Decision

`lacodda-tear` is a crate of this workspace, `crates/lacodda-tear`, published
to crates.io from here, with **its own version** - as `lacodda-seal` is
(ADR 0008). A TCP proxy for tests, on the loopback interface, in front of one
server:

- **cuts by bytes** - after so many more bytes up (a request torn partway) or
  down (an answer torn partway; zero loses it after the server sent it);
- **cuts by time**, **now**, **offline/online** (open connections torn, new
  ones reset at once), and **stall/flow** (bytes held, connections open);
- byte counts run **across connections** from the moment a cut is armed, so a
  client's connection pool does not change where a cut lands;
- a tear is a **reset**, as a dropped mobile connection looks to both ends,
  not an orderly end of stream;
- it knows nothing of HTTP.

It is a dev-dependency of `efema-server`, `efema` and the example, published
before them.

## Consequences

- The client's promises about broken connections are shown against the real
  relay, and the relay is asked around the proxy what it really holds.
- Another first version to publish by hand, and another trusted publisher to
  attach on crates.io.
- turnout and kasl take it from crates.io; if it outgrows efema, moving it is a
  new trusted publisher, not a change for its users.
