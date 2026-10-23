---
title: Without a server
description: Sync through an S3 bucket - Amazon's, MinIO, or another service that speaks S3 - with nothing of your own to run, and what changes against a relay.
---

A relay is a server someone runs. A bucket is a relay nobody has to run: an S3
bucket - Amazon's, a MinIO of your own, or another service that speaks the S3
protocol - and credentials for it are all the devices need. They keep the
stream in it themselves, with the same seal, the same one order for everyone,
the same cursors and the same exactly-once writing as through a relay.

What keeps the order is the one thing S3 does atomically: creating an object
only if there is none under its key (`If-None-Match: *`). Every batch is
created under its first position that way, so two devices can never put two
batches in one place. [A stream in a bucket](/efema/reference/bucket/) has the
whole layout.

## What the bucket needs

- **To refuse to create an object that is there.** Amazon S3 does, since
  2024; MinIO does, and efema's tests run against it. Other services that
  speak S3 differ, and some take the header and ignore it. efema finds out
  before it writes anything: it creates the object `.efema-probe` - twice, if
  it was not there - and refuses to write to a bucket that lets the second
  creation through, rather than fork a stream there without a word.
- **To show a write to every read after it**, listings included. Amazon S3
  and MinIO do.
- **A bucket for the app's streams**, or one shared by several apps whose
  stream names differ. A stream lives under its name, at the bucket's root.

The credentials are an access key's id and secret, as the service issues them.
The secret never leaves the device: each request carries a signature made
with it (Signature Version 4).

## Open a stream

`Bucket` takes the place of `Relay`, and everything else is the same:

```rust
use efema::{Bucket, Client, Credentials, Epoch, Secret, State};

let state = State::open(data_dir.join("sync.sqlite3"))?;
let credentials = Credentials::new(key_id, secret);
let bucket = Bucket::new("https://notes.s3.eu-central-1.amazonaws.com", "eu-central-1", credentials)?;
let notes = Client::open(
    bucket,
    &state,
    "notes".parse()?,
    Epoch(1),
    Secret::Passphrase(passphrase.as_bytes()),
)
.await?;
```

- **The address is the bucket's own**, in whichever form the service gives:
  the bucket as the host (`https://<bucket>.s3.<region>.amazonaws.com`), or
  the service's address with the bucket as the path
  (`https://<account>.r2.cloudflarestorage.com/<bucket>`,
  `http://127.0.0.1:9000/<bucket>` for a MinIO on the device). `https://`,
  or plain `http://` to this machine only, as for a relay.
- **The region** is the bucket's: what Amazon shows next to it, `auto` for
  Cloudflare R2, `us-east-1` for MinIO unless it was set otherwise.

[Syncing an app](/efema/guides/syncing-an-app/) goes on from here - push,
pull, acknowledge, wait, doctor - word for word.

## A folder, through a bucket

`mini-vault`, the [example app](/efema/guides/a-synced-folder/), syncs through
a bucket with `--bucket` and the region; it reads the credentials from the
variables every S3 tool reads:

```console
$ export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_REGION=us-east-1
$ export MINI_VAULT_PASSPHRASE='several random words, not these'
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./laptop --state ./laptop-state sync --once
applied 0 change(s), sent 3
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./desktop --state ./desktop-state sync --once
applied 3 change(s), sent 0
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./desktop --state ./desktop-state sync --once
applied 0 change(s), sent 1
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./laptop --state ./laptop-state sync --once
applied 0 change(s), sent 2
conflict: both versions kept, the other one as notes/plan (conflict b2301866).md
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./laptop --state ./laptop-state doctor
relay    http://127.0.0.1:9000/notes - answered in 11 ms
access   reads and writes - no quota
stream   vault - stream 1830002e, epoch 1, 7 entries
cursor   at 5 - 2 entries to read
queue    empty
key      40bc9d631aa2ae73 - locked 2026-10-23 (today), argon2id 64 MiB × 3 × 4
device   8b2fd1d0
epoch    1 - written, and read up to
```

The same run as [through a relay](/efema/guides/a-synced-folder/), the same
merge. What the bucket holds after it - the probe, the stream's record, four
batches, and the empty offers made before each of them, none of it readable:

