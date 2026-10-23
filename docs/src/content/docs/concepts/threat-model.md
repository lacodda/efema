---
title: Threat model
description: What an efema relay - or the bucket a stream is kept in - can see and do, who can reach it, what it cannot, which of that is shown by tests, and what this version does not protect yet.
---

A relay sits between your devices and holds everything they say to each other.
This page is the honest account of what that position allows. Its first
version was written before the code that seals entries, so the promise was on
record before the implementation. It now says what the code does - and
[which tests show it](#what-is-shown-not-claimed).

## What the relay sees

Whoever runs the relay - you, a friend, a hosting company - can see:

- **stream names**, which travel in the URL, and the **spaces** they are in;
- **which access key** made each request, and when each key was last used;
- for every entry: its **position**, **size**, **epoch** and **link**;
- **when** each batch was written and each read and wait happened;
- the **network address** of every device that talks to it;
- the stream's **key entries**: its key, locked under the passphrase - the
  first entry, and one more each time the passphrase changes - which anyone
  can try passphrases against (below).

Pick stream names accordingly. `notes` says little; `bank-passwords` says what
it says to anyone who can read the relay's database or its logs.

## Who can reach it

Nobody without an access key. Every request under `/v1` carries one, and a
key opens one **space** of the relay: its streams, and no others. The
operator hands keys out and revokes them; a revoked key opens nothing from its
next request on ([`keys`](/efema/reference/keys/)).

- **A key that reads** reads the space's streams and waits on them. It cannot
  append, so it cannot fill a stream, create one, or raise a stream's epoch to
  lock the other devices out.
- **A key that writes** does all of that, within the space.
- **Neither reaches another space.** A name there is another stream; a cursor
  or a stream identity from another space finds nothing.

The relay keeps a digest of each key, not the key, and logs neither. A key is
a bearer secret: whoever holds it is, to the relay, the device it was handed
out to.

**Only through HTTPS.** The relay has no TLS of its own, and answers only its
own machine and requests a trusted reverse proxy took over HTTPS. Plain HTTP
from another machine is refused before the key is looked at - and the client
library refuses to send a key over plain HTTP to anything but its own machine.

**Within limits.** Each key is held to a largest request and entry, a number of
requests a minute, and a quota of data for its space - set by the operator,
announced to the device. A stranger with a stolen key that writes can fill one
space to its quota, not the relay's disk.

## What it does not see

**The contents of entries.** The client library seals every entry on the
device, with XChaCha20-Poly1305 under a key that never leaves the devices
unlocked ([Sealing](/efema/concepts/sealing/)). The relay stores ciphertext,
and so does every backup of the relay. The library has no method that sends an
entry unsealed: a client cannot be made without the stream's passphrase or
key, and "without encryption" is not a mode efema has.

**Which device wrote what.** Each device has an identity, and it travels
inside the seal: the relay sees entries, not their authors.

**The key.** Devices keep the key locked under the passphrase; the relay has
only the lock.

## The passphrase is the key's last line

The lock is on the relay - so whoever holds the relay's database can try
passphrases against it, offline, as fast as their hardware runs Argon2id at
64 MiB and three passes. That cost per guess is the whole of the protection: a
short or common passphrase falls to a patient attacker, a long one does not.
Use several random words, not a word and a number.

A device's state file holds the same lock, and the same advice applies to
anyone who copies it.

Changing the passphrase locks the same key again, with today's Argon2id
parameters, and writes the new lock to the stream; every device moves to it as
it reads. It does not take the old lock away - the stream only grows - so
whoever knows the old passphrase and holds a copy of the stream can still
unlock the key. Change a passphrase that was weak; for one that leaked, start a
new stream.

## What this version does not protect yet

- **A leaked access key works until it is revoked.** Keys are not bound to a
  device or an address. A key that reads, in other hands, downloads the
  stream's ciphertext and its key locks - and with them, the offline guessing
  of the passphrase below. Revoke a key that leaked; it does not undo what was
  read with it.
- **What a revoked key wrote stays.** The stream only grows. Entries a stolen
  key that writes appended are refused by your devices - they do not open -
  but they are in the stream.
- **One proxy setup the relay cannot tell apart.** A reverse proxy on the
  relay's machine that forwards without any forwarding header looks exactly
  like a program on that machine. Caddy says how each request came; nginx has
  to be told to ([guide](/efema/guides/behind-a-reverse-proxy/)).
- **Floods without a key** are refused cheaply, not limited: the rate is per
  key. Holding back a flood from the network is the proxy's job.
- **Sizes and times.** Sealing hides what an entry says, not how long it is or
  when it was written. Items are compressed before they are sealed, so a
  length also says how much of an item repeats. Where one item mixes a secret
  with text someone else chose, and that someone can see the relay, lengths
  are a known way to learn the secret a guess at a time (the attacks on TLS
  called CRIME and BREACH). An app that builds items like that turns
  compression off.
- **No forward secrecy.** Whoever learns the key - or the passphrase and a
  lock - reads the whole stream, past entries included. A changed passphrase
  re-locks the key; it does not replace it.

Behind a reverse proxy that terminates TLS, the relay can be left reachable:
what a stranger without a key gets is a refusal. The operator still sees the
names, sizes and times of every stream - pick an operator, or be one.

## What a dishonest relay can do

Sealing keeps a relay from reading entries. It does not make the relay
trustworthy in every other way. A relay that wants to misbehave can:

- **withhold** entries - from everyone, or from some devices;
- **delay** them, or answer waits late;
- **drop** a stream, or the whole database;
- **show different histories to different devices.** The hash chain catches a
  history that changed under a cursor by accident, but a relay that lies on
  purpose can compute a chain for whatever history it likes.

It cannot read a sealed entry, and it cannot forge or change one: sealing
authenticates as well as encrypts, so an entry the relay made up or altered
fails to open on the device - and the client says which one, instead of
reading past it. It cannot move an entry to another epoch either: the epoch is
sealed in with the entry. Nor can it change the passphrase: a new lock counts
only under the key's own seal, and an old one written again counts for
nothing.

It can make joining slow: a device that joins tries the passphrase on the
locks in the stream, and each try costs what Argon2id costs - more for a lock
whose parameters the relay chose at the upper bound. That is one more way to
withhold the stream, not a way into it.

## What the relay protects against

Strangers on the network:

- **A request without a key, or with one revoked**: refused, under `/v1`, on
  every path.
- **Plain HTTP from another machine**: refused before anything else, whatever
  it carries.
- **A key used more than the operator allows**: refused with when to come
  back; a space past its quota takes nothing more.

And accidents, mostly - the kind that happen to honest operators:

- **A restore from an old backup**, or a disk that lost recent writes: cursors
  past the relay's head are refused with `cursor_ahead`, and cursors into a
  history that went another way with `cursor_diverged`.
- **A relay that was wiped and started again**: streams get new identities,
  old cursors are refused with `stream_replaced`, and a device that knew the
  old stream is refused when it writes, too - its next batch never becomes
  the first entry of a stream it did not start.
- **Two versions of an app writing different formats**: epochs keep an older
  version from mixing its entries in after a newer one moved the stream on.
- **A crash or a power cut mid-write**: a batch is committed whole, and an
  acknowledged write is on disk before the device hears about it.
- **A connection torn mid-write, or an answer lost on its way back**: a batch
  is read whole before any of it is written, and a batch sent again is known
  by what it holds and answered rather than written twice. The device keeps
  what it pushed - sealed - until the relay has it.
- **A relay that hands out a broken history**: the client recomputes every
  link as it reads, and stops at the first one that does not follow.

## In a bucket

A stream kept in an S3 bucket ([Without a server](/efema/guides/without-a-server/))
has no relay, and the bucket stands where the relay stood. Its owner - and the
service that keeps it - sees what a relay's operator sees: stream names and
identities in the keys, every batch's position, size and time, and the batch
keys of the offers. Requests are signed and carry no secret: the credentials'
secret never crosses the network. Entries are sealed as they are on a relay,
so nothing kept in the bucket can be read without the stream's key, and the
stream's key entries in it can be guessed at offline, like any copy of a
relay's database.

What the bucket's owner can do is what a dishonest relay can - withhold,
delay, delete, show a device an older listing - and one thing more: write
objects of their own into it. An object that is not a batch efema wrote is
refused when it is read, a batch whose metadata says other than its body is
not read, and an entry no client sealed does not open - but whoever can write
to the bucket can fill it, or delete what is in it, as anyone who can write to
a relay's disk can. Who that is, the bucket's own policies decide: there is no
access key per device to revoke, only the service's credentials and their
permissions. Credentials that only list and read are a device that only reads.

A bucket that does not honour `If-None-Match` - that lets one object be
created twice - would let two devices put two batches in one place. Before a
client writes, it checks: it creates one object twice, and writes nothing to a
bucket that lets the second creation through.

## What is shown, not claimed

Each promise on this page is held by a test that runs on every change, against
a real relay over a real socket - never a mock of one. Before every release
each property is also broken on purpose, one at a time, to see its test fail:
a test that stays green with the property gone proves nothing.

| Promise | Shown by |
| --- | --- |
| The relay's database, its write-ahead log and everything logged in the process at every level hold no item, passphrase, key or device identity in the clear - after every way a client writes: a new stream, pushes compressed and not, a changed passphrase, a second device joining and writing. Compression is off for the marker, so what hides it has to be the seal. A device's own state holds none of them either - with an item waiting in it to be sent. | `nothing_the_relay_keeps_or_logs_is_plaintext` ([crates/efema/tests/client.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/client.rs)) |
| The relay, run as an operator runs it, logs no entry's bytes - as text, in hex or as numbers - at its most verbose level, refusals and repeats included; nor an access key, known or not. | `the_relay_never_logs_what_it_keeps` ([crates/efema-server/tests/binary.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/binary.rs)) |
| The relay's database holds no access key - not its text, not its bytes - only a digest of it. | `the_database_keeps_no_key` ([crates/efema-server/src/store.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/src/store.rs)) |
| Without a key the relay answers nothing under `/v1` - no key, a wrong one, a malformed header - and a revoked key opens nothing from its next request on, revoked by another process while the relay runs. | `without_a_key_the_relay_answers_nothing_under_v1`, `a_revoked_key_opens_nothing_from_its_next_request` ([crates/efema-server/tests/access.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/access.rs)), `keys_are_handed_out_listed_and_revoked_while_the_relay_runs` ([crates/efema-server/tests/binary.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/binary.rs)) |
| A key that reads writes nothing - no entry, no stream, no epoch - and a device with one keeps nothing to send. | `a_key_that_reads_does_not_write` ([crates/efema-server/tests/access.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/access.rs)), `a_device_with_a_key_that_reads_pulls_and_keeps_nothing_to_send`, `a_key_that_reads_cannot_create_a_stream` ([crates/efema/tests/access.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/access.rs)) |
| A space is closed to the keys of another: not its streams, not its cursors, not its waits. | `a_space_is_closed_to_the_keys_of_another` ([crates/efema-server/tests/access.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/access.rs)), `a_stream_of_another_space_is_another_stream` ([crates/efema/tests/access.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/access.rs)) |
| Plain HTTP from another machine is refused - the built relay on every interface, called at the machine's network address - and so is a proxy that let plain HTTP through or did not say. | `plain_http_from_another_machine_is_refused` ([crates/efema-server/tests/binary.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/binary.rs)), `plain_http_from_outside_the_door_is_refused_before_anything_else`, `a_relay_answers_only_peers_it_trusts` ([crates/efema-server/tests/access.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/access.rs)) |
| Each limit holds - request, entry, quota of a space, requests a minute - and says which it was. | `a_batch_over_the_operators_limits_is_refused_with_the_limit`, `a_full_space_refuses_new_batches_and_no_other_space_feels_it`, `a_key_that_asks_too_often_is_told_when_to_ask_again` ([crates/efema-server/tests/access.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/access.rs)) |
| A batch the relay has is answered whatever the limits are now, and a queue cut for limits since lowered is cut again and lands once, in order. | `a_batch_written_before_the_limits_came_down_is_still_answered` ([crates/efema-server/tests/access.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/access.rs)), `a_queue_cut_to_limits_since_lowered_is_cut_again_and_lands_once` ([crates/efema/tests/access.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/access.rs)) |
| The metrics hold counts and times, no stream name and no entry, and answer only the relay's own machine. | `metrics_count_without_contents_and_only_on_this_machine` ([crates/efema-server/tests/access.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/access.rs)) |
| An altered entry is refused, whether the relay leaves the chain as it was or computes it anew. | `a_relay_cannot_alter_an_entry_unnoticed` ([crates/efema/tests/client.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/client.rs)) |
| An entry no client wrote is refused, not skipped. | `an_entry_no_client_wrote_is_refused_not_skipped` ([crates/efema/tests/client.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/client.rs)) |
| A relabelled epoch does not open. | `an_entry_opens_in_its_epoch_only` ([crates/efema/src/envelope.rs](https://github.com/lacodda/efema/blob/main/crates/efema/src/envelope.rs)) |
| A changed passphrase holds on every device, and a joining device opens with it. | `a_changed_passphrase_holds_on_every_device` ([crates/efema/tests/client.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/client.rs)) |
| A lock the key did not seal is refused, and an old one written again moves no device back. | `a_lock_the_key_did_not_seal_moves_nothing` ([crates/efema/tests/client.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/client.rs)) |
| A connection on which nothing moves is given up on after `STALL` - a push, a pull and a wait alike - and what was pushed lands once when the network moves again; a slow connection that keeps moving is never cut, a push and a pull each taking longer than `STALL`, and neither is a wait the relay holds for longer. | `a_connection_gone_silent_is_given_up_and_what_waited_lands_once`, `a_slow_connection_that_keeps_moving_is_never_cut`, `a_wait_longer_than_stall_is_the_relay_working_not_a_stall` ([crates/efema/tests/torn.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/torn.rs)), `a_request_that_keeps_moving_is_never_taken_for_stalled` ([crates/efema/src/stall.rs](https://github.com/lacodda/efema/blob/main/crates/efema/src/stall.rs)) |
| A batch torn on its way to the relay is not written, not even in part, and goes again whole. | `a_batch_torn_on_its_way_up_is_not_written_and_goes_again_whole` ([crates/efema/tests/torn.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/torn.rs)), `a_batch_torn_on_its_way_in_writes_nothing` ([crates/efema-server/tests/relay.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/relay.rs)) |
| A batch whose answer was lost is not written twice when it is sent again - nor when it is sent many times at once. | `an_answer_lost_on_its_way_back_does_not_write_the_batch_twice` ([crates/efema/tests/torn.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/torn.rs)), `the_same_batch_sent_at_once_lands_once` ([crates/efema-server/tests/relay.rs](https://github.com/lacodda/efema/blob/main/crates/efema-server/tests/relay.rs)) |
| What a device pushed outlives the network going and the app closing, and goes once both are back. | `what_waits_outlives_the_app_closing` ([crates/efema/tests/torn.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/torn.rs)) |
| A push torn partway resumes where it stopped, and a torn read from the acknowledged cursor. | `a_long_push_resumes_where_it_stopped`, `a_pull_torn_partway_resumes_from_the_acknowledged_cursor` ([crates/efema/tests/torn.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/torn.rs)) |
| A compressed frame that claims more than any item may be is refused before it is expanded. | `a_frame_that_lies_about_its_size_is_refused` ([crates/efema/src/envelope.rs](https://github.com/lacodda/efema/blob/main/crates/efema/src/envelope.rs)) |
| Through a bucket, what crosses the network - and so all the bucket can keep - holds no item, passphrase or key in the clear, nor the credentials' secret. | `nothing_that_reaches_the_bucket_is_plaintext` ([crates/efema/tests/bucket.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/bucket.rs)) |
| Writers at once, each with no memory of the others, put every batch in a bucket whole, on consecutive positions, in one order every reader reads; devices starting one stream at once start one. | `writers_at_once_land_whole_batches_in_one_order`, `devices_creating_one_stream_at_once_share_it` ([crates/efema/tests/bucket.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/bucket.rs)) |
| A batch sent to a bucket again - after any of its answers was lost, from a device that remembers nothing, or from several places at once among other writers - lands once. | `whichever_answer_is_lost_a_batch_sent_again_lands_once`, `a_batch_sent_again_lands_once`, `a_batch_sent_from_several_places_at_once_lands_once` ([crates/efema/tests/bucket.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/bucket.rs)) |
| A bucket that lets an object be created twice is not written to; one whose metadata says other than what it holds is not read; credentials that only read write nothing and keep nothing to send. | `a_bucket_that_lets_an_object_be_created_twice_is_not_written_to`, `a_bucket_that_says_one_thing_and_holds_another_is_not_believed`, `credentials_that_only_read_pull_and_keep_nothing_to_send` ([crates/efema/tests/bucket.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/bucket.rs)) |
| The formats are the ones on the [Sealing](/efema/concepts/sealing/) page, frozen against bytes computed outside the code. | `the_formats_are_frozen` ([crates/lacodda-seal/src/lib.rs](https://github.com/lacodda/efema/blob/main/crates/lacodda-seal/src/lib.rs)), `the_sealing_page_gives_the_formats_the_code_writes` ([crates/efema/tests/sealing_page.rs](https://github.com/lacodda/efema/blob/main/crates/efema/tests/sealing_page.rs)) |

The bucket's tests run against a real S3 - MinIO, in CI on every change -
never a mock of one; the one service they cannot have for real, one that
ignores the condition, is a stand that answers every `PUT` with success.
Connections in these tests are torn by a proxy between the client and the
relay ([lacodda-tear](https://crates.io/crates/lacodda-tear)) - partway up a
request, before or partway down an answer, or all at once - and the relay
then asked around it what it holds. What no test here shows: that an
acknowledged write survives a power cut (SQLite's `synchronous=FULL` is set,
on the relay and in the device's state; showing it takes pulling a plug), and
a real mobile network - that is a criterion of 1.0, not a test.
