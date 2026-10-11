# ADR 0014: What is pushed waits on the device, sealed and cut, until the relay has it

- Status: accepted
- Date: 2026-10-11

## Context

Until 0.3 a push was a request: if it failed, the items were the app's
problem again, and a push cut into several batches that failed partway said
how many made it (`Error::Interrupted`). An app on a phone would have to keep
its own outbox, and every app would write it differently - the line's rule
since kasl 1.9 is that a change the user made is not lost to a network that
went away or an app that closed.

The relay now knows a batch it has by what it holds (ADR 0013), which only
helps a writer that sends the same bytes again. Sealing draws a random nonce,
so sealing an item again makes another batch.

## Decision

- **`push` keeps before it sends.** It compresses and seals the items, cuts
  them into batches that fit one request, and keeps the batches in the
  device's state file - one transaction, all or none. Then it sends whatever
  waits for the stream, oldest first.
- **A kept batch never changes.** The schema refuses to change a queued batch
  or an entry of it, to add an entry to any batch but the one being queued,
  to take an entry out without its batch, and to take out any batch but the
  oldest of its stream. A batch goes again exactly as it was cut.
- **A batch belongs to the incarnation it was sealed for**, by a foreign key
  on `(name, stream)`: forgetting the stream takes its queue with it, and a
  queue cannot be sent into a stream that replaced the one it was meant for.
- **A batch leaves when the relay has it** - written now, or known as a
  repeat. What is left goes next time; sending resumes where it stopped.
- **`send` sends what waits** - at start, when the network is back, on the
  app's timer. The library does not retry on its own: when to try again is
  the app's call, and waiting loses nothing.
- **`Error::Unsent { unsent, source }` replaces `Error::Interrupted`.** Any
  stop with items still waiting says so in its type: the items are kept, and
  the source says why sending stopped - a network worth waiting for, or a
  stream that will not take them.
- **One sender per stream in a process**, through a lock the state hands out:
  two clients of one stream take turns instead of sending the same batch
  twice (which the relay would answer as a repeat, at the cost of a request).
- `push` and `send` return `Sent { items, batches }` - what went during the
  call. The positions of a push's own items are no longer returned: with a
  queue they are not always known when the call returns, and an app learns its
  entries' places when it pulls them back (`own`, `seq`).

## Consequences

- A change pushed is either on the relay or in the device's state, sealed, at
  every moment - a power cut included, with `synchronous=FULL`.
- The state file holds sealed items while they wait. It holds no item in the
  clear: the gate that searches the relay's files for plaintext searches the
  device's state with an item waiting in it.
- What waits for a stream that will never take it - gone, replaced, moved to
  an epoch only a newer app writes - stops every send with that reason until
  the app forgets the stream. The app's own data still has every change; it
  sends what the stream lacks after joining again, as it already does when a
  relay loses a stream.
- Batches are cut by the limits known when they are queued. A relay that
  announces smaller limits later (0.5) refuses a batch cut for the old ones
  with `too_large`, which writes nothing - so such a batch can be cut again
  without being doubled; that is 0.5's to do.