```console
$ mc ls --recursive live/notes
[2026-10-23 17:16:44 UTC]   135B STANDARD .efema-probe
[2026-10-23 17:16:45 UTC]   341B STANDARD vault/1830002e23535c313fea91b3aeb5c902/b/99999999999999999993
[2026-10-23 17:16:44 UTC]   202B STANDARD vault/1830002e23535c313fea91b3aeb5c902/b/99999999999999999994
[2026-10-23 17:16:44 UTC]  47KiB STANDARD vault/1830002e23535c313fea91b3aeb5c902/b/99999999999999999997
[2026-10-23 17:16:44 UTC]   218B STANDARD vault/1830002e23535c313fea91b3aeb5c902/b/99999999999999999998
[2026-10-23 17:16:44 UTC]     0B STANDARD vault/1830002e23535c313fea91b3aeb5c902/o/23864e5bffcf8611e216d79e0b6533d87b0f4874e4c6d430af97468b5d0e95f4/00000000000000000005
[2026-10-23 17:16:44 UTC]     0B STANDARD vault/1830002e23535c313fea91b3aeb5c902/o/4457df98e42d72bab9f2a5fc34f1322b24da7ef8f073fd164af4a448ec1d8687/00000000000000000001
[2026-10-23 17:16:45 UTC]     0B STANDARD vault/1830002e23535c313fea91b3aeb5c902/o/8acabac751f35058304c44b54d2740c9133a0383ffa7ea5a3e3012c526b426dc/00000000000000000006
[2026-10-23 17:16:44 UTC]     0B STANDARD vault/1830002e23535c313fea91b3aeb5c902/o/da4b8b78a41ac5f3e8b2d397627106d322d6a39633b9e524083429fc075b9844/00000000000000000002
[2026-10-23 17:16:44 UTC]    21B STANDARD vault/stream
```

A batch's key is the position of its first entry with its digits written from
nines - `99999999999999999998` is position 1 - so the newest batch lists
first.

## A device that only reads

Credentials whose policy lets them list and get the bucket's objects and put
none read the stream and write nothing: the probe's creation is refused, the
device learns it only reads, and nothing it pushes is kept to be sent - as
with a relay key that reads. An Amazon S3 policy for it, or the same in
MinIO:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:ListBucket"],
      "Resource": ["arn:aws:s3:::notes", "arn:aws:s3:::notes/*"]
    }
  ]
}
```

```console
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./phone --state ./phone-state sync --once
applied 6 change(s), sent 0
conflict: both versions kept, the other one as notes/plan (conflict b2301866).md
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./phone --state ./phone-state doctor
relay    http://127.0.0.1:9000/notes - answered in 9 ms
access   reads only - no quota
stream   vault - stream 1830002e, epoch 1, 7 entries
cursor   at 7 - up to date
queue    empty
key      40bc9d631aa2ae73 - locked 2026-10-23 (today), argon2id 64 MiB × 3 × 4
device   15ff8173
epoch    1 - written, and read up to
```

Credentials that write need `s3:PutObject` as well; efema never deletes.

## What it costs

S3 bills by the request and by what is kept. What efema asks of a bucket:

| Operation | Requests |
| --- | --- |
| A push of one batch | the stream's record (`GET`), the head and the batch's offers (`LIST` each), the batch's offer and the batch (`PUT` each), and a `HEAD` the first time a device sees the newest batch |
| A pull with nothing new | the record (`GET`), the head (`LIST`) |
| A pull with entries | as above, and a `GET` for each batch read |
| A wait | the record and the head once; then the record (`GET`) and the next place (`HEAD`) every five seconds, for each stream watched |
| The limits, and the first write of a run | one `PUT` of the probe |

`LIST` and `PUT` are the dearer kind on every service that bills by the
request, `GET` and `HEAD` the cheap one. A device that waits all day on one
stream makes some 35,000 cheap requests. Contention adds a few: a writer that
loses a place to another reads the head again and tries the next one.

What is kept is the entries - sealed, compressed first - and per batch a few
hundred bytes of framing and metadata, plus one empty object per offer.
Nothing is ever deleted: the stream only grows, as on a relay.

## A bucket or a relay

| | A bucket | A relay |
| --- | --- | --- |
| Something to run | nothing | `efema-server`, behind an HTTPS proxy |
| A waiting device learns of a write | within five seconds | at once |
| Who can reach the streams | whoever has credentials to the bucket - the service's own policies decide what they may do | an access key per device, to one space, revoked with one command |
| Limits | the protocol's defaults; no quota the client can see | each key held to the operator's limits, announced |
| Who sees names, sizes and times | the bucket's owner, and the service | the relay's operator |
| Who can read the entries | nobody without the stream's key | nobody without the stream's key |

The [threat model](/efema/concepts/threat-model/#in-a-bucket) has what a
bucket's owner can and cannot do.

## Related

- [A stream in a bucket](/efema/reference/bucket/) - the objects, their keys
  and what each holds.
- [Syncing an app](/efema/guides/syncing-an-app/) - the client library.
