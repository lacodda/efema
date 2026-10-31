//! A stream in an S3 bucket, against a real S3 over a real socket: MinIO in
//! CI, any bucket on a developer's machine.
//!
//! The bucket comes from the environment - `EFEMA_TEST_BUCKET` (its address,
//! `http://127.0.0.1:9000/efema-test`), `EFEMA_TEST_BUCKET_REGION`
//! (`us-east-1` if unset), `EFEMA_TEST_BUCKET_KEY` and
//! `EFEMA_TEST_BUCKET_SECRET`, and credentials to the same bucket that only
//! read it - `EFEMA_TEST_BUCKET_READER_KEY` and
//! `EFEMA_TEST_BUCKET_READER_SECRET`. Without it these tests say they are skipped
//! and pass; with `EFEMA_TEST_BUCKET_REQUIRED` set they fail instead, so the
//! CI job that runs them against MinIO cannot go green without a bucket.
//! Every test writes streams of its own, under fresh names.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Device, NEW_PASSPHRASE, PASSPHRASE, contains, data, drain, name, noise};
use efema::wire::{Batch, Entry, ProblemCode, Watch};
use efema::{Bucket, Compression, Credentials, Cursor, Epoch, Error, Secret, StreamId, StreamName, Transport, chain};
use lacodda_tear::{Cut, Direction, Tear};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Barrier;

/// The bucket the tests write to.
struct Settings {
    url: reqwest::Url,
    region: String,
    id: String,
    secret: String,
    reader: (String, String),
}

impl Settings {
    fn from_env() -> Option<Self> {
        let Ok(url) = std::env::var("EFEMA_TEST_BUCKET") else {
            assert!(
                std::env::var_os("EFEMA_TEST_BUCKET_REQUIRED").is_none(),
                "EFEMA_TEST_BUCKET_REQUIRED is set, and EFEMA_TEST_BUCKET is not: no bucket to test against"
            );
            eprintln!("skipped: EFEMA_TEST_BUCKET is not set");
            return None;
        };
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("EFEMA_TEST_BUCKET needs {name}"));
        Some(Self {
            url: url.parse().expect("EFEMA_TEST_BUCKET is an address"),
            region: std::env::var("EFEMA_TEST_BUCKET_REGION").unwrap_or_else(|_| "us-east-1".into()),
            id: var("EFEMA_TEST_BUCKET_KEY"),
            secret: var("EFEMA_TEST_BUCKET_SECRET"),
            reader: (var("EFEMA_TEST_BUCKET_READER_KEY"), var("EFEMA_TEST_BUCKET_READER_SECRET")),
        })
    }

    fn credentials(&self) -> Credentials {
        Credentials::new(&self.id, &self.secret)
    }

    /// The bucket, as a device that has not talked to it before reaches it.
    fn bucket(&self) -> Bucket {
        Bucket::new(self.url.as_str(), &self.region, self.credentials()).unwrap()
    }

    /// The bucket, with credentials that only read it.
    fn for_reading(&self) -> Bucket {
        Bucket::new(self.url.as_str(), &self.region, Credentials::new(&self.reader.0, &self.reader.1)).unwrap()
    }

    /// The bucket, reached through `proxy` on this machine.
    fn through(&self, proxy: SocketAddr) -> Bucket {
        let mut url = self.url.clone();
        url.set_ip_host(proxy.ip()).unwrap();
        url.set_port(Some(proxy.port())).unwrap();
        Bucket::new(url.as_str(), &self.region, self.credentials()).unwrap()
    }

    /// Where the bucket's service listens.
    fn upstream(&self) -> SocketAddr {
        self.url.socket_addrs(|| None).unwrap()[0]
    }

    /// A stream name no other test and no other run has used.
    fn fresh(&self, what: &str) -> StreamName {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        name(&format!("test-{what}-{suffix}"))
    }
}

fn batch(entries: &[&[u8]], stream: Option<StreamId>) -> Batch {
    Batch { epoch: Epoch(1), entries: entries.iter().map(|e| e.to_vec()).collect(), stream, first: None }
}

