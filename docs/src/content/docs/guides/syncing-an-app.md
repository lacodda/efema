---
title: Syncing an app
description: Use the efema client library in a Rust app - open a stream with an access key, push changes, pull and acknowledge, wait for news, keep working when the network breaks, change the passphrase, and handle what can go wrong.
---

The [`efema`](https://docs.rs/efema) crate is what an app links to. It seals
what the app pushes, opens what it pulls, keeps the device's cursor, and
reports on all of it. What the app's changes are, and how two of them merge,
stay with the app.

```console
$ cargo add efema
$ cargo add tokio --features rt-multi-thread,macros
```

## Open a stream

```rust
use efema::{AccessKey, Client, Epoch, Relay, Secret, State};

let state = State::open(data_dir.join("sync.sqlite3"))?;
let key: AccessKey = access_key.parse()?;
let relay = Relay::new("https://sync.example.com", key)?;
let notes = Client::open(
    relay,
    &state,
    "notes".parse()?,
    Epoch(1),
    Secret::Passphrase(passphrase.as_bytes()),
)
.await?;
```

- **`State`** is the device: its identity, and for each stream its cursor and
  the stream's locked key. One file per device, opened once, shared between
  the clients of all its streams. Only one process opens it at a time.
- **`Relay`** is where the stream lives, `https://` with a path if the relay
  is served under one, and the **access key** its operator handed out
  ([`keys`](/efema/reference/keys/)). The key opens one space of the relay;
  stream names are names in that space. HTTPS is checked against the
  platform's trust store. Plain `http://` is accepted only to this machine -
  `localhost`, `127.0.0.1`, `::1` - since the key goes with every request.
- **`Epoch(1)`** is the format of what this version of the app writes - see
  [Epochs](#epochs).
- **The secret is required.** The first device to open a stream creates its
  key and writes it to the stream, locked under the passphrase; every other
  device unlocks it with the same passphrase. After the first time, a device
  opens the stream without the network. ([Sealing](/efema/concepts/sealing/)
  has the details.)

An app that does not want to ask for the passphrase on every start can keep
the key - `notes.key().to_bytes()` - where it keeps its most private data, and
open with `Secret::Key(Key::from_bytes(bytes))` next time. Whoever holds those
32 bytes can read the stream.

## Push

```rust
let sent = notes.push([change_a.as_slice(), change_b.as_slice()]).await?;
// sent.items: how many went to the relay
```

Each item is compressed and sealed, and **kept on the device** - in its
`State` - before anything is sent. Then everything the device has waiting for
the stream goes, oldest first. Items that fit in one request go as one batch:
on consecutive positions, all or none. A push larger than that is cut into
batches that each fit, each whole on its own. An item too large for a request
of its own is refused before anything is kept - `notes.max_item_len()` says the
limit, which is the same whether the item compresses or not.

The limits are the relay's: it announces them for the device's key, and the
client keeps them in the `State` - so a push is cut without the network, and
a relay with lower limits than another is followed. If the operator lowers a
limit while batches wait on the device, the relay refuses the first of them,
and the client asks for the limits again, cuts what waits to them, in order,
and sends it: each item still lands once. With a key that only reads, `push`
is `Error::ReadOnly`, and nothing is kept.

When sending stops - no network, a relay that is down -
`Error::Unsent` says how many items wait on the device, and its source says
why. They are not lost: the next `push`, or `send`, takes them. See
[When the network breaks](#when-the-network-breaks).

### Compression

Each item is compressed with zstd before it is sealed, if that makes it
shorter - text usually goes as a third of itself; what is already compressed
goes as it is. Nothing to do on the reading side: entries come back as they
were pushed.

Turn it off for an app that puts a secret and text someone else chose into
one item - a password next to a web page's title, say:

```rust
use efema::Compression;

let vault = Client::open(relay, &state, "vault".parse()?, Epoch(1), secret)
    .await?
    .with_compression(Compression::Off);
```

Compressed, an entry's length depends on how much of the item repeats, and
the relay sees lengths - with text an attacker chose next to a secret, that
is enough to learn the secret a guess at a time. The
[threat model](/efema/concepts/threat-model/) has the details.

## Pull, apply, acknowledge

```rust
loop {
    let pulled = notes.pull().await?;
    for entry in &pulled.entries {
        app.apply(entry.seq, entry.device, &entry.data)?;
    }
    notes.ack(&pulled).await?;
    if !pulled.more() {
        break;
    }
}
```

`pull` reads one page after the device's cursor, checks every link of the
hash chain, and opens each entry. It does not move the cursor: `ack` does,
once the app has applied the page. A crash in between hands the same entries
over again - **delivery is at least once**, and an app applies an entry it has
seen before without harm.

Each entry says who wrote it (`device`) and whether that was this device
(`own`). **Apply your own entries too**, in their place in the order: two
devices can change the same thing before either hears from the other, and
only the stream's order says which change came last. Skipping your own entries
is the classic way two devices end up disagreeing.

## When the network breaks

A phone on a train, a laptop closing its lid, a relay restarting: sending
breaks, and what was being sent is in one of three places - not sent, sent and
not written, or written with the answer lost on the way back. The client
treats all three the same way, and none of them loses or doubles a change.

- **What is pushed waits on the device until the relay has it.** Sealed, in
  the state file, cut into the batches it will go in. Closing the app does not
  lose it; the next run sends it.
- **A batch goes again as it was, byte for byte.** The relay knows a batch by
  what it holds, so a batch it wrote already - its answer lost - is answered
  with where it landed, and not written a second time.
- **Sending resumes where it stopped.** Batches the relay took leave the
  device as they land; what is left goes next time, in order, before anything
  pushed since.
- **A torn read moves nothing.** The cursor moves only with `ack`, so the next
  `pull` reads from the last page the app applied - not from the start.

So the app's part is to send again, at a moment of its choosing:

```rust
match notes.push([change.as_slice()]).await {
    Ok(_) => {}
    // Kept on the device: show "waiting to sync", try again later.
    Err(efema::Error::Unsent { unsent, source }) => app.waiting(unsent, &source),
    Err(e) => return Err(e.into()),
}

// When the app starts, when the network is back, on a timer:
notes.send().await?;
```

`notes.queue()` says what waits - how many items, how many bytes, since when -
for a status line; the [doctor](#doctor) shows it too.

The client does not retry on its own: when to try again - at once, in a
minute, when the operating system says the network is back - is the app's to
decide. Waiting does no harm; nothing is lost while it waits.

If what waits cannot go by waiting - the stream is gone from the relay,
replaced, or moved to an epoch only a newer app writes; the space is full
(`QuotaExceeded`, until the operator gives it room); the key was revoked
(`Unauthorized`) or only reads (`ReadOnly`); an item is larger than the relay
takes now (`ItemTooLarge`) - the source of `Unsent` says so, and every later
send stops the same way. `State::forget(&stream)` lets the
device start over, and lets go of what waited with the rest; the app's own
data still has every change, and it sends what the stream lacks once it has
joined again (see [When the relay is not what it was](#when-the-relay-is-not-what-it-was)).

## Wait for news

```rust
if notes.wait(Duration::from_secs(30)).await? {
    // something new: pull
}
```

One request sleeps on the relay until the stream moves past the device's
acknowledged cursor, or the time runs out (at most a minute). Acknowledge
before waiting: a wait from an old cursor returns at once.

## Change the passphrase

```rust
notes.change_passphrase(new_passphrase.as_bytes()).await?;
```

The stream's key stays, and nothing in the stream is sealed again: the key is
locked under the new passphrase and the lock written to the stream. Every
device takes it in as it reads past it, and asks for the new passphrase from
its next start - opening with the old one is `Error::WrongPassphrase` there. A
device that joins later opens with the new passphrase. A device that opens
with the key itself (`Secret::Key`) is not affected.

If two devices change the passphrase at the same moment, the change that
reached the stream first holds on every device, and the other one gets
`Error::PassphraseRace`.

**This does not lock out whoever knew the old passphrase.** The old lock stays
in the stream - the stream only grows - so with the old passphrase and a copy
of the stream the key still opens. Change a passphrase that was weak, or one
the doctor says is locked with weaker parameters than today's. For a
passphrase that leaked, start a new stream and push the app's data into it.

## Epochs

An epoch is the version of the format the app writes. When a new version of
the app changes its format, it opens the stream with the next epoch:

- the newer app reads entries of every epoch up to its own, and knows which
  each one is (`entry.epoch`);
- an older app reads everything before the first newer entry, then stops with
  `Error::NewerEpoch`, its cursor in front of it - it tells its user to
  update, rather than misreading the newer format;
- an older app can no longer write: `Error::EpochBehind`.

## When the key is not what it was

| Error | What happened | What to do |
| --- | --- | --- |
| `Unauthorized` | the relay does not take the key: it was revoked, or never handed out there | ask the operator for a key, and open again with it |
| `ReadOnly` | the key reads the space and does not write to it | nothing for this device to send; a key that writes, if it should |
| `QuotaExceeded` | the space holds all it may | the operator gives it room; what was pushed waits on the device |

A refusal for going over the relay's rate - `rate_limited` in an
`Error::Transport` - says in how many seconds to ask again.

## When the relay is not what it was

Four errors say the relay's history is no longer the one this device read:

| Error | What happened |
| --- | --- |
| `StreamGone` | the relay has no such stream - its data was lost or wiped |
| `StreamReplaced` | the name belongs to a new stream now - wiped, then written again |
| `CursorAhead` | the relay has fewer entries than the device read - restored from an older backup |
| `CursorDiverged` | the relay's history went another way at the device's cursor |

In none of them does the client write anything: a device never sends a batch
into a stream other than the one it knows. `State::forget(&stream)` lets the
device start over - it joins the stream as the relay has it now, and reads it
from the beginning. What the app does with its own data then - send it again,
merge it with what it reads - is its call.

Two more say the relay sent something it should not have: `BrokenChain` (a
link that does not follow) and `Inauthentic` (an entry changed after it was
sealed, or a lock of the key the key did not seal). A pull delivers everything
before such an entry, and stops there.

## Doctor

```rust
println!("{}", notes.doctor().await);
```

```console
relay    http://127.0.0.1:2451 - answered in 15 ms
access   reads and writes - 47.8 KiB of 8.0 GiB in the space
stream   vault - stream aec8e1aa, epoch 1, 7 entries
cursor   at 5 - 2 entries to read
queue    empty
key      164324ac6cbe7a06 - locked 2026-10-14 (today), argon2id 64 MiB × 3 × 4
device   985605d0
epoch    1 - written, and read up to
```

One report, the same in every app: whether the relay answers, what the access
key may do and how full its space is, how far behind the device is, what waits
on it to be sent, which key the stream is sealed under (the same id on every
device), how old its lock is and how many times the passphrase was changed.
`report.problems()` lists what is wrong, one line each, with what to do about
it - items that have waited more than a day, a space nine tenths full, items
waiting with a key that only reads. Show it under your app's own `doctor`
command.

## Related

- [A synced folder](/efema/guides/a-synced-folder/) - a complete app on top of
  the library, with merging.
- [Sealing](/efema/concepts/sealing/) - the keys and the formats.
- [The API on docs.rs](https://docs.rs/efema) - every type and method.
