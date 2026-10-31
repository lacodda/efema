---
title: A stream in a bucket
description: Layout 1 of a stream kept in an S3 bucket - the objects, their keys and bodies, the metadata, and how a batch is appended, read and waited for without a server.
---

How devices keep a stream in an S3 bucket with no relay in between. The Rust
implementation is `efema::Bucket`; this page is the same contract for anyone
writing a client in another language. Every device that syncs a stream reads
what every other one wrote, so the layout is a format like the
[protocol](/efema/reference/protocol/): it has a number, and a stream laid out
in another is not read as this one.

The batches, entries, cursors, chain and batch key are the protocol's own -
the same CBOR, the same formulas. Only where they are kept differs.

## Objects

| Key | Body | Created |
| --- | --- | --- |
| `.efema-probe` | a line of text | by every client, to check the bucket |
| `<stream>/stream` | the stream's [record](#record) | once, by the stream's first batch |
| `<stream>/<id>/b/<first, from nines>` | a [stored batch](#stored-batch) | once per batch |
| `<stream>/<id>/o/<batch key>/<first>` | nothing | before each attempt to put a batch at `first` |

`<stream>` is the stream's name, `<id>` its identity and `<batch key>` the
[batch key](/efema/reference/protocol/#the-batch-key), in lowercase
hexadecimal. A position in a key has twenty decimal digits - as many as the
largest one; `<first>` in an offer is written as it is, zero-padded, and in a
batch's key **from nines**: each digit `d` as `9 - d`. Position 1 is
`99999999999999999998`, position 10 is `99999999999999999989`.

So a listing of `<stream>/<id>/b/`, which S3 returns in the order of the
keys, starts with the newest batch.

The keys may be under a **prefix** - `apps/notes/` before each of them, the
probe included - so one bucket holds the streams of several apps or people,
each with credentials that reach only its own. The prefix is where a client is
told to look; nothing in the objects names it, and the layout under it is the
same. A prefix is parts of letters, digits, `-`, `_` and `.` between slashes,
none of them `.` or `..`.

Every object above except the offers is created with `If-None-Match: *` - and
only if there is none under its key. The offers are written plainly. Nothing
is ever deleted.

## Record

The stream's incarnation, and the layout it is in:

| Key | Field | Type | Meaning |
| --- | --- | --- | --- |
| 0 | `id` | identity | the stream's identity: 16 random bytes, drawn by the device that created the record - or the identity of the stream a copy brought here |
| 1 | `format` | uint | the layout: `1` |

A client that finds another `format` reads nothing of the stream.

## Stored batch

| Key | Field | Type | Meaning |
| --- | --- | --- | --- |
| 0 | `first` | uint | the position of the batch's first entry |
| 1 | `previous` | link | the chain's link at `first - 1`: the stream's genesis link for the first batch |
| 2 | `batch` | [batch](/efema/reference/protocol/#batch) | the batch as its writer sent it, without `stream` |

Each entry's position and link follow from these: the `n`-th entry is at
`first + n`, and its link is computed from the one before it, starting from
`previous`, by [the chain](/efema/reference/protocol/#the-chain).

The object's metadata says what a reader needs without the body:

| Header | Value |
| --- | --- |
| `x-amz-meta-efema-last` | the position of the batch's last entry |
| `x-amz-meta-efema-epoch` | its epoch |
| `x-amz-meta-efema-link` | the link at its last entry |
| `x-amz-meta-efema-key` | the batch key |

A batch whose body and metadata disagree - any of the four, or its first
position and its key - is not read.

## Appending

To append a batch to `<stream>`:

1. Read the record. If the batch names an identity, the record must have it:
   otherwise the batch is refused with `stream_replaced`, or with
   `stream_not_found` if there is no record - unless the batch's `first` is 1:
   then it creates the record with that identity, the first batch of a copy.
   A batch that names none creates the record if there is none, with an
   identity drawn for it. A record is created with `If-None-Match: *`; if
   another device created it first, its identity is the stream's.
2. Find the head: the newest batch is the first key of a listing of
   `<stream>/<id>/b/`, its last position and link in its metadata. A stream
   with no batch has head 0, the genesis link and epoch 0.
3. List `<stream>/<id>/o/<batch key>/`. If a batch at one of the positions
   offered there has this batch key, the batch is in the stream: answer with
   where it landed, as `repeated`.
4. A batch whose `first` is not `head + 1` is refused with `out_of_place`, and
   `head`. A batch in an epoch below the head's is refused with
   `epoch_behind`.
5. Write the offer `<stream>/<id>/o/<batch key>/<head + 1>`.
6. Create the batch at `head + 1` with `If-None-Match: *`. Created: answer
   with its positions. Refused with `412`: another batch has the place - read
   the head again and go back to step 3. (`409` means another creation of the
   same key is under way: try the same request again.)

Step 3 comes after step 2, and again after every place lost: a batch at or
below the head a writer saw was created after its offer, so the offer is
there to be listed. Looked for before the head, two sendings of one batch
could each miss the other.

A batch is refused when it is larger than the protocol's
[defaults](/efema/reference/protocol/#ceilings-and-defaults) - 16 MiB as CBOR,
8 MiB for an entry - with `too_large`, as on a relay.

## Reading

To read after a cursor at position `p`:

1. Read the record: the cursor's identity must be the record's
   (`stream_replaced`).
2. Find the head, as for appending. A cursor past it is `cursor_ahead`.
3. Find the batch that holds `p`: the first key of a listing of
   `<stream>/<id>/b/` that starts after the key of `p + 1`. The cursor's link
   must be the one at `p` - from the metadata when `p` is the batch's last
   position, else computed from its body - or it is `cursor_diverged`.
4. Read on from `p + 1`: the rest of that batch, then the batch at the last
   position plus one, and so on, to the head or the page's end - its `limit`
   in entries, or 8 MiB of entry data, never empty while there is an entry.

The answer is a [page](/efema/reference/protocol/#page), the head and epoch
those of step 2.

## Waiting

Nothing in a bucket wakes anyone. A wait reads each watched stream's record
and head once, and answers at once if any moved - by the
[relay's rule](/efema/reference/protocol/#waiting-get-v1wait). Then every five
seconds, until its time is up, it reads the record again and asks for the
next place - a `HEAD` of the batch at the head plus one - and reads the head
again only when something is there.

## The probe

Before its first write, and whenever it is asked for its limits, a client
creates `.efema-probe` with `If-None-Match: *`. Refused with `412`: the bucket
honours the condition. Created: it creates it once more, and if that is not
refused either, the bucket does not honour it, and the client writes nothing
to it. Refused with `403`: the credentials only read.

## Limits

A bucket announces the [limits](/efema/reference/protocol/#limits-get-v1limits)
of a relay with its operator's defaults, except that its space has no quota
(`quota` is the largest `uint`, 2⁶⁴ - 1) and nothing is counted (`used` is 0),
and no rate is held (`rate` is 2⁶⁴ - 1). `write` is what the probe found.

## Related

- [Without a server](/efema/guides/without-a-server/) - syncing through a
  bucket, and what it costs.
- [Moving a stream](/efema/guides/moving-a-stream/) - from a relay into a
  bucket, or back.
- [Protocol](/efema/reference/protocol/) - the batches, pages and cursors
  stored here.
