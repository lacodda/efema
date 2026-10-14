---
title: Protocol
description: Version 1 of the efema protocol - access keys and spaces, the endpoints, the CBOR bodies with their keys, the refusals and the limits.
---

What a client and a relay say to each other. The Rust definitions are the
[`efema-proto`](https://docs.rs/efema-proto) crate, which both sides build on;
this page is the same contract for anyone writing a client in another
language.

## Endpoints

| Method | Path | What |
| --- | --- | --- |
| `POST` | `/v1/streams/{name}/entries` | append a [batch](#batch) |
| `GET` | `/v1/streams/{name}/entries` | read a [page](#page) after a cursor |
| `GET` | `/v1/wait` | wait until a stream moves; answers with [heads](#heads) |
| `GET` | `/v1/limits` | what the [access key](#access) may do, and how much: [limits](#limits-get-v1limits) |
| `GET` | `/health` | `200` and `ok` while the relay is up |
| `GET` | `/metrics` | counts and times, for the operator - see [`serve`](/efema/reference/serve/#metrics) |

Everything under `/v1` answers in CBOR (`application/cbor`), refusals included,
and with `Cache-Control: no-store`. A later major version of the protocol
gets a prefix of its own.

`{name}` is a stream name: one to 64 characters of lowercase letters, digits,
`-`, `_` and `.`, starting with a letter or a digit.

## Who the relay answers

The relay has no TLS of its own. It answers a request made on its own machine,
and one that a reverse proxy took over HTTPS and forwarded; everything else is
`403 https_required`, on every path, before anything else about the request is
looked at.

Both are told by the connection's address: loopback, or one the operator
trusts as the proxy (`serve --trust-proxy`). A request from such an address
with no forwarding header - `Forwarded`, `X-Forwarded-Proto`,
`X-Forwarded-For`, `X-Forwarded-Host` - was made there. One with them was
forwarded, and passes only if every protocol they name - each value of
`X-Forwarded-Proto`, each `proto=` of `Forwarded` - is `https`. A forwarded
request that names no protocol is refused.

## Access

Every request under `/v1` carries an access key:

```
Authorization: Bearer efema_<64 lowercase hex digits>
```

The relay's operator hands keys out (`efema-server keys add`). A key opens one
**space** - a set of streams - and every stream name in a request is a stream
of that space: two spaces may each have a `notes`, and neither reaches the
other's. A key **reads** the space, or **writes** to it, which includes
reading. A request with no key, a malformed one or one the relay does not know
- never handed out, or revoked - is `401 unauthorized`, with
`WWW-Authenticate: Bearer realm="efema"`; a write with a key that reads is
`403 forbidden`.

A relay keeps only `SHA-256("efema/access/v1" || key)` of each key - the
key's 32 bytes, not its text - and names a key by the first eight hex digits
of it.

Each key may make a number of requests a minute, as many at once; past that,
`429 rate_limited`, with `Retry-After` and `retry` saying in how many seconds
to ask again.

## Encoding

Bodies are CBOR maps keyed by small integers - the numbers in the tables below.
A decoder ignores keys it does not know, so a field added later breaks nobody;
a required field stays for the life of the protocol version. Byte strings are
CBOR byte strings: an entry travels as its own bytes, with no base64 and no
growth. A body is exactly one message: bytes after it are refused.

Types: `uint` is an unsigned integer; an *identity* is a byte string of exactly
16 bytes, a *link* one of exactly 32.

## Writing: `POST /v1/streams/{name}/entries`

The body is a [batch](#batch), sent with `Content-Type: application/cbor`. The
answer is a [written](#written) record.

The relay gives the entries consecutive positions after the stream's head, in
the order listed, all of them or none. A batch in an epoch below the stream's
is refused; one above it raises the stream. A batch is refused, too, when its
body or one of its entries is larger than the relay takes (`too_large`), and
when its entries would take the space past its quota (`quota_exceeded`) - the
[limits](#limits-get-v1limits) say how much that is.

A batch that names a `stream` goes into that incarnation only: if the name
belongs to another one now, it is refused with `stream_replaced`, and if to
none, with `stream_not_found` - nothing is created. A batch without it goes to
whatever stream has the name, and creates the stream if none does: that is how
a stream's first batch is written, and the only batch a client sends without
it.

The body is read to its end before anything is written: a connection torn
partway through a batch writes none of it.

### Sending a batch again

A batch is known by what it holds - its [key](#the-batch-key). A batch the
stream already has is not written again: the answer is the
[written](#written) record of where it landed the first time, with `repeated`
set. So a writer whose answer was lost - the connection torn after the relay
wrote the batch - sends the same batch again, byte for byte, and the stream
holds it once. Nothing has to be kept for this beyond the batch itself.

A repeat is known for as long as the stream exists - not for a while - and
before the epoch, the limits and the quota are checked: a batch written before
the stream moved to a newer epoch, or before the operator lowered a limit, is
still answered. So `too_large` and `quota_exceeded` mean the stream does not
have the batch, and its writer may cut it again. A repeat is not known across
incarnations: the stream it names must still be the one it went into.

The same entries in another epoch, or cut into batches differently, are
another batch. A writer that means two equal batches - the same bytes, twice -
cannot have them: in efema every entry is sealed with a random nonce, so no two
batches a client writes are equal.

### The batch key

```
key = SHA-256("efema/batch/v1" || epoch: u32 || count: u64 || (length(data): u64 || data) for each entry)
```

Integers are big-endian; `count` is the number of entries. The stream the
batch names is not part of it. A relay keeps the key of every batch it writes,
so the formula cannot change within a protocol version.

### batch

| Key | Field | Type | Meaning |
| --- | --- | --- | --- |
| 0 | `epoch` | uint, 32-bit | the epoch the entries are written in |
| 1 | `entries` | array of byte strings, at least one | the entries, opaque to the relay |
| 2 | `stream` | identity, optional | the incarnation the writer means to append to |

### written

| Key | Field | Type | Meaning |
| --- | --- | --- | --- |
| 0 | `stream` | identity | the stream written to - new if this batch created it |
| 1 | `epoch` | uint | the stream's epoch after the write |
| 2 | `first` | uint | the position of the batch's first entry |
| 3 | `last` | uint | the position of its last entry |
| 4 | `repeated` | bool, optional | `true` when the stream had this batch already and nothing was written now; absent otherwise |

## Reading: `GET /v1/streams/{name}/entries`

| Parameter | Default | Meaning |
| --- | --- | --- |
| `after` | none: from the start | a [cursor](#cursors); entries after it are returned |
| `limit` | 1000 | the most entries to return, from 1 to 10000 |

The answer is a [page](#page). A page ends at `limit` entries, or earlier once
its entries' data passes 8 MiB - but never empty while there is an entry to
return, however large. When the last entry's position is below `head`, there
is more: read again with the cursor of that entry.

### page

| Key | Field | Type | Meaning |
| --- | --- | --- | --- |
| 0 | `stream` | identity | the stream read |
| 1 | `epoch` | uint | the stream's current epoch |
| 2 | `head` | uint | the stream's last position at the moment of the read |
| 3 | `entries` | array of [entry](#entry) | in position order, without gaps |

### entry

| Key | Field | Type | Meaning |
| --- | --- | --- | --- |
| 0 | `seq` | uint | the entry's position, from 1 |
| 1 | `epoch` | uint | the epoch it was written in |
| 2 | `hash` | link | its link in the stream's chain |
| 3 | `data` | byte string | the bytes as the writer sent them |

## Cursors

In a query string a cursor is `<seq>.<stream>.<hash>`: the position as a
decimal number without leading zeros, then the identity and the link in
lowercase hexadecimal. One spelling per cursor. The cursor of a reader that has
an entry is that entry's `seq` and `hash` with the page's `stream`; a reader
that knows a stream but has none of it holds position 0 and the chain's first
link.

### The chain

Links are SHA-256. With `||` for concatenation and integers big-endian:

```
genesis = SHA-256("efema/chain/v1/genesis" || stream)
link(n) = SHA-256(link(n-1) || seq: u64 || epoch: u32 || length(data): u64 || data)
```

where `link(0)` is `genesis` and `stream` is the identity's 16 bytes.

## Waiting: `GET /v1/wait`

| Parameter | Default | Meaning |
| --- | --- | --- |
| `watch` | required, up to 64 | a stream to watch: `<name>`, or `<name>:<cursor>`; repeat for each stream |
| `timeout` | 30 | seconds to wait, from 0 to 60; 0 only looks |

The relay answers as soon as a watched stream differs from what the waiter
knows, or when the timeout runs out. With a cursor, "differs" is any other
identity or head than the cursor's - including a stream that is gone. Without
one, it is a stream with any entries at all.

The answer is a [heads](#heads) record listing every watched stream that
differs; it is empty after a timeout, and when the relay is stopping. It never
carries entries: the waiter reads what it missed.

### heads

| Key | Field | Type | Meaning |
| --- | --- | --- | --- |
| 0 | `heads` | array of [head](#head) | the watched streams worth reading now |

### head

| Key | Field | Type | Meaning |
| --- | --- | --- | --- |
| 0 | `stream` | text | the stream's name |
| 1 | `id` | identity, or absent | its identity; absent if no stream has the name |
| 2 | `head` | uint | its last position; 0 if there is no stream |

## Limits: `GET /v1/limits`

What the relay holds the key to, as the operator set it, and how much its
space holds now. A client cuts its batches and pages to these rather than to
the [ceilings](#ceilings-and-defaults): an operator may have set any of them lower. Sizes
are in bytes.

### limits

| Key | Field | Type | Meaning |
| --- | --- | --- | --- |
| 0 | `write` | bool | whether the key writes; `false` for a key that only reads |
| 1 | `body` | uint | the largest request body - a batch, as CBOR |
| 2 | `entry` | uint | the largest entry |
| 3 | `quota` | uint | bytes of entry data the space may hold, all its streams together |
| 4 | `used` | uint | bytes of entry data the space holds now |
| 5 | `rate` | uint | requests the key may make in a minute |
| 6 | `page_entries` | uint | entries in a page when the reader does not say |
| 7 | `page_entries_max` | uint | the most entries a reader may ask for |
| 8 | `page_bytes` | uint | bytes of entry data in a page, beyond which it ends early |
| 9 | `wait_max` | uint | the longest wait, in seconds |
| 10 | `watches` | uint | the most streams one wait may watch |

## Refusals

A refusal has a status and a [problem](#problem) body. Programs act on `code`;
`message` is for people and may change.

| Status | `code` | When |
| --- | --- | --- |
| 400 | `bad_request` | a body that is not the expected CBOR, an empty batch, an invalid name or query parameter - unknown parameters included |
| 401 | `unauthorized` | no access key, a malformed one, or one the relay does not know |
| 403 | `forbidden` | a write with a key that only reads; `/metrics` through the proxy |
| 403 | `https_required` | a request that is neither from the relay's machine nor forwarded over HTTPS by a trusted proxy |
| 404 | `stream_not_found` | a read of a stream that does not exist in the key's space, or a batch naming a stream when the name has none |
| 404 | `not_found` | a path the relay does not serve |
| 405 | `method_not_allowed` | a path it serves, with another method |
| 409 | `stream_replaced` | a cursor, or a batch naming a stream, of another identity than the stream's |
| 409 | `cursor_ahead` | a cursor past the stream's head; `head` says where the stream ends |
| 409 | `cursor_diverged` | a cursor whose link the stream does not have at that position |
| 409 | `epoch_behind` | a batch in an epoch below the stream's; `epoch` says the stream's |
| 413 | `too_large` | a body, or an entry, over the limit; `limit` says it |
| 415 | `unsupported_media_type` | a batch not sent as `application/cbor` |
| 429 | `rate_limited` | more requests with the key than the relay allows in a minute; `retry` says when to ask again |
| 500 | `internal` | something failed on the relay's side; its log says what |
| 507 | `quota_exceeded` | a batch that would take the space past its quota; `limit` says it |

A client keeps codes it does not know: a later relay may add reasons.

### problem

| Key | Field | Type | Meaning |
| --- | --- | --- | --- |
| 0 | `code` | text | the reason, from the table above |
| 1 | `message` | text | the reason, in a sentence |
| 2 | `head` | uint, optional | with `cursor_ahead` |
| 3 | `epoch` | uint, optional | with `epoch_behind` |
| 4 | `limit` | uint, optional | with `too_large` and `quota_exceeded`, in bytes |
| 5 | `retry` | uint, optional | with `rate_limited`, in seconds |

## Ceilings and defaults

The protocol's ceilings, which no relay goes above, and the defaults of a
relay whose operator sets nothing. What a particular relay holds a key to is
what it [announces](#limits-get-v1limits).

<!-- limits: the test release_consistency holds these to efema_proto::limits -->

| Limit | Value |
| --- | --- |
| request body, at most and by default | 16777216 bytes (16 MiB) |
| entry, by default | 8388608 bytes (8 MiB) |
| space quota, by default | 8589934592 bytes (8 GiB) |
| requests per key in a minute, by default | 600 |
| entries in a page, by default | 1000 |
| entries in a page, at most | 10000 |
| entry data in a page | 8388608 bytes (8 MiB) |
| wait, by default | 30 seconds |
| wait, at most | 60 seconds |
| streams in one wait | 64 |
| stream name | 64 characters |

## Example

A first write to `notes` and the read that follows, in CBOR's diagnostic
notation:

```
POST /v1/streams/notes/entries
Authorization: Bearer efema_…
{0: 1, 1: [h'…' / 640 bytes /, h'…' / 560 bytes /]}

200
{0: h'11c3cd7e971ccb544d78c74ae8d814ba', 1: 1, 2: 1, 3: 2}

GET /v1/streams/notes/entries
Authorization: Bearer efema_…

200
{0: h'11c3cd7e971ccb544d78c74ae8d814ba', 1: 1, 2: 2, 3: [
  {0: 1, 1: 1, 2: h'0d72a730…' / 32 bytes /, 3: h'…' / 640 bytes /},
  {0: 2, 1: 1, 2: h'19cdc65a…' / 32 bytes /, 3: h'…' / 560 bytes /}
]}
```

The reader's cursor is now
`2.11c3cd7e971ccb544d78c74ae8d814ba.19cdc65a0a991e8d40e19813aad37514cf0a734663686eb4433285f7d49190ed`.

## Related

- [Streams and cursors](/efema/concepts/streams-and-cursors/) - what these
  messages mean.
- [`serve`](/efema/reference/serve/) - the relay that answers them.
