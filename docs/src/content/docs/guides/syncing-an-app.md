---
title: Syncing an app
description: Use the efema client library in a Rust app - open a stream, push changes, pull and acknowledge, wait for news, and handle what can go wrong.
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
use efema::{Client, Epoch, Relay, Secret, State};

let state = State::open(data_dir.join("sync.sqlite3"))?;
let relay = Relay::new("https://sync.example.com")?;
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
- **`Relay`** is where the stream lives, `http://` or `https://`, with a path if
  the relay is served under one. HTTPS is checked against the platform's
  trust store.
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
let pushed = notes.push([change_a.as_slice(), change_b.as_slice()]).await?;
// pushed.positions: where each one landed, in order
```

Each item is sealed and appended. Items that fit in one request land as one
batch: on consecutive positions, all or none. A push larger than that is cut
into batches that each fit; if one fails partway, `Error::Interrupted` says
how many items made it. An item too large for a request of its own is refused
before anything is sent - `notes.max_item_len()` says the limit.

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

## Wait for news

```rust
if notes.wait(Duration::from_secs(30)).await? {
    // something new: pull
}
```

One request sleeps on the relay until the stream moves past the device's
acknowledged cursor, or the time runs out (at most a minute). Acknowledge
before waiting: a wait from an old cursor returns at once.

## Epochs

An epoch is the version of the format the app writes. When a new version of
the app changes its format, it opens the stream with the next epoch:

- the newer app reads entries of every epoch up to its own, and knows which
  each one is (`entry.epoch`);
- an older app reads everything before the first newer entry, then stops with
  `Error::NewerEpoch`, its cursor in front of it - it tells its user to
  update, rather than misreading the newer format;
- an older app can no longer write: `Error::EpochBehind`.

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
sealed). A pull delivers everything before such an entry, and stops there.

## Doctor

```rust
println!("{}", notes.doctor().await);
```

```console
relay    http://127.0.0.1:2451 - answered in 2 ms
stream   vault - stream 650365e8, epoch 1, 7 entries
cursor   at 7 - up to date
key      c154b5aad4ec9958 - locked 2026-10-09 (today), argon2id 64 MiB × 3 × 4
device   234d67dc
epoch    1 - written, and read up to
```

One report, the same in every app: whether the relay answers, how far behind
the device is, which key the stream is sealed under (the same id on every
device) and how old its lock is. `report.problems()` lists what is wrong, one
line each, with what to do about it. Show it under your app's own `doctor`
command.

## Related

- [A synced folder](/efema/guides/a-synced-folder/) - a complete app on top of
  the library, with merging.
- [Sealing](/efema/concepts/sealing/) - the keys and the formats.
- [The API on docs.rs](https://docs.rs/efema) - every type and method.
