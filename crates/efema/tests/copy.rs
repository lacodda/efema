//! A stream moved to another relay: every device goes on there from where it
//! stood - its cursor holds, and a batch it was still sending is known by
//! the new relay and not written twice. Against real relays on real sockets.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{Between, Device, TestRelay, TornRelay, data, drain, name, unsent};
use efema::wire::{Batch, Heads, Page, ProblemCode, Watch, Written};
use efema::{CopyError, Cursor, Limits, Relay, StreamName, Transport, TransportError, copy};
use lacodda_tear::Cut;

/// Every entry of `stream` as `transport` has it, marks of batches and all.
async fn everything<T: Transport>(transport: &T, stream: &str) -> Page {
    transport.read(&name(stream), None, 10_000).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_moved_stream_goes_on_from_where_each_device_stood() {
    let old = TornRelay::start().await;
    let new = TestRelay::start().await;
    let (laptop, phone) = (Device::new(), Device::new());

    let on_laptop = laptop.open(old.through(), "notes", 1).await;
    on_laptop.push([b"one".as_slice(), b"two"]).await.unwrap();
    on_laptop.push([b"three"]).await.unwrap();
    let on_phone = phone.open(old.direct(), "notes", 1).await;
    assert_eq!(data(&drain(&on_phone).await), [b"one".as_slice(), b"two", b"three"]);
    // The laptop's last push lands on the old relay, and its answer is lost:
    // the batch waits on the laptop, to be sent again.
    old.tear.cut(Cut::Down(0));
    let (waiting, _) = unsent(on_laptop.push([b"four"]).await);
    assert_eq!(waiting, 1);
    let head = old.head("notes").await;

    let copied = copy(&old.direct(), &new.transport(), &name("notes")).await.unwrap();
    assert_eq!((copied.cursor.seq, copied.entries), (head, head));
    assert_eq!(copied.cursor, everything(&old.direct(), "notes").await.cursor().unwrap());
    assert_eq!(everything(&new.transport(), "notes").await, everything(&old.direct(), "notes").await);

    // On the new relay: the waiting batch is known there and not written
    // again, and the phone reads on from its cursor.
    let on_laptop = laptop.open(new.transport(), "notes", 1).await;
    let sent = on_laptop.send().await.unwrap();
    assert_eq!((sent.items, sent.batches), (1, 1));
    assert_eq!(everything(&new.transport(), "notes").await.head, head, "the waiting batch landed twice");
    let on_phone = phone.open(new.transport(), "notes", 1).await;
    assert_eq!(data(&drain(&on_phone).await), [b"four".as_slice()]);
    on_laptop.push([b"five"]).await.unwrap();
    assert_eq!(data(&drain(&on_phone).await), [b"five".as_slice()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_is_copied_into_another_space_of_the_same_relay() {
    let relay = TestRelay::start().await;
    let elsewhere = relay.with(&relay.key("elsewhere", efema_server::Access::Write));
    let (laptop, phone) = (Device::new(), Device::new());
    let on_laptop = laptop.open(relay.transport(), "notes", 1).await;
    on_laptop.push([b"one".as_slice()]).await.unwrap();
    let on_phone = phone.open(relay.transport(), "notes", 1).await;
    drain(&on_phone).await;

    copy(&relay.transport(), &elsewhere, &name("notes")).await.unwrap();
    on_laptop.push([b"two".as_slice()]).await.unwrap();
    let copied = copy(&relay.transport(), &elsewhere, &name("notes")).await.unwrap();
    assert_eq!((copied.batches, copied.entries), (1, 1), "the second copy brought over only what was new");

    let on_phone = phone.open(elsewhere, "notes", 1).await;
    assert_eq!(data(&drain(&on_phone).await), [b"two".as_slice()]);
}

/// A relay that loses the connection on the appends `cut` names, counted
/// from 1 - with the batch not written.
struct Cutting {
    relay: Relay,
    appends: AtomicUsize,
    cut: Vec<usize>,
}

impl Transport for Cutting {
    fn describe(&self) -> String {
        self.relay.describe()
    }

    async fn limits(&self) -> Result<Limits, TransportError> {
        self.relay.limits().await
    }

    async fn append(&self, stream: &StreamName, batch: &Batch) -> Result<Written, TransportError> {
        let number = self.appends.fetch_add(1, Ordering::SeqCst) + 1;
        if self.cut.contains(&number) {
            return Err(TransportError::Unreachable { at: self.describe(), source: "the connection was torn".into() });
        }
        self.relay.append(stream, batch).await
    }

    async fn read(&self, stream: &StreamName, after: Option<&Cursor>, limit: usize) -> Result<Page, TransportError> {
        self.relay.read(stream, after, limit).await
    }

    async fn wait(&self, watches: &[Watch], timeout: Duration) -> Result<Heads, TransportError> {
        self.relay.wait(watches, timeout).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_that_stopped_goes_on_and_one_that_ran_writes_nothing() {
    let (old, new) = (TestRelay::start().await, TestRelay::start().await);
    let laptop = Device::new();
    let on_laptop = laptop.open(old.transport(), "notes", 1).await;
    for item in [b"one".as_slice(), b"two", b"three", b"four"] {
        on_laptop.push([item]).await.unwrap();
    }
    // The key's entry and four pushes: five batches, the third torn.
    let torn = Cutting { relay: new.transport(), appends: AtomicUsize::new(0), cut: vec![3] };
    let stopped = copy(&old.transport(), &torn, &name("notes")).await.unwrap_err();
    assert!(matches!(stopped, CopyError::Target { source: TransportError::Unreachable { .. }, .. }), "{stopped}");
    assert_eq!(everything(&new.transport(), "notes").await.head, 2);

    let went_on = copy(&old.transport(), &torn, &name("notes")).await.unwrap();
    assert_eq!((went_on.batches, went_on.entries), (3, 3));
    assert_eq!(everything(&new.transport(), "notes").await, everything(&old.transport(), "notes").await);
    let again = copy(&old.transport(), &new.transport(), &name("notes")).await.unwrap();
    assert_eq!((again.batches, again.cursor), (0, went_on.cursor));

    // A copy writes nothing the target has, and checks it reads the same.
    let reads = Arc::new(AtomicUsize::new(0));
    let mut counted = Between::new(new.transport());
    let seen = reads.clone();
    counted.tamper = Some(Arc::new(move |_, _| {
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    copy(&old.transport(), &counted, &name("notes")).await.unwrap();
    assert_eq!(counted.appends.load(Ordering::SeqCst), 0, "a copy that had nothing to do wrote");
    assert_eq!(reads.load(Ordering::SeqCst), 2, "where the target stands, and that it agrees");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_does_not_go_over_another_history() {
    let (old, new) = (TestRelay::start().await, TestRelay::start().await);
    let (laptop, phone) = (Device::new(), Device::new());
    let on_laptop = laptop.open(old.transport(), "notes", 1).await;
    on_laptop.push([b"one".as_slice()]).await.unwrap();

    // Another stream of the name where the copy goes.
    let stranger = Device::new();
    let theirs = stranger.open(new.transport(), "notes", 1).await.stream_id();
    let taken = copy(&old.transport(), &new.transport(), &name("notes")).await.unwrap_err();
    assert!(
        matches!(taken, CopyError::Taken { ours, theirs: t, .. } if ours == on_laptop.stream_id() && t == theirs),
        "{taken}"
    );

    // The stream moved, and a device wrote there: a copy finds the target
    // ahead on the same history, and has nothing to do.
    let target = TestRelay::start().await;
    copy(&old.transport(), &target.transport(), &name("notes")).await.unwrap();
    let on_phone = phone.open(target.transport(), "notes", 1).await;
    on_phone.push([b"moved".as_slice()]).await.unwrap();
    let ahead = copy(&old.transport(), &target.transport(), &name("notes")).await.unwrap();
    assert_eq!(ahead.batches, 0);
    // Then a device that had not moved wrote to the old one: the two have
    // parted, and the copy says where.
    on_laptop.push([b"stayed".as_slice()]).await.unwrap();
    let head = everything(&old.transport(), "notes").await.head;
    let parted = copy(&old.transport(), &target.transport(), &name("notes")).await.unwrap_err();
    assert!(matches!(parted, CopyError::Parted { seq, .. } if seq == head), "{parted}");
}

/// A target that someone else writes to while the copy goes on: a batch of
/// theirs lands right before the copy's `before`-th append.
struct Racing {
    relay: Relay,
    appends: AtomicUsize,
    before: usize,
}

impl Transport for Racing {
    fn describe(&self) -> String {
        self.relay.describe()
    }

    async fn limits(&self) -> Result<Limits, TransportError> {
        self.relay.limits().await
    }

    async fn append(&self, stream: &StreamName, batch: &Batch) -> Result<Written, TransportError> {
        if self.appends.fetch_add(1, Ordering::SeqCst) + 1 == self.before {
            let theirs = Batch {
                epoch: batch.epoch,
                entries: vec![b"someone else's".to_vec()],
                stream: batch.stream,
                first: None,
            };
            self.relay.append(stream, &theirs).await?;
        }
        self.relay.append(stream, batch).await
    }

    async fn read(&self, stream: &StreamName, after: Option<&Cursor>, limit: usize) -> Result<Page, TransportError> {
        self.relay.read(stream, after, limit).await
    }

    async fn wait(&self, watches: &[Watch], timeout: Duration) -> Result<Heads, TransportError> {
        self.relay.wait(watches, timeout).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_of_a_copy_lands_where_it_was_or_nowhere() {
    let (old, new) = (TestRelay::start().await, TestRelay::start().await);
    let laptop = Device::new();
    let on_laptop = laptop.open(old.transport(), "notes", 1).await;
    on_laptop.push([b"one".as_slice()]).await.unwrap();
    on_laptop.push([b"two".as_slice()]).await.unwrap();

    let racing = Racing { relay: new.transport(), appends: AtomicUsize::new(0), before: 2 };
    let parted = copy(&old.transport(), &racing, &name("notes")).await.unwrap_err();
    assert!(matches!(parted, CopyError::Parted { seq: 2, .. }), "{parted}");
    // The other writer's batch is there, and the copy's is not behind it.
    let target = everything(&new.transport(), "notes").await;
    assert_eq!(target.head, 2);
    assert_eq!(target.entries[1].data, b"someone else's");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_that_does_not_say_where_its_batches_end_is_not_copied() {
    let (old, new) = (TestRelay::start().await, TestRelay::start().await);
    let laptop = Device::new();
    let on_laptop = laptop.open(old.transport(), "notes", 1).await;
    on_laptop.push([b"one".as_slice(), b"two"]).await.unwrap();

    // A relay older than the marks.
    let mut unmarked = Between::new(old.transport());
    unmarked.tamper = Some(Arc::new(|_, page: &mut Page| {
        for entry in &mut page.entries {
            entry.ends_batch = false;
        }
    }));
    let error = copy(&unmarked, &new.transport(), &name("notes")).await.unwrap_err();
    assert!(matches!(error, CopyError::Unmarked { .. }), "{error}");
    assert!(
        matches!(new.transport().read(&name("notes"), None, 1).await, Err(e) if e.code() == Some(&ProblemCode::StreamNotFound)),
        "a copy that never saw a batch end wrote"
    );

    // One that says a batch ends where it does not: entries of two epochs.
    let raised = laptop.open(old.transport(), "notes", 2).await;
    raised.push([b"three".as_slice()]).await.unwrap();
    let mut joined = Between::new(old.transport());
    joined.tamper = Some(Arc::new(|_, page: &mut Page| {
        let last = page.entries.len() - 1;
        page.entries[last - 1].ends_batch = false;
    }));
    let error = copy(&joined, &TestRelay::start().await.transport(), &name("notes")).await.unwrap_err();
    assert!(matches!(error, CopyError::Unmarked { .. }), "{error}");

    // One whose chain does not hold together.
    let mut altered = Between::new(old.transport());
    altered.tamper = Some(Arc::new(|_, page: &mut Page| {
        page.entries[1].data[0] ^= 1;
    }));
    let error = copy(&altered, &TestRelay::start().await.transport(), &name("notes")).await.unwrap_err();
    assert!(matches!(error, CopyError::BrokenChain { seq: 2, .. }), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_needs_a_target_that_writes() {
    let (old, new) = (TestRelay::start().await, TestRelay::start().await);
    let laptop = Device::new();
    laptop.open(old.transport(), "notes", 1).await.push([b"one".as_slice()]).await.unwrap();
    let reader = new.with(&new.key("test", efema_server::Access::Read));
    let error = copy(&old.transport(), &reader, &name("notes")).await.unwrap_err();
    let CopyError::Target { source, .. } = &error else { panic!("{error}") };
    assert_eq!(source.code(), Some(&ProblemCode::Forbidden), "{error}");

    // And a source there is.
    let error = copy(&old.transport(), &new.transport(), &name("todo")).await.unwrap_err();
    let CopyError::Source { source, .. } = &error else { panic!("{error}") };
    assert_eq!(source.code(), Some(&ProblemCode::StreamNotFound), "{error}");
}

/// A relay older than the places: it takes no copy's first batch, and puts
/// any other wherever its next place is.
struct Older {
    relay: Relay,
}

impl Transport for Older {
    fn describe(&self) -> String {
        self.relay.describe()
    }

    async fn limits(&self) -> Result<Limits, TransportError> {
        self.relay.limits().await
    }

    async fn append(&self, stream: &StreamName, batch: &Batch) -> Result<Written, TransportError> {
        self.relay.append(stream, &Batch { first: None, ..batch.clone() }).await
    }

    async fn read(&self, stream: &StreamName, after: Option<&Cursor>, limit: usize) -> Result<Page, TransportError> {
        self.relay.read(stream, after, limit).await
    }

    async fn wait(&self, watches: &[Watch], timeout: Duration) -> Result<Heads, TransportError> {
        self.relay.wait(watches, timeout).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_target_older_than_the_places_is_told_apart() {
    let (old, new) = (TestRelay::start().await, TestRelay::start().await);
    let laptop = Device::new();
    let on_laptop = laptop.open(old.transport(), "notes", 1).await;
    on_laptop.push([b"one".as_slice()]).await.unwrap();
    let older = Older { relay: new.transport() };
    let error = copy(&old.transport(), &older, &name("notes")).await.unwrap_err();
    assert!(matches!(error, CopyError::OldTarget { .. }), "{error}");

    // With the stream there already, a batch it puts somewhere else than
    // the copy asked is caught by where it says it went.
    copy(&old.transport(), &new.transport(), &name("notes")).await.unwrap();
    let stranger = Device::new().open(new.transport(), "notes", 1).await;
    stranger.push([b"elsewhere".as_slice()]).await.unwrap();
    on_laptop.push([b"two".as_slice()]).await.unwrap();
    let error = copy(&old.transport(), &older, &name("notes")).await.unwrap_err();
    assert!(matches!(error, CopyError::Parted { seq: 3, .. }), "{error}");
}

/// A target that keeps something other than what it was sent: a byte of
/// every entry turned.
struct Garbling {
    relay: Relay,
}

impl Transport for Garbling {
    fn describe(&self) -> String {
        self.relay.describe()
    }

    async fn limits(&self) -> Result<Limits, TransportError> {
        self.relay.limits().await
    }

    async fn append(&self, stream: &StreamName, batch: &Batch) -> Result<Written, TransportError> {
        let mut garbled = batch.clone();
        for entry in &mut garbled.entries {
            entry[0] ^= 1;
        }
        self.relay.append(stream, &garbled).await
    }

    async fn read(&self, stream: &StreamName, after: Option<&Cursor>, limit: usize) -> Result<Page, TransportError> {
        self.relay.read(stream, after, limit).await
    }

    async fn wait(&self, watches: &[Watch], timeout: Duration) -> Result<Heads, TransportError> {
        self.relay.wait(watches, timeout).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_that_does_not_read_back_as_the_stream_is_not_taken_for_one() {
    let (old, new) = (TestRelay::start().await, TestRelay::start().await);
    let laptop = Device::new();
    laptop.open(old.transport(), "notes", 1).await.push([b"one".as_slice()]).await.unwrap();
    let error = copy(&old.transport(), &Garbling { relay: new.transport() }, &name("notes")).await.unwrap_err();
    assert!(matches!(error, CopyError::Parted { seq: 2, .. }), "{error}");
}
