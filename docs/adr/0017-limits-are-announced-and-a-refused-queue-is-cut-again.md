# ADR 0017: Limits are the operator's, announced per key; a queue refused as too large is cut again

- Status: accepted
- Date: 2026-10-14

## Context

0.4 had one set of limits, the protocol's, written into both the relay and
the client: two copies of one number. A relay that can be left reachable needs
limits its operator picks - a Raspberry Pi has less disk than a server - and
limits on what a stranger with a write key can cost: how large a request, how
large an entry, how much data, how many requests.

Once the relay picks its limits, the client cannot know them in advance. And
0.4 keeps what a device pushes on the device, cut into batches, until the
relay has it: batches cut for yesterday's limits may be too large for today's.

## Decision

- **The protocol keeps ceilings; the operator can only go below them.** A
  request body is at most 16 MiB on any relay. `serve` takes `--max-body`,
  `--max-entry`, `--space-quota` and `--rate`; the relay refuses to start with a
  limit above its ceiling.
- **The quota is per space, not per stream.** A key that writes can create
  streams; a quota per stream would bound nothing. The number of spaces is the
  operator's.
- **The rate is per key**: a bucket of a minute's requests, refilled evenly,
  in memory. A refusal says when to ask again (`Retry-After`, and `retry` in the
  problem). The client does not retry on its own.
- **The relay announces the limits a key is held to** at `GET /v1/limits`:
  whether the key writes, the body, entry, quota and rate, how much the space
  holds now, and the page and wait limits. One source; the docs give only the
  defaults.
- **A batch the stream has is answered before any limit is looked at.** The
  relay reads the body up to the protocol's ceiling, finds the batch by its key
  (ADR 0013), and only for a new batch checks the operator's limits and the
  quota. So `too_large` and `quota_exceeded` mean the stream does not have the
  batch - whatever the limits were when it was first sent.
- **The client keeps the announced limits in the device's state** and cuts
  every push to them without the network. It refuses a push with a key that
  reads before keeping anything, so such a device's queue does not fill with
  what can never go.
- **A queue refused as too large is cut again, all of it, in one transaction.**
  The relay does not have the refused batch, and nothing behind it was sent -
  a send goes strictly in order and stops at the first failure - so cutting
  them again lands each entry once. The client asks for the limits, cuts runs
  of one epoch again, and goes on; once per send, so a relay that refuses
  batches cut to its own limits is not argued with. An entry larger than what
  the relay takes now cannot be cut: it waits, and `ItemTooLarge` says so.
- **The client asks for the limits again** after a send that reached the relay,
  when what it keeps is older than a day: a raised limit is heard within a day,
  a lowered one at the next refusal.

## Consequences

- Opening a stream for the first time asks for the limits, which also checks
  the key; a device opening a stream it knows still needs no network.
- An operator lowering limits costs each device one refusal and one request.
- A space that is full refuses new batches with `quota_exceeded`; what devices
  push waits on them. Making room - a larger quota now, retention later - is the
  operator's.
