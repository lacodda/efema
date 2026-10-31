//! The client's promises, against a real relay over a real socket.
//!
//! Each device is a state file of its own; where a relay has to misbehave,
//! a transport stands between the client and the real relay and changes what
//! the relay said - the relay itself is never replaced by a mock.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{
    Between, Device, NEW_PASSPHRASE, PASSPHRASE, TestRelay, contains, data, drain, name, relay_bytes, unsent,
};
use efema::{Compression, ENTRY_OVERHEAD, Epoch, Error, Key, Limits, Secret, Transport, chain};
use tokio::sync::Barrier;

#[tokio::test(flavor = "multi_thread")]
async fn two_devices_sync_through_the_relay() {
    let relay = TestRelay::start().await;
    let (laptop, phone) = (Device::new(), Device::new());
    let on_laptop = laptop.open(relay.transport(), "notes", 1).await;
    let sent = on_laptop.push([b"first".as_slice(), b"second"]).await.unwrap();
    assert_eq!((sent.items, sent.batches), (2, 1));
    assert!(on_laptop.queue().await.unwrap().is_empty());

    let on_phone = phone.open(relay.transport(), "notes", 1).await;
    let received = drain(&on_phone).await;
    assert_eq!(data(&received), [b"first".as_slice(), b"second"]);
    assert!(received.iter().all(|r| !r.own && r.device == laptop.state.device()));
    assert_eq!(on_phone.key().id(), on_laptop.key().id());

    on_phone.push([b"from the phone"]).await.unwrap();
    let received = drain(&on_laptop).await;
    assert_eq!(data(&received), [b"first".as_slice(), b"second", b"from the phone"]);
    assert_eq!(received.iter().map(|r| r.own).collect::<Vec<_>>(), [true, true, false]);
    assert_eq!(received[2].device, phone.state.device());
}

