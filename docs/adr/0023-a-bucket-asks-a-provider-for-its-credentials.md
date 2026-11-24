# ADR 0023: A bucket asks a provider for its credentials

- Status: accepted
- Date: 2026-11-24

## Context

A `Bucket` took its credentials as a value, at construction: an access key's
id and its secret, held for good. That is one of two ways a device comes by
credentials to a bucket. The other is the careful one - the device is handed
credentials for an hour, by the service's STS or by the app's own backend:
an access key, its secret, a session token that goes with every request, and
a moment they run out. A key held for good on every phone is a key that is
taken back only by changing it everywhere; one handed out for an hour is
taken back by not handing out the next.

Signing with a session token is one more signed header
(`x-amz-security-token`). What a value in a constructor cannot do is change:
a device that syncs all day outlives its credentials a dozen times over, and
a `Bucket` that has to be made again for each of them takes every client
opened over it down with it - each holds its transport.

## Decision

**`Bucket::new` takes a `CredentialsProvider`** - one method, `credentials`,
that hands out what to sign with from now on - **and the bucket asks it**:

- before its first request;
- before a request, once the credentials it holds say they run out within
  five minutes (`Credentials::until`), and before every request after that
  until it is handed ones that last longer. While the ones held still last,
  a provider that fails does not fail the request;
- after a request refused because of the credentials themselves - an
  unknown, expired or revoked key or token, a signature that does not match;
  to `HEAD`, whose refusal has no body, any `401` or `403`. Handed other
  credentials - another key, secret or token - the bucket signs the same
  request with them and sends it once more. Handed the same ones, it reports
  the refusal as it came.

`Credentials` are a provider of themselves, so the bucket has one path:
credentials held for good are the provider that always hands out the same,
asked once, and refused - nothing replaces them - as before.

The bucket holds what it was handed, behind a lock that lets one question
through at a time: a provider needs no cache, and twenty requests waiting on
credentials about to run out ask once. A bucket and its prefixes
(`Bucket::within`) share the holder.

A provider that fails when nothing held still lasts is
`TransportError::Credentials`, with the provider's own error as its source -
an `Error::Transport` to a client, worth trying again later, and nothing was
sent.

**The second sending is not a retry of the kind the library does not do**
(ADR 0014, 0017). A request refused for its credentials was not looked at:
no object was created, no condition was checked. The bucket never saw the
batch, so there is nothing to send twice.

## Alternatives

- **An STS client in the library** (`AssumeRole` with a long-lived key).
  There are half a dozen ways credentials are handed out - `AssumeRole`,
  web identity, an R2 token endpoint, the app's own backend - and the device
  that could call `AssumeRole` holds the long-lived key the whole exercise
  is to keep off it. Where they come from is the app's; the library's part
  is asking at the right moments.
- **`Bucket::set_credentials`**, called by the app on a timer. The app would
  have to know when - the margin, the refusal - and every app would write
  the same loop, some of them wrong. A refusal mid-append would still fail.
- **Only the refusal, no expiry.** Simpler, and every hour one request of
  every device is refused before it works. Services count failed
  authentications, and a device that says when its credentials run out has
  no reason to find out by being refused.
- **Only the expiry, no refusal.** A device's clock is not the service's,
  credentials are revoked ahead of time, and the ones from an environment
  variable do not say when they run out at all.
- **A closure instead of a trait.** `Credentials` itself could not be one,
  and the bucket would have two ways in.

## Consequences

- `Bucket::new(url, region, credentials)` reads as before for a caller with
  `Credentials`; it now takes any provider.
- `Credentials` have a session token (`with_token`) and may say when they run
  out (`until`). `Debug` prints the key's id and that there is a token,
  never the token or the secret.
- The session token crosses the network with every request, in its header -
  that is how S3 reads it - and nowhere else; the secret still never does.
  The header is marked sensitive, so the HTTP client's own logging leaves it
  out.
- Checked against a real STS: MinIO's `AssumeRole`, in the CI job `bucket`
  on every push - and, since STS hands nothing out for less than fifteen
  minutes, in a job of its own that waits them out: credentials that ran out
  do not write, and both ways of getting the next ones do.
- A WebDAV transport to come has credentials of another kind; the provider
  is the bucket's, not the transport trait's.
