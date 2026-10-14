//! What a device can do with the access key it has, and what it does with
//! the limits the relay holds that key to - against a real relay over a real
//! socket.

mod common;

use common::{Device, PASSPHRASE, TestRelay, data, drain, noise, unsent};
use efema::{Compression, Error, Relay, Secret};
use efema_server::{Access, Config};

fn tight(body: usize, entry: usize, quota: u64) -> Config {
    Config { limits: efema_server::Limits { body, entry, quota, rate: 600 }, ..Config::default() }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_with_a_key_that_reads_pulls_and_keeps_nothing_to_send() {
    let relay = TestRelay::start().await;
    let (laptop, phone) = (Device::new(), Device::new());
    let on_laptop = laptop.open(relay.transport(), "notes", 1).await;
    on_laptop.push([b"from the laptop".as_slice()]).await.unwrap();
    drain(&on_laptop).await;

    let reader = relay.with(&relay.key("test", Access::Read));
    let on_phone = phone.open(reader, "notes", 1).await;
    assert_eq!(data(&drain(&on_phone).await), [b"from the laptop".as_slice()]);

    let refused = on_phone.push([b"from the phone".as_slice()]).await.unwrap_err();
    assert!(matches!(refused, Error::ReadOnly { .. }), "{refused:?}");
    assert!(on_phone.queue().await.unwrap().is_empty(), "a key that reads filled the queue");
    let refused = on_phone.change_passphrase(b"another passphrase").await.unwrap_err();
    assert!(matches!(refused, Error::ReadOnly { .. }), "{refused:?}");
    assert_eq!(drain(&on_laptop).await.len(), 0, "the phone wrote something");

    let report = on_phone.doctor().await;
    assert!(report.to_string().contains("\naccess   reads only - "), "{report}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_that_reads_cannot_create_a_stream() {
    let relay = TestRelay::start().await;
    let reader = relay.with(&relay.key("test", Access::Read));
    let refused = Device::new().try_open(reader, "notes", 1, Secret::Passphrase(PASSPHRASE)).await.unwrap_err();
    assert!(matches!(refused, Error::ReadOnly { .. }), "{refused:?}");
    // Nothing on the relay: a device with a key that writes creates it.
    Device::new().open(relay.transport(), "notes", 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_the_relay_does_not_take_says_so() {
    let relay = TestRelay::start().await;
    let stranger = relay.with(&efema::AccessKey::from_bytes([3; 32]));
    let refused = Device::new().try_open(stranger, "notes", 1, Secret::Passphrase(PASSPHRASE)).await.unwrap_err();
    assert!(matches!(refused, Error::Unauthorized { .. }), "{refused:?}");

    // A key revoked under a device that opened with it: what it pushes is
    // kept, and why it waits is said.
    let phone_key = relay.key("test", Access::Write);
    let device = Device::new();
    let client = device.open(relay.with(&phone_key), "notes", 1).await;
    efema_server::store::Store::open(relay.dir.path()).unwrap().revoke(&phone_key.id()).unwrap();
    let (kept, why) = unsent(client.push([b"after the revocation".as_slice()]).await);
    assert_eq!(kept, 1);
    assert!(matches!(why, Error::Unauthorized { .. }), "{why:?}");
    assert!(matches!(client.pull().await, Err(Error::Unauthorized { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_of_another_space_is_another_stream() {
    let relay = TestRelay::start().await;
    let (ours, theirs) = (Device::new(), Device::new());
    let on_ours = ours.open(relay.transport(), "notes", 1).await;
    on_ours.push([b"ours".as_slice()]).await.unwrap();

    let other = relay.with(&relay.key("other", Access::Write));
    let on_theirs = theirs.try_open(other, "notes", 1, Secret::Passphrase(b"their own passphrase")).await.unwrap();
    assert_ne!(on_theirs.stream_id(), on_ours.stream_id());
    assert_ne!(on_theirs.key().id(), on_ours.key().id(), "they joined our stream's key");
    on_theirs.push([b"theirs".as_slice()]).await.unwrap();
    assert_eq!(data(&drain(&on_ours).await), [b"ours".as_slice()]);
    assert_eq!(data(&drain(&on_theirs).await), [b"theirs".as_slice()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_without_the_network_cuts_to_the_limits_kept() {
    let relay = TestRelay::start_with(tight(3_000, 3_000, u64::MAX)).await;
    let device = Device::new();
    let client = device.open(relay.transport(), "notes", 1).await.with_compression(Compression::Off);
    let (url, key) = (relay.url.clone(), relay.key.clone());
    let dir = relay.stop().await;

    // The relay is gone; what it announced is not.
    let items: Vec<Vec<u8>> = (0..6).map(|i| noise(1_200, i)).collect();
    let (kept, _) = unsent(client.push(&items).await);
    assert_eq!(kept, 6);
    let queue = client.queue().await.unwrap();
    assert_eq!(queue.batches, 3, "cut to 3000 bytes a request, two items each: {queue:?}");
    assert!(client.max_item_len().await.unwrap() < 3_000);
    drop(client);

    let relay = TestRelay::start_in_with(dir, tight(3_000, 3_000, u64::MAX)).await;
    let device = device.restart();
    let client = device.open(Relay::new(&relay.url, key).unwrap(), "notes", 1).await;
    assert_ne!(relay.url, url);
    let sent = client.send().await.unwrap();
    assert_eq!((sent.items, sent.batches), (6, 3), "the batches went as they were cut");
    assert_eq!(data(&drain(&client).await), items.iter().map(Vec::as_slice).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queue_cut_to_limits_since_lowered_is_cut_again_and_lands_once() {
    let relay = TestRelay::start().await;
    let device = Device::new();
    let client = device.open(relay.transport(), "notes", 1).await.with_compression(Compression::Off);
    let key = relay.key.clone();
    let dir = relay.stop().await;

    // Cut offline to the 16 MiB the relay announced: one batch.
    let first: Vec<Vec<u8>> = (0..5).map(|i| noise(1_200, i)).collect();
    let second: Vec<Vec<u8>> = (5..7).map(|i| noise(1_200, i)).collect();
    unsent(client.push(&first).await);
    unsent(client.push(&second).await);
    assert_eq!(client.queue().await.unwrap().batches, 2);
    drop(client);

    // The operator lowers the limit before the relay comes back.
    let relay = TestRelay::start_in_with(dir, tight(3_000, 3_000, u64::MAX)).await;
    let device = device.restart();
    let client = device.open(Relay::new(&relay.url, key).unwrap(), "notes", 1).await;
    let sent = client.send().await.unwrap();
    // Seven items, two to a request of 3000 bytes.
    assert_eq!((sent.items, sent.batches), (7, 4));
    assert!(client.queue().await.unwrap().is_empty());
    assert!(client.max_item_len().await.unwrap() < 3_000, "the lowered limits were not kept");

    let phone = Device::new();
    let on_phone = phone.open(relay.transport(), "notes", 1).await;
    let mut all = first.clone();
    all.extend(second);
    assert_eq!(data(&drain(&on_phone).await), all.iter().map(Vec::as_slice).collect::<Vec<_>>(), "once, in order");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_item_larger_than_the_relay_takes_now_waits_and_says_so() {
    let relay = TestRelay::start().await;
    let device = Device::new();
    let client = device.open(relay.transport(), "notes", 1).await.with_compression(Compression::Off);
    let key = relay.key.clone();
    let dir = relay.stop().await;
    unsent(client.push([noise(5_000, 1)]).await);
    drop(client);

    let relay = TestRelay::start_in_with(dir, tight(16_000, 2_000, u64::MAX)).await;
    let device = device.restart();
    let client = device.open(Relay::new(&relay.url, key).unwrap(), "notes", 1).await;
    let (kept, why) = unsent(client.send().await);
    assert_eq!(kept, 1);
    assert!(matches!(why, Error::ItemTooLarge { limit: 2_000, .. }), "{why:?}");
    assert_eq!(client.queue().await.unwrap().batches, 1, "a cut that could not be made changed the queue");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_space_keeps_what_was_pushed_and_says_why() {
    let relay = TestRelay::start_with(tight(16_000, 8_000, 5_000)).await;
    let device = Device::new();
    let client = device.open(relay.transport(), "notes", 1).await.with_compression(Compression::Off);
    client.push([noise(2_200, 1), noise(2_200, 2)]).await.unwrap();
    let (kept, why) = unsent(client.push([noise(2_200, 3)]).await);
    assert_eq!(kept, 1);
    assert!(matches!(why, Error::QuotaExceeded { quota: 5_000, .. }), "{why:?}");

    let report = client.doctor().await;
    let shown = report.to_string();
    assert!(shown.contains("of 4.9 KiB in the space"), "{shown}");
    assert!(report.problems().iter().any(|p| p.starts_with("the space holds")), "{shown}");
}