/// Every entry of `stream`, read page by page by a device that knows nothing
/// of the bucket - each entry checked to follow the one before it.
async fn everything(at: &Settings, stream: &StreamName, page: usize) -> Vec<Entry> {
    let mut all: Vec<Entry> = Vec::new();
    let mut after: Option<Cursor> = None;
    loop {
        let read = at.bucket().read(stream, after.as_ref(), page).await.unwrap();
        assert!(read.entries.len() <= page, "a page of {} entries, asked for {page}", read.entries.len());
        let mut previous = after.unwrap_or_else(|| Cursor::start(read.stream));
        for entry in &read.entries {
            assert_eq!(entry.seq, previous.seq + 1, "a gap before {}", entry.seq);
            assert_eq!(entry.hash, chain::link(&previous.hash, entry.seq, entry.epoch, &entry.data), "a broken link");
            previous = entry.cursor(read.stream);
        }
        all.extend(read.entries.iter().cloned());
        match read.cursor() {
            Some(cursor) if cursor.seq < read.head => after = Some(cursor),
            _ => return all,
        }
    }
}

fn texts(entries: &[Entry]) -> Vec<String> {
    entries.iter().map(|entry| String::from_utf8(entry.data.clone()).unwrap()).collect()
}

fn refusal(result: Result<impl std::fmt::Debug, efema::TransportError>) -> (ProblemCode, efema::wire::Problem) {
    match result {
        Err(efema::TransportError::Refused { problem, .. }) => (problem.code.clone(), *problem),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_devices_sync_through_a_bucket() {
    let Some(at) = Settings::from_env() else { return };
    let stream = at.fresh("sync");
    let (laptop, phone) = (Device::new(), Device::new());
    let on_laptop = laptop.open(at.bucket(), stream.as_str(), 1).await;
    let sent = on_laptop.push([b"first".as_slice(), b"second"]).await.unwrap();
    assert_eq!((sent.items, sent.batches), (2, 1));

    let on_phone = phone.open(at.bucket(), stream.as_str(), 1).await;
    let received = drain(&on_phone).await;
    assert_eq!(data(&received), [b"first".as_slice(), b"second"]);
    assert!(received.iter().all(|r| !r.own && r.device == laptop.state.device()));
    assert_eq!(on_phone.key().id(), on_laptop.key().id());

    on_phone.push([b"from the phone"]).await.unwrap();
    assert!(on_laptop.wait(Duration::from_secs(30)).await.unwrap());
    let received = drain(&on_laptop).await;
    assert_eq!(data(&received), [b"first".as_slice(), b"second", b"from the phone"]);

    let report = on_laptop.doctor().await;
    assert!(report.problems().is_empty(), "{report}");
    assert_eq!(report.behind(), Some(0), "{report}");
    assert!(report.to_string().contains("reads and writes - no quota"), "{report}");
}

/// The order of a stream without anyone to keep it: writers at once, each
/// with no memory of the others, and every batch lands whole, on
/// consecutive positions, in one sequence that every reader reads the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn writers_at_once_land_whole_batches_in_one_order() {
    let Some(at) = Settings::from_env() else { return };
    let at = Arc::new(at);
    let stream = at.fresh("busy");
    let id = at.bucket().append(&stream, &batch(&[b"start"], None)).await.unwrap().stream;

    const WRITERS: usize = 8;
    const BATCHES: usize = 5;
    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut tasks = Vec::new();
    for writer in 0..WRITERS {
        let (at, stream, barrier) = (at.clone(), stream.clone(), barrier.clone());
        tasks.push(tokio::spawn(async move {
            let bucket = at.bucket();
            barrier.wait().await;
            let mut written = Vec::new();
            for number in 0..BATCHES {
                let entries: Vec<Vec<u8>> =
                    (0..=(writer + number) % 4).map(|e| format!("w{writer} b{number} e{e}").into_bytes()).collect();
                let landed = bucket
                    .append(
                        &stream,
                        &Batch { epoch: Epoch(1), entries: entries.clone(), stream: Some(id), first: None },
                    )
                    .await
                    .unwrap();
                assert!(!landed.repeated);
                assert_eq!(landed.last - landed.first + 1, entries.len() as u64);
                written.push((landed, entries));
            }
            written
        }));
    }
    let mut written = Vec::new();
    for task in tasks {
        written.extend(task.await.unwrap());
    }

    let all = everything(&at, &stream, 7).await;
    let total: usize = written.iter().map(|(_, entries)| entries.len()).sum();
    assert_eq!(all.len(), 1 + total, "entries lost or written twice");
    for (landed, entries) in &written {
        let there: Vec<&[u8]> = (landed.first..=landed.last).map(|seq| all[seq as usize - 1].data.as_slice()).collect();
        let sent: Vec<&[u8]> = entries.iter().map(Vec::as_slice).collect();
        assert_eq!(there, sent, "the batch at {}..{} is not whole there", landed.first, landed.last);
    }
    // A second reader, with pages of another length, reads the same.
    assert_eq!(everything(&at, &stream, 1000).await, all);
}

/// Devices that start one stream at the same moment start one stream: the
/// record is created once, and every device writes into it.
#[tokio::test(flavor = "multi_thread")]
async fn devices_creating_one_stream_at_once_share_it() {
    let Some(at) = Settings::from_env() else { return };
    for _ in 0..5 {
        let stream = at.fresh("created");
        let (one, other) = (at.bucket(), at.bucket());
        let (first, second) = (batch(&[b"from one"], None), batch(&[b"from the other"], None));
        let (a, b) = tokio::join!(one.append(&stream, &first), other.append(&stream, &second));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.stream, b.stream, "two incarnations of one name");
        let mut read = texts(&everything(&at, &stream, 100).await);
        read.sort();
        assert_eq!(read, ["from one", "from the other"]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_sent_again_lands_once() {
    let Some(at) = Settings::from_env() else { return };
    let stream = at.fresh("again");
    let first = batch(&[b"one", b"two"], None);
    let written = at.bucket().append(&stream, &first).await.unwrap();
    assert_eq!((written.first, written.last, written.repeated), (1, 2, false));

    // Sent again by a device that remembers nothing of the first sending -
    // its answer lost, the app restarted.
    let again = at.bucket().append(&stream, &first).await.unwrap();
    assert_eq!((again.first, again.last, again.repeated), (1, 2, true));
    assert_eq!(again.stream, written.stream);

    // And after the stream has gone on: still where it landed.
    let id = written.stream;
    at.bucket().append(&stream, &batch(&[b"three"], Some(id))).await.unwrap();
    let named = Batch { stream: Some(id), ..first.clone() };
    let late = at.bucket().append(&stream, &named).await.unwrap();
    assert_eq!((late.first, late.last, late.repeated), (1, 2, true));

    // Sent at once, twice: one of the two writes it.
    let both = batch(&[b"four"], Some(id));
    let (one, other) = (at.bucket(), at.bucket());
    let (a, b) = tokio::join!(one.append(&stream, &both), other.append(&stream, &both));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!((a.first, a.last), (b.first, b.last));
    assert!(a.repeated != b.repeated, "{a:?} and {b:?}");

    assert_eq!(texts(&everything(&at, &stream, 100).await), ["one", "two", "three", "four"]);
}

/// One batch sent from several places at once - a send given up on as
/// stalled while its request still went through, and sent again - among
/// other writers: it lands once, and every sending is told where.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_batch_sent_from_several_places_at_once_lands_once() {
    let Some(at) = Settings::from_env() else { return };
    let at = Arc::new(at);
    let stream = at.fresh("twins");
    let id = at.bucket().append(&stream, &batch(&[b"start"], None)).await.unwrap().stream;
    const TWINS: usize = 6;
    const OTHERS: usize = 3;
    for round in 0..8 {
        let twin = format!("twin {round}");
        let barrier = Arc::new(Barrier::new(TWINS + OTHERS));
        let mut tasks = Vec::new();
        for task in 0..TWINS + OTHERS {
            let (at, stream, barrier) = (at.clone(), stream.clone(), barrier.clone());
            let entry = if task < TWINS { twin.clone() } else { format!("other {round} {task}") };
            tasks.push(tokio::spawn(async move {
                let bucket = at.bucket();
                barrier.wait().await;
                let written = bucket.append(&stream, &batch(&[entry.as_bytes()], Some(id))).await.unwrap();
                (task < TWINS, written)
            }));
        }
        let mut twins = Vec::new();
        for task in tasks {
            let (is_twin, written) = task.await.unwrap();
            if is_twin {
                twins.push(written);
            }
        }
        assert_eq!(twins.iter().filter(|w| !w.repeated).count(), 1, "round {round}: {twins:?}");
        assert!(twins.iter().all(|w| (w.first, w.last) == (twins[0].first, twins[0].last)), "round {round}: {twins:?}");
        let read = texts(&everything(&at, &stream, 1000).await);
        assert_eq!(read.iter().filter(|text| **text == twin).count(), 1, "round {round}: {read:?}");
        assert_eq!(read[twins[0].first as usize - 1], twin);
    }
}

/// Whichever answer of an append the network loses - the record, the
/// listings, the offer, the batch itself - sending the batch again writes it
/// once, and the stream goes on without a gap. And whichever request it
/// tears on the way up, nothing half-written is left.
#[tokio::test(flavor = "multi_thread")]
async fn whichever_answer_is_lost_a_batch_sent_again_lands_once() {
    let Some(at) = Settings::from_env() else { return };
    let stream = at.fresh("torn");
    let id = at.bucket().append(&stream, &batch(&[b"start"], None)).await.unwrap().stream;
    let tear = Tear::start(at.upstream()).await.unwrap();
    let torn = at.through(tear.addr());
    torn.limits().await.unwrap();

    // One append, to learn how many bytes its requests and answers take.
    let (up, down) = (tear.passed(Direction::Up), tear.passed(Direction::Down));
    torn.append(&stream, &batch(&[b"measured"], Some(id))).await.unwrap();
    let (up, down) = (tear.passed(Direction::Up) - up, tear.passed(Direction::Down) - down);

    let mut expected = vec!["start".to_string(), "measured".to_string()];
    let cuts =
        (0..down).step_by((down / 30).max(1)).map(Cut::Down).chain((0..up).step_by((up / 30).max(1)).map(Cut::Up));
    for (number, cut) in cuts.enumerate() {
        let item = format!("item {number}");
        let one = batch(&[item.as_bytes()], Some(id));
        tear.cut(cut);
        let first = torn.append(&stream, &one).await;
        tear.disarm();
        // Again, from a device with no memory of the first try.
        let again = at.through(tear.addr()).append(&stream, &one).await.unwrap_or_else(|e| panic!("{cut:?}: {e}"));
        if let Ok(first) = first {
            assert!(again.repeated, "{cut:?}: sent twice, written twice");
            assert_eq!((again.first, again.last), (first.first, first.last));
        }
        expected.push(item);
    }
    assert!(tear.torn() >= 30, "the sweep tore {} connections", tear.torn());
    assert_eq!(texts(&everything(&at, &stream, 1000).await), expected);
}

/// A reader that stopped inside a batch - its page cut by length or by
/// bytes - reads on from there, on a device that knows nothing of where the
/// batches begin.
#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_inside_a_batch_reads_on_from_there() {
    let Some(at) = Settings::from_env() else { return };
    let stream = at.fresh("inside");
    let ten: Vec<String> = (1..=10).map(|n| format!("entry {n}")).collect();
    let refs: Vec<&[u8]> = ten.iter().map(|t| t.as_bytes()).collect();
    let id = at.bucket().append(&stream, &batch(&refs, None)).await.unwrap().stream;
    at.bucket().append(&stream, &batch(&[b"entry 11", b"entry 12"], Some(id))).await.unwrap();

    let read = everything(&at, &stream, 3).await;
    let mut expected = ten.clone();
    expected.extend(["entry 11".to_string(), "entry 12".to_string()]);
    assert_eq!(texts(&read), expected);

    // Larger than a page in bytes: two entries of 5 MiB, one page each.
    let large = at.fresh("large");
    let id = at.bucket().append(&large, &batch(&[b"small"], None)).await.unwrap().stream;
    let (a, b) = (noise(5 << 20, 1), noise(5 << 20, 2));
    at.bucket().append(&large, &batch(&[&a, &b, b"tail"], Some(id))).await.unwrap();
    let bucket = at.bucket();
    let first = bucket.read(&large, None, 100).await.unwrap();
    assert_eq!(first.entries.len(), 2, "a page holds at most 8 MiB of entries, and at least one");
    let second = bucket.read(&large, first.cursor().as_ref(), 100).await.unwrap();
    assert_eq!(second.entries.len(), 2);
    assert_eq!(second.entries[0].data, b);
    assert_eq!(second.entries[1].data, b"tail");
    assert_eq!(second.head, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_the_bucket_does_not_have_is_refused() {
    let Some(at) = Settings::from_env() else { return };
    let stream = at.fresh("cursors");
    let id = at.bucket().append(&stream, &batch(&[b"a", b"b", b"c"], None)).await.unwrap().stream;
    at.bucket().append(&stream, &batch(&[b"d"], Some(id))).await.unwrap();
    let all = everything(&at, &stream, 100).await;
    let inside = all[1].cursor(id);
    let at_end = all[2].cursor(id);
    assert_eq!(texts(&at.bucket().read(&stream, Some(&inside), 100).await.unwrap().entries), ["c", "d"]);
    assert_eq!(texts(&at.bucket().read(&stream, Some(&at_end), 100).await.unwrap().entries), ["d"]);

    let (code, problem) = refusal(at.bucket().read(&stream, Some(&Cursor { seq: 5, ..at_end }), 100).await);
    assert_eq!((code, problem.head), (ProblemCode::CursorAhead, Some(4)));
    // A real link, from another position.
    let wrong = all[3].hash;
    for cursor in
        [Cursor { hash: wrong, ..inside }, Cursor { hash: wrong, ..at_end }, Cursor { seq: 0, hash: wrong, stream: id }]
    {
        let (code, _) = refusal(at.bucket().read(&stream, Some(&cursor), 100).await);
        assert_eq!(code, ProblemCode::CursorDiverged, "{cursor}");
    }
    let other = StreamId::from_bytes([7; 16]);
    let (code, _) = refusal(at.bucket().read(&stream, Some(&Cursor::start(other)), 100).await);
    assert_eq!(code, ProblemCode::StreamReplaced);
    let (code, _) = refusal(at.bucket().append(&stream, &batch(&[b"e"], Some(other))).await);
    assert_eq!(code, ProblemCode::StreamReplaced);

    let missing = at.fresh("missing");
    let (code, _) = refusal(at.bucket().read(&missing, None, 100).await);
    assert_eq!(code, ProblemCode::StreamNotFound);
    let (code, _) = refusal(at.bucket().append(&missing, &batch(&[b"e"], Some(id))).await);
    assert_eq!(code, ProblemCode::StreamNotFound);
    assert_eq!(texts(&everything(&at, &stream, 100).await), ["a", "b", "c", "d"], "a refused batch was written");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_in_an_older_epoch_is_refused() {
    let Some(at) = Settings::from_env() else { return };
    let stream = at.fresh("epochs");
    let newer = Batch { epoch: Epoch(2), ..batch(&[b"two"], None) };
    let id = at.bucket().append(&stream, &newer).await.unwrap().stream;
    let (code, problem) = refusal(at.bucket().append(&stream, &batch(&[b"one"], Some(id))).await);
    assert_eq!((code, problem.epoch), (ProblemCode::EpochBehind, Some(Epoch(2))));
    let raised = at.bucket().append(&stream, &Batch { epoch: Epoch(3), ..batch(&[b"three"], Some(id)) }).await.unwrap();
    assert_eq!(raised.epoch, Epoch(3));
    let page = at.bucket().read(&stream, None, 100).await.unwrap();
    assert_eq!((page.epoch, texts(&page.entries)), (Epoch(3), vec!["two".to_string(), "three".to_string()]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_wakes_for_a_write_and_ends_without_one() {
    let Some(at) = Settings::from_env() else { return };
    let stream = at.fresh("wait");
    let id = at.bucket().append(&stream, &batch(&[b"one"], None)).await.unwrap().stream;
    let cursor = at.bucket().read(&stream, None, 100).await.unwrap().cursor().unwrap();
    let bucket = at.bucket();

    // A reader behind, or with nothing yet, is told at once.
    let started = Instant::now();
    let behind = bucket.wait(&[Watch { stream: stream.clone(), cursor: None }], Duration::from_secs(30)).await.unwrap();
    assert_eq!((behind.heads.len(), behind.heads[0].head, behind.heads[0].id), (1, 1, Some(id)));
    assert!(started.elapsed() < Duration::from_secs(5));

    // Nothing comes: the wait ends when its time does, with nothing.
    let watch = Watch { stream: stream.clone(), cursor: Some(cursor) };
    let started = Instant::now();
    assert!(bucket.wait(std::slice::from_ref(&watch), Duration::from_secs(2)).await.unwrap().heads.is_empty());
    let waited = started.elapsed();
    assert!(waited >= Duration::from_secs(2) && waited < Duration::from_secs(5), "{waited:?}");

    // Another device writes: the wait wakes within a look of it.
    let writer = at.bucket();
    let (writing, ready) = (stream.clone(), Duration::from_secs(1));
    tokio::spawn(async move {
        tokio::time::sleep(ready).await;
        writer.append(&writing, &batch(&[b"two"], Some(id))).await.unwrap();
    });
    let started = Instant::now();
    let woken = bucket.wait(&[watch], Duration::from_secs(30)).await.unwrap();
    assert_eq!(woken.heads[0].head, 2);
    assert!(started.elapsed() < ready + Bucket::POLL + Duration::from_secs(3), "{:?}", started.elapsed());

    // A stream no one has written: nothing to wake for.
    let quiet = Watch { stream: at.fresh("never"), cursor: None };
    assert!(bucket.wait(&[quiet], Duration::ZERO).await.unwrap().heads.is_empty());
}

/// What crosses the network to the bucket - and so everything the bucket
/// can keep - is sealed: every byte of every request and answer is
/// searched for what the bucket's owner must never have.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_that_reaches_the_bucket_is_plaintext() {
    let Some(at) = Settings::from_env() else { return };
    let recorder = Proxy::start(at.upstream(), None).await;
    let stream = at.fresh("sealed");
    let (laptop, phone) = (Device::new(), Device::new());
    let marker = b"PLAINTEXT-MARKER-the-bucket-must-never-see";
    let on_laptop = laptop.open(at.through(recorder.addr), stream.as_str(), 1).await.with_compression(Compression::Off);
    on_laptop.push([marker.as_slice(), marker.as_slice()]).await.unwrap();
    on_laptop.change_passphrase(NEW_PASSPHRASE).await.unwrap();
    let on_phone = phone
        .try_open(at.through(recorder.addr), stream.as_str(), 1, Secret::Passphrase(NEW_PASSPHRASE))
        .await
        .unwrap();
    assert_eq!(drain(&on_phone).await.len(), 2);

    let traffic = recorder.bytes.lock().unwrap().clone();
    assert!(contains(&traffic, stream.as_str().as_bytes()), "the recorder saw nothing of the stream");
    let key: String = on_laptop.key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    for (what, secret) in [
        ("an item", marker.as_slice()),
        ("the passphrase", PASSPHRASE),
        ("the new passphrase", NEW_PASSPHRASE),
        ("the stream's key", on_laptop.key().to_bytes().as_slice()),
        ("the stream's key in hex", key.as_bytes()),
        ("the bucket's secret", at.secret.as_bytes()),
    ] {
        assert!(!contains(&traffic, secret), "{what} crossed to the bucket");
    }
}

/// A batch whose object says in its metadata one thing and holds another -
/// a writer with a bug, a service that lost a header - is not read as
/// either.
#[tokio::test(flavor = "multi_thread")]
async fn a_bucket_that_says_one_thing_and_holds_another_is_not_believed() {
    let Some(at) = Settings::from_env() else { return };
    let stream = at.fresh("tampered");
    at.bucket().append(&stream, &batch(&[b"a", b"b"], None)).await.unwrap();
    let lying = Proxy::start(at.upstream(), Some((b"x-amz-meta-efema-last: 2\r", b"x-amz-meta-efema-last: 3\r"))).await;
    let read = at.through(lying.addr).read(&stream, None, 100).await;
    assert!(
        matches!(&read, Err(efema::TransportError::NotProtocol { reason, .. }) if reason.contains("metadata")),
        "{read:?}"
    );
    assert!(lying.swapped(), "the proxy changed nothing");
}

/// A stream a newer release laid out in a newer layout is not read as this
/// one, and the app is told to update - not to try again later.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_in_a_newer_layout_is_named_and_not_read() {
    let Some(at) = Settings::from_env() else { return };
    let stream = at.fresh("layout");
    let written = at.bucket().append(&stream, &batch(&[b"a"], None)).await.unwrap();
    let record = |format: u8| -> &'static [u8] {
        Box::leak([&[0xa2, 0x00, 0x50][..], written.stream.as_bytes(), &[0x01, format]].concat().into_boxed_slice())
    };
    let newer = Proxy::start(at.upstream(), Some((record(1), record(2)))).await;
    let read = at.through(newer.addr).read(&stream, None, 100).await.unwrap_err();
    assert!(newer.swapped(), "the proxy changed nothing");
    let error = Error::from(read);
    assert!(matches!(&error, Error::NewerFormat { seq: None, part: efema::Part::Layout, version: 2, .. }), "{error:?}");
    assert!(error.to_string().contains("is in bucket layout 2, newer than this library reads"), "{error}");
}

/// Credentials the bucket lets read and not write: the device reads the
/// stream, and nothing it pushes is kept to be sent - nor can it start a
/// stream.
#[tokio::test(flavor = "multi_thread")]
async fn credentials_that_only_read_pull_and_keep_nothing_to_send() {
    let Some(at) = Settings::from_env() else { return };
    let stream = at.fresh("reader");
    let on_laptop = Device::new().open(at.bucket(), stream.as_str(), 1).await;
    on_laptop.push([b"for everyone".as_slice()]).await.unwrap();

    assert!(!at.for_reading().limits().await.unwrap().write);
    let phone = Device::new();
    let on_phone = phone.open(at.for_reading(), stream.as_str(), 1).await;
    assert_eq!(data(&drain(&on_phone).await), [b"for everyone".as_slice()]);
    let refused = on_phone.push([b"from the phone".as_slice()]).await;
    assert!(matches!(refused, Err(Error::ReadOnly { .. })), "{refused:?}");
    assert!(on_phone.queue().await.unwrap().is_empty());
    let (code, _) = refusal(at.for_reading().append(&stream, &batch(&[b"x"], None)).await);
    assert_eq!(code, ProblemCode::Forbidden);

    let refused =
        phone.try_open(at.for_reading(), at.fresh("unborn").as_str(), 1, Secret::Passphrase(PASSPHRASE)).await;
    assert!(matches!(refused, Err(Error::ReadOnly { .. })), "{refused:?}");
    assert_eq!(everything(&at, &stream, 100).await.len(), 2, "the key entry and one item");
}

#[tokio::test(flavor = "multi_thread")]
async fn credentials_the_bucket_does_not_take_are_refused() {
    let Some(at) = Settings::from_env() else { return };
    let wrong = Bucket::new(at.url.as_str(), &at.region, Credentials::new(&at.id, "not the secret")).unwrap();
    let (code, _) = refusal(wrong.limits().await);
    assert_eq!(code, ProblemCode::Unauthorized);
    let refused = Device::new().try_open(wrong, at.fresh("wrong").as_str(), 1, Secret::Passphrase(PASSPHRASE)).await;
    assert!(matches!(refused, Err(Error::Unauthorized { .. })), "{refused:?}");
}

/// A batch that says which position it starts at lands there or nowhere,
/// and the first batch of a copy makes the stream under the identity it
/// names - as on a relay.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_that_says_where_it_goes_lands_there_or_nowhere() {
    let Some(at) = Settings::from_env() else { return };
    let (stream, bucket) = (at.fresh("placed"), at.bucket());
    let one = batch(&[b"a", b"b"], None);
    let id = bucket.append(&stream, &one).await.unwrap().stream;
    let placed = |first| Batch { first: Some(first), ..batch(&[b"c"], Some(id)) };
    for wrong in [1, 2, 4] {
        let (code, problem) = refusal(bucket.append(&stream, &placed(wrong)).await);
        assert_eq!((code, problem.head), (ProblemCode::OutOfPlace, Some(2)), "at {wrong}");
    }
    let written = bucket.append(&stream, &placed(3)).await.unwrap();
    assert_eq!((written.first, written.repeated), (3, false));
    let again = bucket.append(&stream, &Batch { first: Some(9), stream: Some(id), ..one }).await.unwrap();
    assert_eq!((again.first, again.repeated), (1, true), "a batch the stream has is answered where it is");

    let copy = at.fresh("placed-copy");
    let ours = StreamId::from_bytes([0x42; 16]);
    for wrong in [None, Some(2)] {
        let (code, _) = refusal(bucket.append(&copy, &Batch { first: wrong, ..batch(&[b"x"], Some(ours)) }).await);
        assert_eq!(code, ProblemCode::StreamNotFound, "a batch at {wrong:?} made the stream");
    }
    let made = bucket.append(&copy, &Batch { first: Some(1), ..batch(&[b"x"], Some(ours)) }).await.unwrap();
    assert_eq!((made.stream, made.first), (ours, 1));
    let other = StreamId::from_bytes([0x43; 16]);
    let (code, _) = refusal(bucket.append(&copy, &Batch { first: Some(1), ..batch(&[b"y"], Some(other)) }).await);
    assert_eq!(code, ProblemCode::StreamReplaced);
}

/// A service that takes `If-None-Match` and ignores it - every PUT a
/// success - would let two devices put two batches in one place. The
/// bucket finds that out by creating one object twice, and writes nothing.
/// Not S3 at all, so it needs no bucket: it runs everywhere.
#[tokio::test(flavor = "multi_thread")]
async fn a_bucket_that_lets_an_object_be_created_twice_is_not_written_to() {
    let puts = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = puts.clone();
    let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let seen = seen.clone();
        async move {
            if request.method() == axum::http::Method::PUT {
                seen.lock().unwrap().push(request.uri().path().to_string());
                return axum::http::StatusCode::OK;
            }
            axum::http::StatusCode::NOT_FOUND
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let bucket = Bucket::new(&format!("http://{addr}/lying"), "us-east-1", Credentials::new("id", "secret")).unwrap();
    let refused = bucket.limits().await.unwrap_err();
    assert!(refused.to_string().contains("If-None-Match"), "{refused}");
    let refused = bucket.append(&name("notes"), &batch(&[b"x"], None)).await.unwrap_err();
    assert!(matches!(refused, efema::TransportError::NotProtocol { .. }), "{refused}");
    let puts = puts.lock().unwrap().clone();
    assert!(!puts.is_empty() && puts.iter().all(|path| path == "/lying/.efema-probe"), "{puts:?}");
}

/// A proxy on this machine that keeps every byte it passes, both ways - and,
/// given a pair, swaps the first for the second in what comes back.
struct Proxy {
    addr: SocketAddr,
    bytes: Arc<Mutex<Vec<u8>>>,
    swaps: Arc<Mutex<usize>>,
}

type Swap = (&'static [u8], &'static [u8]);

impl Proxy {
    async fn start(upstream: SocketAddr, swap: Option<Swap>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (bytes, swaps) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(0)));
        let (kept, swapped) = (bytes.clone(), swaps.clone());
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let server = tokio::net::TcpStream::connect(upstream).await.unwrap();
                let (client_read, client_write) = client.into_split();
                let (server_read, server_write) = server.into_split();
                tokio::spawn(pass(client_read, server_write, kept.clone(), None, swapped.clone()));
                tokio::spawn(pass(server_read, client_write, kept.clone(), swap, swapped.clone()));
            }
        });
        Self { addr, bytes, swaps }
    }

    fn swapped(&self) -> bool {
        *self.swaps.lock().unwrap() > 0
    }
}

async fn pass(
    mut from: impl AsyncReadExt + Unpin,
    mut to: impl AsyncWriteExt + Unpin,
    kept: Arc<Mutex<Vec<u8>>>,
    swap: Option<Swap>,
    swaps: Arc<Mutex<usize>>,
) {
    let mut buffer = vec![0; 64 * 1024];
    while let Ok(read) = from.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        let mut piece = buffer[..read].to_vec();
        if let Some((this, that)) = swap
            && let Some(at) = piece.windows(this.len()).position(|window| window.eq_ignore_ascii_case(this))
        {
            piece.splice(at..at + this.len(), that.iter().copied());
            *swaps.lock().unwrap() += 1;
        }
        if to.write_all(&piece).await.is_err() {
            break;
        }
        kept.lock().unwrap().extend_from_slice(&piece);
    }
    let _ = to.shutdown().await;
}
