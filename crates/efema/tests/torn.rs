//! What a client does when the network breaks - shown against connections
//! torn the way a network tears them, not against a mock that pretends to.
//!
//! A proxy (lacodda-tear) stands between the client and a real relay and
//! tears a connection on cue: partway up a request, before or partway down
//! an answer, or all of them at once, as a phone going offline. What the
//! relay holds is then asked of the relay itself, around the proxy.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Between, Device, TornRelay, data, drain, name, noise, unsent};
use efema::{Compression, Epoch, Error, Limits};
use lacodda_tear::{Cut, Direction};

/// The push stopped, its items are kept, and the network is why.
fn kept_for_the_network(result: Result<efema::Sent, Error>, items: usize) {
    let (unsent, source) = unsent(result);
    assert_eq!(unsent, items, "how many items wait");
    assert!(matches!(source, Error::Transport(_)), "stopped by {source:?}, not by the network");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_torn_on_its_way_up_is_not_written_and_goes_again_whole() {
    let torn = TornRelay::start().await;
    let (laptop, phone) = (Device::new(), Device::new());
    let on_laptop = laptop.open(torn.through(), "notes", 1).await.with_compression(Compression::Off);
    let head = torn.head("notes").await;

    // Through the headers and partway into the body of a 10 KB push.
    torn.tear.cut(Cut::Up(2_000));
    let item = noise(10_000, 1);
    kept_for_the_network(on_laptop.push([&item]).await, 1);
    assert_eq!(torn.tear.torn(), 1, "the request was not torn");
    assert_eq!(torn.head("notes").await, head, "half a batch reached the stream");
    assert_eq!(on_laptop.queue().await.unwrap().items, 1);

    let sent = on_laptop.send().await.unwrap();
    assert_eq!((sent.items, sent.batches), (1, 1));
    assert!(on_laptop.queue().await.unwrap().is_empty());
    let on_phone = phone.open(torn.direct(), "notes", 1).await;
    assert_eq!(data(&drain(&on_phone).await), [item.as_slice()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_lost_on_its_way_back_does_not_write_the_batch_twice() {
    let torn = TornRelay::start().await;
    let (laptop, phone) = (Device::new(), Device::new());
    let on_laptop = laptop.open(torn.through(), "notes", 1).await;
    let head = torn.head("notes").await;

    // The relay writes the batch and answers; the answer never arrives.
    torn.tear.cut(Cut::Down(0));
    kept_for_the_network(on_laptop.push([b"one".as_slice(), b"two"]).await, 2);
    assert_eq!(torn.head("notes").await, head + 2, "the relay did not write the batch");

    // Sent again, byte for byte: the relay knows it, and writes nothing.
    let sent = on_laptop.send().await.unwrap();
    assert_eq!((sent.items, sent.batches), (2, 1));
    assert_eq!(torn.head("notes").await, head + 2, "the batch was written twice");
    let on_phone = phone.open(torn.direct(), "notes", 1).await;
    assert_eq!(data(&drain(&on_phone).await), [b"one".as_slice(), b"two"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn what_waits_outlives_the_app_closing() {
    let torn = TornRelay::start().await;
    let (laptop, phone) = (Device::new(), Device::new());
    let on_laptop = laptop.open(torn.through(), "notes", 1).await;

    torn.tear.offline();
    kept_for_the_network(on_laptop.push([b"written on a train".as_slice()]).await, 1);
    kept_for_the_network(on_laptop.push([b"and another".as_slice()]).await, 2);
    drop(on_laptop);
    let laptop = laptop.restart();

    // A new run, still offline: the client opens from its state, and what
    // waited is there.
    let on_laptop = laptop.open(torn.through(), "notes", 1).await;
    assert_eq!(on_laptop.queue().await.unwrap().items, 2);
    kept_for_the_network(on_laptop.send().await, 2);

    torn.tear.online();
    let sent = on_laptop.send().await.unwrap();
    assert_eq!((sent.items, sent.batches), (2, 2));
    let on_phone = phone.open(torn.direct(), "notes", 1).await;
    assert_eq!(data(&drain(&on_phone).await), [b"written on a train".as_slice(), b"and another"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_push_resumes_where_it_stopped() {
    let torn = TornRelay::start().await;
    let (laptop, phone) = (Device::new(), Device::new());
    let mut between = Between::new(torn.through());
    // One 1200-byte item per request.
    between.limits = Some(Limits { body: 2_000, ..Limits::DEFAULT });
    let appends = between.appends.clone();
    // The append that loses its answer, once the client has opened.
    let doomed = Arc::new(AtomicUsize::new(usize::MAX));
    let (tear, target) = (torn.tear.clone(), doomed.clone());
    between.on_append = Some(Arc::new(move |number| {
        if number == target.load(Ordering::SeqCst) {
            tear.cut(Cut::Down(0));
        }
    }));
    let on_laptop = laptop.open(between, "notes", 1).await.with_compression(Compression::Off);
    let opened = appends.load(Ordering::SeqCst);
    let head = torn.head("notes").await;

    // The third batch is written, and its answer lost.
    doomed.store(opened + 3, Ordering::SeqCst);
    let items: Vec<Vec<u8>> = (0..5).map(|i| noise(1_200, i)).collect();
    kept_for_the_network(on_laptop.push(&items).await, 3);
    assert_eq!(torn.head("notes").await, head + 3);

    let sent = on_laptop.send().await.unwrap();
    assert_eq!((sent.items, sent.batches), (3, 3));
    // Batches one and two went once; three went again, and was known.
    assert_eq!(appends.load(Ordering::SeqCst) - opened, 3 + 3, "the push started over");
    assert_eq!(torn.head("notes").await, head + 5);
    let on_phone = phone.open(torn.direct(), "notes", 1).await;
    assert_eq!(data(&drain(&on_phone).await), items.iter().map(Vec::as_slice).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pull_torn_partway_resumes_from_the_acknowledged_cursor() {
    let torn = TornRelay::start().await;
    let (laptop, phone) = (Device::new(), Device::new());
    let on_laptop = laptop.open(torn.direct(), "notes", 1).await.with_compression(Compression::Off);
    let items: Vec<Vec<u8>> = (0..30).map(|i| noise(1_000, i)).collect();
    on_laptop.push(&items).await.unwrap();

    let mut between = Between::new(torn.through());
    between.limits = Some(Limits { page_entries: 10, ..Limits::DEFAULT });
    let on_phone = phone.open(between, "notes", 1).await;
    let first = on_phone.pull().await.unwrap();
    on_phone.ack(&first).await.unwrap();
    let cursor = on_phone.cursor().await.unwrap();

    // Partway down the next page.
    torn.tear.cut(Cut::Down(3_000));
    let refused = on_phone.pull().await.unwrap_err();
    assert!(matches!(refused, Error::Transport(_)), "{refused:?}");
    assert!(torn.tear.passed(Direction::Down) > 0);
    assert_eq!(on_phone.cursor().await.unwrap(), cursor, "a torn pull moved the cursor");

    // Not from the start: from the first entry after the acknowledged page.
    let again = on_phone.pull().await.unwrap();
    assert_eq!(again.entries[0].data, items[first.entries.len()]);
    on_phone.ack(&again).await.unwrap();
    let mut all = first.entries;
    all.extend(again.entries);
    all.extend(drain(&on_phone).await);
    assert_eq!(data(&all), items.iter().map(Vec::as_slice).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sends_at_once_take_turns() {
    let torn = TornRelay::start().await;
    let device = Device::new();
    let between = Between::new(torn.through());
    let appends = between.appends.clone();
    let client = device.open(between, "notes", 1).await;

    torn.tear.offline();
    for item in [b"a".as_slice(), b"b", b"c"] {
        let _ = client.push([item]).await;
    }
    torn.tear.online();
    let online = appends.load(Ordering::SeqCst);
    let (one, other) = tokio::join!(client.send(), client.send());
    let (one, other) = (one.unwrap(), other.unwrap());
    assert_eq!(one.items + other.items, 3);
    assert_eq!(appends.load(Ordering::SeqCst) - online, 3, "a batch was sent by both");
    assert_eq!(torn.head("notes").await, 1 + 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn what_an_older_app_left_waiting_is_refused_by_name_until_forgotten() {
    let torn = TornRelay::start().await;
    let (device, other) = (Device::new(), Device::new());
    let old_app = device.open(torn.through(), "notes", 1).await;
    torn.tear.offline();
    kept_for_the_network(old_app.push([b"from the old app".as_slice()]).await, 1);
    drop(old_app);

    // Meanwhile a newer app on another device moves the stream to epoch 2,
    // and this device is updated before it is back online.
    other.open(torn.direct(), "notes", 2).await.push([b"epoch 2"]).await.unwrap();
    torn.tear.online();
    let device = device.restart();
    let new_app = device.open(torn.through(), "notes", 2).await;
    let (waiting, refused) = unsent(new_app.send().await);
    assert_eq!(waiting, 1);
    // Named for what was being sent - the old app's epoch - not this app's.
    assert!(matches!(refused, Error::EpochBehind { ours: Epoch(1), current: Epoch(2), .. }), "{refused:?}");
    let (_, again) = unsent(new_app.push([b"from the new app"]).await);
    assert!(matches!(again, Error::EpochBehind { .. }), "a later push went ahead of what waited: {again:?}");

    // The way out: forget, join again, and send what the stream lacks.
    device.state.forget(&name("notes")).unwrap();
    let new_app = device.open(torn.through(), "notes", 2).await;
    assert!(new_app.queue().await.unwrap().is_empty());
    new_app.push([b"from the new app"]).await.unwrap();
    assert_eq!(data(&drain(&new_app).await), [b"epoch 2".as_slice(), b"from the new app"]);
}
