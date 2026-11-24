---
title: Temporary credentials
description: Sync through an S3 bucket with credentials handed out for an hour - a session token in the signature, and a provider the bucket asks again before they run out.
---

An access key held for good on every device is taken back only by changing
it everywhere. The careful way to let a device into a bucket is to hand it
credentials for an hour: the service's STS - or the app's own backend, asking
it - gives out an access key, its secret, a **session token** that goes with
every request, and the moment they run out. Taking them back is not handing
out the next ones.

[Without a server](/efema/guides/without-a-server/) has everything else about
a bucket; this page is what changes when its credentials do not last.

## For one run

Temporary credentials are `Credentials` with their token:

```rust
use efema::{Bucket, Credentials};

let credentials = Credentials::new(key_id, secret).with_token(session_token);
let bucket = Bucket::new("https://notes.s3.eu-central-1.amazonaws.com", "eu-central-1", credentials)?;
```

They sign until the bucket stops taking them; nothing replaces them. That is
enough for a program that runs and ends - `mini-vault`, the
[example app](/efema/guides/a-synced-folder/), reads the token from
`AWS_SESSION_TOKEN`, next to the two variables it reads the key from:

```console
$ export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_SESSION_TOKEN=… AWS_REGION=us-east-1
$ export MINI_VAULT_PASSPHRASE='several random words, not these'
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./laptop --state ./laptop-state sync --once
applied 0 change(s), sent 3
```

A second device, with credentials handed out to it:

```console
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./desktop --state ./desktop-state sync --once
applied 3 change(s), sent 0
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./desktop --state ./desktop-state doctor
relay    http://127.0.0.1:9000/notes - answered in 17 ms
access   reads and writes - no quota
stream   vault - stream 0bff130c, epoch 1, 4 entries
cursor   at 4 - up to date
queue    empty
key      9963ee4ab3f50d4e - locked 2026-11-24 (today), argon2id 64 MiB × 3 × 4
device   2ec7266e
epoch    1 - written, and read up to
```

Once they have run out - or with a token the service does not know, or the
key and secret without their token:

```console
$ mini-vault --bucket http://127.0.0.1:9000/notes --folder ./desktop --state ./desktop-state sync --once
error: http://127.0.0.1:9000/notes does not take the access key: it was revoked, it ran out, or it was never handed out there
```

## For an app that keeps running

A device that syncs all day outlives its credentials a dozen times. Give the
bucket what hands them out instead of what was handed out - a
`CredentialsProvider`:

```rust
use std::time::{Duration, SystemTime};

use efema::{Bucket, Credentials, CredentialsProvider};

/// Credentials for an hour, from the app's own backend.
struct Backend { /* the app's session with its backend */ }

impl CredentialsProvider for Backend {
    async fn credentials(&self) -> Result<Credentials, Box<dyn std::error::Error + Send + Sync>> {
        let handed = self.ask_for_bucket_credentials().await?;
        Ok(Credentials::new(handed.key_id, handed.secret)
            .with_token(handed.session_token)
            .until(SystemTime::UNIX_EPOCH + Duration::from_secs(handed.expires_at)))
    }
}

let bucket = Bucket::new("https://notes.s3.eu-central-1.amazonaws.com", "eu-central-1", Backend { /* … */ })?;
```

The bucket - the same one, and every client opened over it - asks the
provider:

- **before its first request**;
- **before they run out.** Once the credentials it holds say they have less
  than five minutes left (`until`), it asks before each request until it is
  handed ones that last longer. While the ones it holds still last, a
  provider that fails does not fail the request: the backend has those five
  minutes to come back;
- **when the bucket refuses them** - run out without having said when,
  revoked, or a device's clock that is not the service's. Handed other
  credentials, the bucket signs the refused request with them and sends it
  once more; handed the same ones, it reports the refusal.

It asks one question at a time, however many requests wait for the answer,
and keeps what it is handed - a provider needs no cache of its own. A bucket
and its prefixes (`Bucket::within`) ask as one.

Where the credentials come from is the app's: its backend calling
`AssumeRole`, a web identity exchanged with STS, a token endpoint of the
service. What is handed out can be narrower than the role it comes from - a
session policy that reaches one
[prefix](/efema/guides/without-a-server/#under-a-prefix), or only reads.

## When there are none

| A device sees | What happened |
| --- | --- |
| `Error::Transport`, in it `TransportError::Credentials` | the provider failed, and nothing it handed out before still lasts; its own error is the source. Nothing was sent |
| `Error::Unauthorized` | the bucket refused the credentials, and the provider had no others |
| `Error::Unsent`, its source one of the two | a push did not get through; what was pushed waits on the device, sealed |

Neither loses anything: the cursor stays where it was, what was pushed goes
with the next `send` - as when
[the connection tears](/efema/guides/when-something-goes-wrong/#the-connection-tears).

## What crosses the network

The secret never does: a request carries a signature made with it. The
session token does, with every request - in the header S3 reads it from,
`x-amz-security-token`, covered by the signature, and nowhere else: not in
an address, not in an error, not in what `Debug` prints. It is worth what an
hour of the credentials is worth, and only together with the secret; use
`https://` to anything but the device itself, as efema already insists.

## How this is checked

Against a real STS: `AssumeRole` of MinIO, in CI on every push - two devices
syncing with temporary credentials while every byte to the bucket is
searched for the secret and for the token outside its header; credentials
the bucket refuses replaced on the same `Bucket`, with one refusal on the
wire; credentials a minute from running out replaced with none. And since
no STS hands credentials out for less than fifteen minutes, one test waits
them out: what ran out does not write, and both ways of getting the next
ones do.

## Related

- [Without a server](/efema/guides/without-a-server/) - a bucket in a relay's
  place.
- [A stream in a bucket](/efema/reference/bucket/#requests) - how each request
  is signed.
- [Threat model](/efema/concepts/threat-model/#in-a-bucket) - what a bucket's
  owner can and cannot do.