/// The relay cannot read what it keeps - shown, not claimed: every way a
/// client writes goes through a real relay, and then its database, its
/// write-ahead log and everything logged in the process at every level are
/// searched for what the relay must never have.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_the_relay_keeps_or_logs_is_plaintext() {
    let log = common::log();
    let relay = TestRelay::start().await;
    let (laptop, phone) = (Device::new(), Device::new());
    // Compression off: what hides the marker has to be the seal, not zstd.
    let on_laptop = laptop.open(relay.transport(), "notes", 1).await.with_compression(Compression::Off);
    let marker = b"PLAINTEXT-MARKER-the-relay-must-never-see";
    on_laptop.push([marker.as_slice(), marker.as_slice()]).await.unwrap();
    let mut long = marker.repeat(100);
    long.extend_from_slice(b"and a tail that does not repeat");
    on_laptop.push([&long]).await.unwrap();
    on_laptop.with_compression(Compression::Zstd).push([&long]).await.unwrap();
    let on_laptop = laptop.try_open(relay.transport(), "notes", 1, Secret::Passphrase(PASSPHRASE)).await.unwrap();
    on_laptop.change_passphrase(NEW_PASSPHRASE).await.unwrap();

    let on_phone = phone.try_open(relay.transport(), "notes", 1, Secret::Passphrase(NEW_PASSPHRASE)).await.unwrap();
    assert_eq!(drain(&on_phone).await.len(), 4);
    on_phone.push([marker]).await.unwrap();
    assert!(on_laptop.wait(Duration::from_secs(5)).await.unwrap());
    drain(&on_laptop).await;

    let key = on_laptop.key().to_bytes();
    let hex_key: String = key.iter().map(|b| format!("{b:02x}")).collect();
    let devices = [laptop.state.device(), phone.state.device()];
    let url = relay.url.clone();
    let relay_key = relay.key.clone();
    let dir = relay.stop().await;

    // With the relay gone, a push waits on the device - sealed there too.
    let offline = efema::Relay::new(&url, relay_key.clone()).unwrap();
    let waiting = laptop.try_open(offline, "notes", 1, Secret::Passphrase(NEW_PASSPHRASE)).await.unwrap();
    let (kept, _) = unsent(waiting.with_compression(Compression::Off).push([marker]).await);
    assert_eq!(kept, 1);

    let stored = relay_bytes(dir.path());
    assert!(stored.len() > long.len(), "the relay's files were not found");
    // The access key travels with every request; the relay keeps its digest.
    let access = relay_key.expose();
    let access = access.trim_start_matches(efema::ACCESS_KEY_PREFIX);
    let secrets: [(&str, &[u8]); 5] = [
        ("an item", marker),
        ("the passphrase", PASSPHRASE),
        ("the new passphrase", NEW_PASSPHRASE),
        ("the key", key.as_slice()),
        ("the access key", access.as_bytes()),
    ];
    for (what, secret) in secrets {
        assert!(!contains(&stored, secret), "the relay stored {what} in the clear");
    }
    for device in devices {
        assert!(!contains(&stored, device.as_bytes()), "the relay can see which device wrote");
    }

    let log = log.lock().unwrap().clone();
    assert!(contains(&log, b"notes: wrote"), "the relay's log was not captured");
    for (what, secret) in secrets.into_iter().chain([("the key in hex", hex_key.as_bytes())]) {
        assert!(!contains(&log, secret), "{what} was logged");
    }

    // Nor does a device's own state hold an item, the key or the passphrase:
    // only the key, locked, and what waits to be sent, sealed.
    for device in [&laptop, &phone] {
        let state = relay_bytes(device.dir.path());
        assert!(state.len() > 1000, "the state files were not found");
        for (what, secret) in secrets {
            assert!(!contains(&state, secret), "the state holds {what}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_opens_only_with_its_secret() {
    let relay = TestRelay::start().await;
    let first = Device::new();
    let client = first.open(relay.transport(), "notes", 1).await;
    let key = client.key().clone();

    let stranger = Device::new();
    let wrong = stranger.try_open(relay.transport(), "notes", 1, Secret::Passphrase(b"wrong")).await;
    assert!(matches!(wrong, Err(Error::WrongPassphrase { .. })), "{wrong:?}");

    let other_key = Key::generate().unwrap();
    let mismatch = stranger.try_open(relay.transport(), "notes", 1, Secret::Key(other_key)).await;
    assert!(matches!(mismatch, Err(Error::KeyMismatch { .. })), "{mismatch:?}");

    // The key itself opens the stream on another device - how a key kept in
    // a keyring, or shown as a QR code, is used.
    let with_key = stranger.try_open(relay.transport(), "notes", 1, Secret::Key(key)).await.unwrap();
    client.push([b"hello"]).await.unwrap();
    assert_eq!(data(&drain(&with_key).await), [b"hello".as_slice()]);

    // A key cannot create a stream: the other devices would have no lock to
    // unlock it with.
    let fresh = Device::new();
    let refused = fresh.try_open(relay.transport(), "empty", 1, Secret::Key(Key::generate().unwrap())).await;
    assert!(matches!(refused, Err(Error::NoStreamKey { .. })), "{refused:?}");
    assert!(relay.transport().read(&name("empty"), None, 10).await.is_err(), "a refused open created a stream");
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_creating_a_stream_at_once_agree_on_one_key() {
    let relay = TestRelay::start().await;
    let devices: Vec<Device> = (0..4).map(|_| Device::new()).collect();
    let devices = Arc::new(devices);
    let mut tasks = Vec::new();
    for index in 0..devices.len() {
        let devices = devices.clone();
        let transport = relay.transport();
        tasks.push(tokio::spawn(async move {
            let client = devices[index].open(transport, "notes", 1).await;
            client.push([format!("from {index}").into_bytes()]).await.unwrap();
            client.key().id()
        }));
    }
    let mut keys = Vec::new();
    for task in tasks {
        keys.push(task.await.unwrap());
    }
    assert!(keys.windows(2).all(|pair| pair[0] == pair[1]), "devices ended up with different keys: {keys:?}");

    // And everyone reads everyone.
    let reader = Device::new();
    let client = reader.open(relay.transport(), "notes", 1).await;
    let mut seen: Vec<String> =
        drain(&client).await.iter().map(|r| String::from_utf8(r.data.clone()).unwrap()).collect();
    seen.sort();
    assert_eq!(seen, ["from 0", "from 1", "from 2", "from 3"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_joined_opens_without_the_network() {
    let relay = TestRelay::start().await;
    let device = Device::new();
    device.open(relay.transport(), "notes", 1).await;
    let gone = relay.url.clone();
    let relay_key = relay.key.clone();
    relay.stop().await;

    let device = device.restart();
    let offline = efema::Relay::new(&gone, relay_key.clone()).unwrap();
    let client = device.try_open(offline, "notes", 1, Secret::Passphrase(PASSPHRASE)).await.unwrap();
    let wrong =
        device.try_open(efema::Relay::new(&gone, relay_key).unwrap(), "notes", 1, Secret::Passphrase(b"nope")).await;
    assert!(matches!(wrong, Err(Error::WrongPassphrase { .. })), "{wrong:?}");

    // Kept on the device, and not sent: the relay cannot be reached.
    let (unsent, unreachable) = unsent(client.push([b"x"]).await);
    assert_eq!(unsent, 1);
    assert!(matches!(unreachable, Error::Transport(efema::TransportError::Unreachable { .. })), "{unreachable:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cursor_survives_a_restart() {
    let relay = TestRelay::start().await;
    let writer = Device::new();
    let on_writer = writer.open(relay.transport(), "notes", 1).await;
    on_writer.push([b"one", b"two"]).await.unwrap();

    let reader = Device::new();
    let client = reader.open(relay.transport(), "notes", 1).await;
    assert_eq!(drain(&client).await.len(), 2);
    drop(client);

    let reader = reader.restart();
    on_writer.push([b"three"]).await.unwrap();
    let client = reader.open(relay.transport(), "notes", 1).await;
    assert_eq!(data(&drain(&client).await), [b"three".as_slice()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn what_was_not_acknowledged_comes_again() {
    let relay = TestRelay::start().await;
    let device = Device::new();
    let client = device.open(relay.transport(), "notes", 1).await;
    client.push([b"one", b"two"]).await.unwrap();

    let pulled = client.pull().await.unwrap();
    assert_eq!(pulled.entries.len(), 2);
    // No ack - the app crashed while applying.
    drop(client);
    let device = device.restart();
    let client = device.open(relay.transport(), "notes", 1).await;
    let again = client.pull().await.unwrap();
    assert_eq!(again.entries, pulled.entries);

    client.ack(&again).await.unwrap();
    assert!(client.pull().await.unwrap().entries.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_app_stops_before_a_newer_epoch_and_cannot_write_behind_it() {
    let relay = TestRelay::start().await;
    let (old, new) = (Device::new(), Device::new());
    let on_old = old.open(relay.transport(), "notes", 1).await;
    on_old.push([b"v1 a", b"v1 b"]).await.unwrap();

    // A newer version of the app, on another device, writes in epoch 2.
    let on_new = new.open(relay.transport(), "notes", 2).await;
    on_new.push([b"v2 a"]).await.unwrap();
    on_old.push([b"v1 c"]).await.unwrap_err();

    // The old app reads everything before the newer entry...
    let pulled = on_old.pull().await.unwrap();
    assert_eq!(data(&pulled.entries), [b"v1 a".as_slice(), b"v1 b"]);
    assert!(pulled.more());
    on_old.ack(&pulled).await.unwrap();
    // ...and stops in front of it, with its cursor there.
    let stopped = on_old.pull().await.unwrap_err();
    assert!(matches!(stopped, Error::NewerEpoch { seq: 4, ours: Epoch(1), entry: Epoch(2), .. }), "{stopped:?}");
    assert_eq!(on_old.cursor().await.unwrap().seq, 3);
    // Both of the old app's pushes wait on its device: the stream will not
    // take them, and says why.
    let (waiting, refused) = unsent(on_old.push([b"v1 d"]).await);
    assert_eq!(waiting, 2);
    assert!(matches!(refused, Error::EpochBehind { ours: Epoch(1), current: Epoch(2), .. }), "{refused:?}");

    // The newer app reads both epochs, and knows which is which.
    let received = drain(&on_new).await;
    assert_eq!(data(&received), [b"v1 a".as_slice(), b"v1 b", b"v2 a"]);
    assert_eq!(received.iter().map(|r| r.epoch.0).collect::<Vec<_>>(), [1, 1, 2]);
}

/// A newer release on another device may write a part of the format in a
/// version this one does not read: the entry's format or codec, the seal, a
/// lock, or the seal over a lock. Whichever it is, a device reads up to it,
/// stops in front of it, and says which part and which version - it never
/// reads one part as another.
#[tokio::test(flavor = "multi_thread")]
async fn a_part_newer_than_this_release_stops_a_device_in_front_of_it() {
    let relay = TestRelay::start().await;
    let context = [b"efema/entry/v1".as_slice(), &1u32.to_be_bytes()].concat();
    let cases: [(&str, efema::Part, u32); 5] = [
        ("entry", efema::Part::Entry, 3),
        ("codec", efema::Part::Codec, 9),
        ("seal", efema::Part::Seal, 2),
        ("lock", efema::Part::Lock, 2),
        ("lock-seal", efema::Part::Seal, 2),
    ];
    for (stream, part, version) in cases {
        let writer = Device::new();
        let on_writer = writer.open(relay.transport(), stream, 1).await;
        on_writer.push([b"before it"]).await.unwrap();
        let key = on_writer.key();
        let device = writer.state.device();
        let inner = |format: u8, codec: u8| [&[format][..], device.as_bytes(), &[codec], b"an item"].concat();
        let lock = on_writer.locked_key().to_bytes();
        let generation = key.seal(&[b"efema/key-entry/v1".as_slice(), &lock].concat(), &7u32.to_be_bytes()).unwrap();
        let newer = match stream {
            "entry" => key.seal(&context, &inner(3, 0)).unwrap(),
            "codec" => key.seal(&context, &inner(2, 9)).unwrap(),
            "seal" => {
                let mut sealed = key.seal(&context, &inner(2, 0)).unwrap();
                sealed[1] = 2;
                sealed
            }
            "lock" => {
                let mut entry = [lock.clone(), generation].concat();
                entry[1] = 2;
                entry
            }
            _ => {
                let mut seal = generation;
                seal[1] = 2;
                [lock.clone(), seal].concat()
            }
        };
        let batch = efema::wire::Batch {
            epoch: Epoch(1),
            entries: vec![newer],
            stream: Some(on_writer.stream_id()),
            first: None,
        };
        relay.transport().append(&name(stream), &batch).await.unwrap();

        let reader = Device::new();
        let on_reader = reader.open(relay.transport(), stream, 1).await;
        let pulled = on_reader.pull().await.unwrap();
        assert_eq!(data(&pulled.entries), [b"before it".as_slice()], "{stream}");
        on_reader.ack(&pulled).await.unwrap();
        let stopped = on_reader.pull().await.unwrap_err();
        let Error::NewerFormat { seq: Some(3), part: named, version: found, .. } = &stopped else {
            panic!("{stream}: {stopped:?}");
        };
        assert_eq!((*named, *found), (part, version), "{stream}");
        assert!(stopped.to_string().contains(&format!("is in {part} {version}, newer")), "{stopped}");
        assert_eq!(on_reader.cursor().await.unwrap().seq, 2, "{stream}: the cursor moved past it");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_is_cut_to_what_one_request_may_carry() {
    let relay = TestRelay::start().await;
    let device = Device::new();
    let mut between = Between::new(relay.transport());
    between.limits = Some(Limits { body: 1000, ..Limits::DEFAULT });
    let appends = between.appends.clone();
    let client = device.open(between, "notes", 1).await;
    let opened = appends.load(Ordering::SeqCst);

    let items: Vec<Vec<u8>> = (0..20u8).map(|i| vec![i; 150]).collect();
    let sent = client.push(&items).await.unwrap();
    let batches = appends.load(Ordering::SeqCst) - opened;
    assert!(batches > 1, "twenty items of 150 bytes went in one request of at most 1000");
    assert_eq!((sent.items, sent.batches), (20, batches));
    assert_eq!(data(&drain(&client).await), items.iter().map(Vec::as_slice).collect::<Vec<_>>());

    let largest = client.max_item_len().await.unwrap();
    assert!(largest < 1000 && largest > 800, "{largest}");
    client.push([vec![0u8; largest]]).await.unwrap();
    let sent = appends.load(Ordering::SeqCst);
    let refused = client.push([vec![1u8; 10], vec![0u8; largest + 1]]).await.unwrap_err();
    assert!(matches!(refused, Error::ItemTooLarge { .. }), "{refused:?}");
    assert_eq!(appends.load(Ordering::SeqCst), sent, "part of a push that could not fit was sent");
    assert!(client.queue().await.unwrap().is_empty(), "part of a push that could not fit was kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_does_not_fit_the_batch_leaves_it_waiting() {
    let relay = TestRelay::start().await;
    let device = Device::new();
    let mut between = Between::new(relay.transport());
    // 0: the truth; 1: one position for two entries; 2: another stream.
    let lie = Arc::new(AtomicUsize::new(0));
    let lying = lie.clone();
    between.answer = Some(Arc::new(move |written: &mut efema::wire::Written| match lying.load(Ordering::SeqCst) {
        1 => written.last = written.first,
        2 => written.stream = efema::StreamId::from_bytes([9; 16]),
        _ => {}
    }));
    let client = device.open(between, "notes", 1).await;

    lie.store(1, Ordering::SeqCst);
    let (waiting, refused) = unsent(client.push([b"a".as_slice(), b"b"]).await);
    assert_eq!(waiting, 2);
    assert!(matches!(refused, Error::Transport(efema::TransportError::NotProtocol { .. })), "{refused:?}");
    lie.store(2, Ordering::SeqCst);
    let (waiting, refused) = unsent(client.send().await);
    assert_eq!(waiting, 2);
    assert!(matches!(refused, Error::StreamReplaced { .. }), "{refused:?}");

    // Still the batch the relay has: sent again, it is known, and lands once.
    lie.store(0, Ordering::SeqCst);
    assert_eq!(client.send().await.unwrap().items, 2);
    assert_eq!(data(&drain(&client).await), [b"a".as_slice(), b"b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_lost_its_data_is_not_written_into() {
    let relay = TestRelay::start().await;
    let device = Device::new();
    let client = device.open(relay.transport(), "notes", 1).await;
    client.push([b"before"]).await.unwrap();

    // Lost, as a relay whose disk was wiped: the same device meets a relay
    // with an empty directory. (Deleting the old one instead races Windows,
    // which can hold a file just closed for a moment longer.)
    drop(relay.stop().await);
    let relay = TestRelay::start().await;
    let client = device.open(relay.transport(), "notes", 1).await;
    assert!(matches!(unsent(client.push([b"after"]).await), (1, Error::StreamGone { .. })));
    assert!(matches!(client.pull().await, Err(Error::StreamGone { .. })));
    assert!(relay.transport().read(&name("notes"), None, 10).await.is_err(), "the push created a stream");

    // Another device starts the stream again: this device's stream is not
    // that one, and it says so instead of mixing into it.
    let other = Device::new();
    other.open(relay.transport(), "notes", 1).await.push([b"anew"]).await.unwrap();
    assert!(matches!(unsent(client.push([b"after"]).await), (2, Error::StreamReplaced { .. })));
    assert!(matches!(client.pull().await, Err(Error::StreamReplaced { .. })));

    // Forgetting the stream lets the device join the new one - and lets go
    // of what waited for the old.
    device.state.forget(&name("notes")).unwrap();
    let client = device.open(relay.transport(), "notes", 1).await;
    assert!(client.queue().await.unwrap().is_empty());
    assert_eq!(data(&drain(&client).await), [b"anew".as_slice()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_restored_from_an_old_backup_is_named() {
    let relay = TestRelay::start().await;
    let device = Device::new();
    let client = device.open(relay.transport(), "notes", 1).await;
    client.push([b"one"]).await.unwrap();
    let dir = relay.stop().await;
    let backup = tempfile::tempdir().unwrap();
    for file in std::fs::read_dir(dir.path()).unwrap() {
        let file = file.unwrap();
        std::fs::copy(file.path(), backup.path().join(file.file_name())).unwrap();
    }

    let relay = TestRelay::start_in(dir).await;
    let client = device.open(relay.transport(), "notes", 1).await;
    client.push([b"two".as_slice(), b"three"]).await.unwrap();
    drain(&client).await;
    relay.stop().await;

    // Restored: the relay has lost "two" and "three".
    let relay = TestRelay::start_in(backup).await;
    let client = device.open(relay.transport(), "notes", 1).await;
    let ahead = client.pull().await.unwrap_err();
    assert!(matches!(ahead, Error::CursorAhead { cursor: 4, head: 2, .. }), "{ahead:?}");
    let report = client.doctor().await;
    assert!(report.problems().iter().any(|p| p.contains("State::forget")), "{report}");

    // Someone else writes on: the relay reaches position 4 again, with
    // another history.
    let other = Device::new();
    other.open(relay.transport(), "notes", 1).await.push([b"x", b"y"]).await.unwrap();
    let diverged = client.pull().await.unwrap_err();
    assert!(matches!(diverged, Error::CursorDiverged { seq: 4, .. }), "{diverged:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_cannot_alter_an_entry_unnoticed() {
    let relay = TestRelay::start().await;
    let writer = Device::new();
    writer.open(relay.transport(), "notes", 1).await.push([b"good".as_slice(), b"to be altered"]).await.unwrap();

    // Altered on the way, links left as they were: the chain gives it away.
    let mut careless = Between::new(relay.transport());
    careless.tamper = Some(Arc::new(|_, page| {
        if let Some(entry) = page.entries.iter_mut().find(|e| e.seq == 3) {
            let last = entry.data.len() - 1;
            entry.data[last] ^= 1;
        }
    }));
    let reader = Device::new();
    let client = reader.open(careless, "notes", 1).await;
    let pulled = client.pull().await.unwrap();
    assert_eq!(data(&pulled.entries), [b"good".as_slice()], "what came before the altered entry is delivered");
    client.ack(&pulled).await.unwrap();
    let broken = client.pull().await.unwrap_err();
    assert!(matches!(broken, Error::BrokenChain { seq: 3, .. }), "{broken:?}");

    // Altered with the chain computed anew, as a dishonest relay can: the
    // seal gives it away.
    let mut careful = Between::new(relay.transport());
    careful.tamper = Some(Arc::new(|after, page| {
        let mut previous = after.map_or_else(|| chain::genesis(&page.stream), |cursor| cursor.hash);
        for entry in &mut page.entries {
            if entry.seq == 3 {
                let last = entry.data.len() - 1;
                entry.data[last] ^= 1;
            }
            entry.hash = chain::link(&previous, entry.seq, entry.epoch, &entry.data);
            previous = entry.hash;
        }
    }));
    let reader = Device::new();
    let client = reader.open(careful, "notes", 1).await;
    let pulled = client.pull().await.unwrap();
    client.ack(&pulled).await.unwrap();
    let forged = client.pull().await.unwrap_err();
    assert!(matches!(forged, Error::Inauthentic { seq: 3, .. }), "{forged:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_entry_no_client_wrote_is_refused_not_skipped() {
    let relay = TestRelay::start().await;
    let device = Device::new();
    let client = device.open(relay.transport(), "notes", 1).await;
    client.push([b"sealed"]).await.unwrap();
    // Something that speaks the protocol but does not seal writes into the
    // stream: the relay takes it, the client does not.
    let plain =
        efema::wire::Batch { epoch: Epoch(1), entries: vec![b"plain text".to_vec()], stream: None, first: None };
    relay.transport().append(&name("notes"), &plain).await.unwrap();

    let pulled = client.pull().await.unwrap();
    assert_eq!(data(&pulled.entries), [b"sealed".as_slice()]);
    client.ack(&pulled).await.unwrap();
    let foreign = client.pull().await.unwrap_err();
    assert!(matches!(foreign, Error::ForeignEntry { seq: 3, .. }), "{foreign:?}");

    // A stream that starts with one cannot be joined: there is no key in
    // front of it.
    relay.transport().append(&name("plain"), &plain).await.unwrap();
    let refused = Device::new().try_open(relay.transport(), "plain", 1, Secret::Passphrase(PASSPHRASE)).await;
    assert!(matches!(refused, Err(Error::ForeignEntry { seq: 1, .. })), "{refused:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_passphrase_holds_on_every_device() {
    let relay = TestRelay::start().await;
    let (laptop, phone) = (Device::new(), Device::new());
    let on_laptop = laptop.open(relay.transport(), "notes", 1).await;
    let on_phone = phone.open(relay.transport(), "notes", 1).await;
    on_laptop.push([b"before".as_slice(), b"the change"]).await.unwrap();
    let key = on_laptop.key().id();

    on_laptop.change_passphrase(NEW_PASSPHRASE).await.unwrap();
    assert_eq!(on_laptop.passphrase_changes(), 1);
    on_laptop.push([b"after"]).await.unwrap();

    // Nothing was sealed again: the stream is its key, two items, the new
    // lock and one more item - and every entry opens under the same key.
    let page = relay.transport().read(&name("notes"), None, 100).await.unwrap();
    assert_eq!(page.head, 5);
    assert!(lacodda_seal::is_locked_key(&page.entries[3].data));

    // The phone takes the new lock in as it reads past it, and from its next
    // start the old passphrase no longer opens the stream there.
    assert_eq!(data(&drain(&on_phone).await), [b"before".as_slice(), b"the change", b"after"]);
    assert_eq!((on_phone.passphrase_changes(), on_phone.locked_key()), (1, on_laptop.locked_key()));
    assert!(on_phone.doctor().await.to_string().contains("passphrase changed once"));
    drop(on_phone);
    let phone = phone.restart();
    let old = phone.try_open(relay.transport(), "notes", 1, Secret::Passphrase(PASSPHRASE)).await;
    assert!(matches!(old, Err(Error::WrongPassphrase { .. })), "{old:?}");
    let on_phone = phone.try_open(relay.transport(), "notes", 1, Secret::Passphrase(NEW_PASSPHRASE)).await.unwrap();
    assert_eq!(on_phone.key().id(), key);

    // So does the laptop, which made the change.
    drop(on_laptop);
    let laptop = laptop.restart();
    let old = laptop.try_open(relay.transport(), "notes", 1, Secret::Passphrase(PASSPHRASE)).await;
    assert!(matches!(old, Err(Error::WrongPassphrase { .. })), "{old:?}");
    laptop.try_open(relay.transport(), "notes", 1, Secret::Passphrase(NEW_PASSPHRASE)).await.unwrap();

    // A device that joins now opens with the new passphrase, and reads
    // everything, the entries sealed before the change included.
    let tablet = Device::new();
    let on_tablet = tablet.try_open(relay.transport(), "notes", 1, Secret::Passphrase(NEW_PASSPHRASE)).await.unwrap();
    assert_eq!((on_tablet.key().id(), on_tablet.passphrase_changes()), (key, 1));
    assert_eq!(data(&drain(&on_tablet).await), [b"before".as_slice(), b"the change", b"after"]);
    let wrong = Device::new().try_open(relay.transport(), "notes", 1, Secret::Passphrase(b"neither")).await;
    assert!(matches!(wrong, Err(Error::WrongPassphrase { .. })), "{wrong:?}");

    // The old passphrase is not revoked - its lock is in the stream for good
    // - but a device that joins with it moves to the new lock as it reads,
    // like everyone else.
    let late = Device::new();
    let on_late = late.try_open(relay.transport(), "notes", 1, Secret::Passphrase(PASSPHRASE)).await.unwrap();
    drain(&on_late).await;
    assert_eq!(on_late.passphrase_changes(), 1);
    drop(on_late);
    let late = late.restart();
    let old = late.try_open(relay.transport(), "notes", 1, Secret::Passphrase(PASSPHRASE)).await;
    assert!(matches!(old, Err(Error::WrongPassphrase { .. })), "{old:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lock_the_key_did_not_seal_moves_nothing() {
    let relay = TestRelay::start().await;
    let device = Device::new();
    let client = device.open(relay.transport(), "notes", 1).await;
    client.change_passphrase(NEW_PASSPHRASE).await.unwrap();
    let page = relay.transport().read(&name("notes"), None, 10).await.unwrap();
    let (first, changed) = (page.entries[0].data.clone(), page.entries[1].data.clone());

    // The relay - or anyone who can write to it - hands the stream's first
    // lock in again: a real entry of the key, from an older generation. It
    // does not take the stream back to the old passphrase. Twice, in one
    // batch: alone, it would be the stream's first batch over again, which
    // the relay knows and does not write twice.
    let write = |entries: Vec<Vec<u8>>| efema::wire::Batch { epoch: Epoch(1), entries, stream: None, first: None };
    let replayed = relay.transport().append(&name("notes"), &write(vec![first.clone(), first])).await.unwrap();
    assert!(!replayed.repeated);
    let reader = Device::new();
    let on_reader = reader.try_open(relay.transport(), "notes", 1, Secret::Passphrase(NEW_PASSPHRASE)).await.unwrap();
    drain(&on_reader).await;
    drain(&client).await;
    assert_eq!(on_reader.passphrase_changes(), 1);
    assert_eq!(client.locked_key().to_bytes(), changed[..lacodda_seal::LOCKED_LEN]);

    // A lock of the key under a seal that is not the key's: refused, as an
    // altered entry is, rather than taken in.
    let mut forged = changed.clone();
    let last = forged.len() - 1;
    forged[last] ^= 1;
    let at = relay.transport().append(&name("notes"), &write(vec![forged])).await.unwrap().first;
    let refused = client.pull().await.unwrap_err();
    assert!(matches!(refused, Error::Inauthentic { seq, .. } if seq == at), "{refused:?}");
    assert_eq!(client.passphrase_changes(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_changes_at_once_agree_on_one_passphrase() {
    let relay = TestRelay::start().await;
    let (a, b) = (Device::new(), Device::new());
    let together = Arc::new(Barrier::new(2));
    let through = |together: &Arc<Barrier>| {
        let mut between = Between::new(relay.transport());
        between.together = Some(together.clone());
        between
    };
    a.open(relay.transport(), "notes", 1).await;
    b.open(relay.transport(), "notes", 1).await;
    // A device that reads both changes later, in one page.
    let e = Device::new();
    let on_e = e.open(relay.transport(), "notes", 1).await;
    // Opened again through transports that hold each append for the other.
    let on_a = a.try_open(through(&together), "notes", 1, Secret::Passphrase(PASSPHRASE)).await.unwrap();
    let on_b = b.try_open(through(&together), "notes", 1, Secret::Passphrase(PASSPHRASE)).await.unwrap();

    let (from_a, from_b) =
        tokio::join!(on_a.change_passphrase(b"set on a, at once"), on_b.change_passphrase(b"set on b, at once"));
    let lost = [&from_a, &from_b].iter().filter(|r| matches!(r, Err(Error::PassphraseRace { .. }))).count();
    assert_eq!((from_a.is_ok() || from_b.is_ok(), lost), (true, 1), "{from_a:?} {from_b:?}");
    assert_eq!(on_a.locked_key(), on_b.locked_key(), "the two devices hold different locks");
    assert_eq!((on_a.passphrase_changes(), on_b.passphrase_changes()), (1, 1));
    drain(&on_e).await;
    assert_eq!((on_e.locked_key(), on_e.passphrase_changes()), (on_a.locked_key(), 1));

    // A third device reads both changes, and takes the one that held.
    let c = Device::new();
    let held = if from_a.is_ok() { b"set on a, at once".as_slice() } else { b"set on b, at once" };
    let on_c = c.try_open(relay.transport(), "notes", 1, Secret::Passphrase(held)).await.unwrap();
    drain(&on_c).await;
    assert_eq!(on_c.locked_key(), on_a.locked_key());

    // The passphrase of the change that lost still opens - that lock is the
    // key's too - and the device keeps the lock that held, as the others do.
    let d = Device::new();
    let lost = if from_a.is_ok() { b"set on b, at once".as_slice() } else { b"set on a, at once" };
    let on_d = d.try_open(relay.transport(), "notes", 1, Secret::Passphrase(lost)).await.unwrap();
    assert_eq!((on_d.locked_key(), on_d.passphrase_changes()), (on_a.locked_key(), 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_behind_changes_the_passphrase_after_the_newest() {
    let relay = TestRelay::start().await;
    let (a, b, late) = (Device::new(), Device::new(), Device::new());
    let on_a = a.open(relay.transport(), "notes", 1).await;
    let on_b = b.open(relay.transport(), "notes", 1).await;
    on_a.change_passphrase(NEW_PASSPHRASE).await.unwrap();

    // The second device has not read the first change; its own still comes
    // after it, rather than losing to it.
    let third = b"a third passphrase, set from behind";
    on_b.change_passphrase(third).await.unwrap();
    assert_eq!(on_b.passphrase_changes(), 2);
    drain(&on_a).await;
    assert_eq!(on_a.locked_key(), on_b.locked_key());
    let on_late = late.try_open(relay.transport(), "notes", 1, Secret::Passphrase(third)).await.unwrap();
    assert_eq!(on_late.passphrase_changes(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn what_compresses_travels_compressed() {
    let relay = TestRelay::start().await;
    let (writer, reader) = (Device::new(), Device::new());
    let on_writer = writer.open(relay.transport(), "notes", 1).await;
    assert_eq!(on_writer.compression(), Compression::Zstd);
    let text = "- [ ] a task in a markdown note, much like the one before it\n".repeat(500).into_bytes();
    on_writer.push([&text]).await.unwrap();
    let plain = on_writer.with_compression(Compression::Off);
    plain.push([&text]).await.unwrap();

    let page = relay.transport().read(&name("notes"), None, 10).await.unwrap();
    let sizes: Vec<usize> = page.entries[1..].iter().map(|e| e.data.len()).collect();
    assert!(sizes[0] < text.len() / 3, "{} bytes of text went as {}", text.len(), sizes[0]);
    assert_eq!(sizes[1], text.len() + ENTRY_OVERHEAD, "compression off still compressed");

    let on_reader = reader.open(relay.transport(), "notes", 1).await;
    assert_eq!(data(&drain(&on_reader).await), [text.as_slice(), &text]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_wakes_on_a_new_entry_and_not_before() {
    let relay = TestRelay::start().await;
    let (a, b) = (Device::new(), Device::new());
    let on_a = a.open(relay.transport(), "notes", 1).await;
    let on_b = b.open(relay.transport(), "notes", 1).await;
    drain(&on_a).await;

    let started = Instant::now();
    assert!(!on_a.wait(Duration::from_secs(1)).await.unwrap());
    assert!(started.elapsed() >= Duration::from_millis(900));

    let waiting = tokio::spawn(async move {
        let woke = on_a.wait(Duration::from_secs(30)).await.unwrap();
        (woke, on_a)
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    on_b.push([b"wake up"]).await.unwrap();
    let (woke, on_a) = waiting.await.unwrap();
    assert!(woke && started.elapsed() < Duration::from_secs(10));
    assert_eq!(data(&drain(&on_a).await), [b"wake up".as_slice()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_doctor_reports_what_the_device_and_the_relay_know() {
    let relay = TestRelay::start().await;
    let (a, b) = (Device::new(), Device::new());
    let on_a = a.open(relay.transport(), "notes", 1).await;
    let on_b = b.open(relay.transport(), "notes", 1).await;
    on_a.push([b"1", b"2", b"3"]).await.unwrap();

    let report = on_b.doctor().await;
    assert_eq!(report.behind(), Some(4), "{report}");
    assert!(report.problems().is_empty(), "{report}");
    assert_eq!(report.key.id, on_a.key().id());
    let shown = report.to_string();
    for line in ["relay ", "stream ", "cursor ", "queue ", "key ", "device ", "epoch "] {
        assert!(shown.contains(line), "the report has no `{line}` line:\n{shown}");
    }
    assert!(shown.contains("4 entries to read"), "{shown}");
    assert!(shown.contains(&on_a.key().id().to_string()), "{shown}");

    drain(&on_b).await;
    assert_eq!(on_b.doctor().await.behind(), Some(0));

    let url = relay.url.clone();
    relay.stop().await;
    let report = on_b.doctor().await;
    assert!(report.relay.is_err() && !report.problems().is_empty(), "{report}");
    assert!(report.to_string().contains(&url), "{report}");
}
