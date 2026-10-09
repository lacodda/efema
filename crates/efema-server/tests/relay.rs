//! The relay's promises, checked over a real socket.
//!
//! Each test starts a relay on a free port in a fresh directory and talks to
//! it over HTTP, as any client would.

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use common::TestRelay;
use efema_proto::wire::{self, Batch, ProblemCode, Watch};
use efema_proto::{Cursor, Epoch, Hash, StreamId, chain};
use reqwest::StatusCode;

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_comes_back_as_it_went_in() {
    let relay = TestRelay::start().await;
    let entries: [&[u8]; 3] = [b"first", b"", &[0, 255, 0, 255]];
    let written = relay.write("notes", 1, &entries).await.unwrap();
    assert_eq!((written.first, written.last, written.epoch), (1, 3, Epoch(1)));

    let page = relay.read("notes", None, None).await.unwrap();
    assert_eq!(page.stream, written.stream);
    assert_eq!(page.head, 3);
    let data: Vec<&[u8]> = page.entries.iter().map(|e| e.data.as_slice()).collect();
    assert_eq!(data, entries);
    let seqs: Vec<u64> = page.entries.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [1, 2, 3]);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_entry_is_linked_to_the_one_before() {
    let relay = TestRelay::start().await;
    relay.write("notes", 1, &[b"a", b"b"]).await.unwrap();
    relay.write("notes", 2, &[b"c"]).await.unwrap();

    let page = relay.read("notes", None, None).await.unwrap();
    let mut previous = chain::genesis(&page.stream);
    for entry in &page.entries {
        assert_eq!(entry.hash, chain::link(&previous, entry.seq, entry.epoch, &entry.data));
        previous = entry.hash;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_after_a_cursor_returns_only_what_came_later() {
    let relay = TestRelay::start().await;
    relay.write("notes", 1, &[b"a", b"b"]).await.unwrap();
    let cursor = relay.read("notes", None, None).await.unwrap().cursor().unwrap();
    relay.write("notes", 1, &[b"c"]).await.unwrap();

    let page = relay.read("notes", Some(&cursor), None).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!((page.entries[0].seq, page.entries[0].data.as_slice()), (3, &b"c"[..]));

    // At the head, a read is empty - and leaves the cursor where it was.
    let at_head = relay.read("notes", page.cursor().as_ref(), None).await.unwrap();
    assert!(at_head.entries.is_empty());
    assert_eq!(at_head.cursor(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_at_the_start_of_a_stream_is_a_cursor() {
    let relay = TestRelay::start().await;
    let written = relay.write("notes", 1, &[b"a"]).await.unwrap();
    let page = relay.read("notes", Some(&Cursor::start(written.stream)), None).await.unwrap();
    assert_eq!(page.entries.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writers_get_one_order_with_no_gaps_and_whole_batches() {
    let relay = std::sync::Arc::new(TestRelay::start().await);
    let writers = 8;
    let batches = 12;
    let mut tasks = Vec::new();
    for writer in 0..writers {
        let relay = relay.clone();
        tasks.push(tokio::spawn(async move {
            let mut landed = Vec::new();
            for batch in 0..batches {
                // Each batch is three entries tagged with who wrote it.
                let tag = |part: u8| vec![writer as u8, batch as u8, part];
                let entries = [tag(0), tag(1), tag(2)];
                let refs: Vec<&[u8]> = entries.iter().map(Vec::as_slice).collect();
                let written = relay.write("shared", 1, &refs).await.unwrap();
                assert_eq!(written.last - written.first, 2);
                landed.push((written.first, writer as u8, batch as u8));
            }
            landed
        }));
    }
    let mut landed = Vec::new();
    for task in tasks {
        landed.extend(task.await.unwrap());
    }

    let total = (writers * batches * 3) as u64;
    let everything = relay.read_all("shared", None, 10_000).await;
    let seqs: Vec<u64> = everything.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=total).collect::<Vec<_>>(), "positions are 1..=n, each once, in order");

    // A batch occupies consecutive positions: nobody else's entry lands inside.
    for (first, writer, batch) in landed {
        for part in 0..3u8 {
            assert_eq!(everything[(first - 1) as usize + part as usize].data, vec![writer, batch, part]);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_readers_with_different_pages_see_one_sequence() {
    let relay = TestRelay::start().await;
    for i in 0..50u32 {
        let a = i.to_be_bytes();
        let b = (i * 7).to_be_bytes();
        relay.write("notes", 1, &[&a, &b]).await.unwrap();
    }
    let one_by_one = relay.read_all("notes", None, 1).await;
    let in_sevens = relay.read_all("notes", None, 7).await;
    let all_at_once = relay.read_all("notes", None, 10_000).await;
    assert_eq!(one_by_one.len(), 100);
    assert_eq!(one_by_one, in_sevens);
    assert_eq!(one_by_one, all_at_once);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_ends_at_its_byte_budget_and_still_carries_a_huge_entry() {
    let relay = TestRelay::start().await;
    // Each batch stays under the body limit; together they pass the page's
    // byte budget, so one read cannot carry them all.
    let chunk = vec![7u8; 3 * 1024 * 1024];
    for _ in 0..4 {
        relay.write("big", 1, &[&chunk]).await.unwrap();
    }
    let page = relay.read("big", None, None).await.unwrap();
    assert!(page.entries.len() < 4, "8 MiB of budget cannot hold 12 MiB");
    assert!(!page.entries.is_empty());
    assert_eq!(page.head, 4);
    assert_eq!(relay.read_all("big", None, 1000).await.len(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn epochs_only_go_up() {
    let relay = TestRelay::start().await;
    relay.write("notes", 1, &[b"old format"]).await.unwrap();

    // A newer client raises the stream...
    let raised = relay.write("notes", 2, &[b"new format"]).await.unwrap();
    assert_eq!(raised.epoch, Epoch(2));

    // ...after which an older one is refused rather than mixed in.
    let refused = relay.write("notes", 1, &[b"old again"]).await.unwrap_err();
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.problem.code, ProblemCode::EpochBehind);
    assert_eq!(refused.problem.epoch, Some(Epoch(2)));

    // Every entry says the epoch it was written in, so a reader that knows
    // only epoch 1 can stop before the first entry it cannot read.
    let page = relay.read("notes", None, None).await.unwrap();
    assert_eq!(page.epoch, Epoch(2));
    let epochs: Vec<Epoch> = page.entries.iter().map(|e| e.epoch).collect();
    assert_eq!(epochs, [Epoch(1), Epoch(2)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_from_another_incarnation_is_refused() {
    let relay = TestRelay::start().await;
    relay.write("notes", 1, &[b"a"]).await.unwrap();
    let foreign = Cursor::start(StreamId::from_bytes([9; 16]));
    let refused = relay.read("notes", Some(&foreign), None).await.unwrap_err();
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.problem.code, ProblemCode::StreamReplaced);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_past_the_head_is_refused_with_the_head() {
    let relay = TestRelay::start().await;
    let written = relay.write("notes", 1, &[b"a", b"b"]).await.unwrap();
    let ahead = Cursor { stream: written.stream, seq: 5, hash: Hash::from_bytes([0; 32]) };
    let refused = relay.read("notes", Some(&ahead), None).await.unwrap_err();
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.problem.code, ProblemCode::CursorAhead);
    assert_eq!(refused.problem.head, Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_with_another_history_is_refused() {
    let relay = TestRelay::start().await;
    relay.write("notes", 1, &[b"a", b"b"]).await.unwrap();
    let mut cursor = relay.read("notes", None, Some(1)).await.unwrap().cursor().unwrap();
    // Same stream, same position, different link: the history this reader
    // holds is not the relay's.
    cursor.hash = chain::link(&chain::genesis(&cursor.stream), 1, Epoch(1), b"not a");
    let refused = relay.read("notes", Some(&cursor), None).await.unwrap_err();
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.problem.code, ProblemCode::CursorDiverged);

    // And at position 0 the link is the genesis link, checked like any other.
    let mut start = Cursor::start(cursor.stream);
    start.hash = Hash::from_bytes([1; 32]);
    let refused = relay.read("notes", Some(&start), None).await.unwrap_err();
    assert_eq!(refused.problem.code, ProblemCode::CursorDiverged);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_that_does_not_exist_is_not_found() {
    let relay = TestRelay::start().await;
    let refused = relay.read("nothing", None, None).await.unwrap_err();
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    assert_eq!(refused.problem.code, ProblemCode::StreamNotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_are_separate() {
    let relay = TestRelay::start().await;
    let a = relay.write("a", 1, &[b"for a"]).await.unwrap();
    let b = relay.write("b", 5, &[b"for b", b"also b"]).await.unwrap();
    assert_ne!(a.stream, b.stream);
    assert_eq!((b.first, b.last), (1, 2), "positions are per stream");
    assert_eq!(relay.read("a", None, None).await.unwrap().entries.len(), 1);
    // A cursor of one stream means nothing to another.
    let cursor_of_a = Cursor::start(a.stream);
    assert_eq!(relay.read("b", Some(&cursor_of_a), None).await.unwrap_err().problem.code, ProblemCode::StreamReplaced);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_writer_that_names_its_stream_writes_only_into_that_one() {
    let mut relay = TestRelay::start().await;
    let first = relay.write("notes", 1, &[b"a"]).await.unwrap();
    let written = relay.write_to("notes", first.stream, 1, &[b"b"]).await.unwrap();
    assert_eq!((written.first, written.stream), (2, first.stream));

    // The relay loses its data and starts again: the name is free, then taken
    // by a new incarnation. A writer that knew the old one is refused both
    // times, and its batch never becomes the start of someone else's stream.
    // An empty directory stands for the lost one. (Deleting the old one
    // instead races Windows, which can hold a file just closed for a moment
    // longer.)
    drop(relay.stop().await);
    relay = TestRelay::start().await;
    let refused = relay.write_to("notes", first.stream, 1, &[b"c"]).await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::NOT_FOUND, ProblemCode::StreamNotFound));
    assert_eq!(relay.read("notes", None, None).await.unwrap_err().problem.code, ProblemCode::StreamNotFound);

    relay.write("notes", 1, &[b"fresh"]).await.unwrap();
    let refused = relay.write_to("notes", first.stream, 1, &[b"c"]).await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::CONFLICT, ProblemCode::StreamReplaced));
    assert_eq!(relay.read("notes", None, None).await.unwrap().head, 1, "nothing refused was written");
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_requests_are_refused_and_say_why() {
    let relay = TestRelay::start().await;
    let code = |r: Result<_, common::Refused>| {
        let refused = r.unwrap_err();
        (refused.status, refused.problem.code)
    };
    let bad = (StatusCode::BAD_REQUEST, ProblemCode::BadRequest);

    assert_eq!(code(relay.write("notes", 1, &[]).await), bad, "an empty batch");
    assert_eq!(code(relay.write("Notes", 1, &[b"x"]).await), bad, "an invalid name");
    assert_eq!(code(relay.write_raw("notes", b"not cbor".to_vec()).await), bad, "a body that is not CBOR");
    let mut trailing = wire::encode(&Batch { epoch: Epoch(1), entries: vec![b"x".to_vec()], stream: None });
    trailing.push(0);
    assert_eq!(code(relay.write_raw("notes", trailing).await), bad, "trailing bytes");

    relay.write("notes", 1, &[b"x"]).await.unwrap();
    for query in ["after=12", "limit=0", "limit=10001", "limit=x", "lmit=5"] {
        let response = relay.client.get(relay.url(&format!("/v1/streams/notes/entries?{query}"))).send().await.unwrap();
        let refused = common::answer::<wire::Page>(response).await.unwrap_err();
        assert_eq!((refused.status, refused.problem.code), bad, "query {query}");
    }
    assert_eq!(relay.read("notes", None, None).await.unwrap().entries.len(), 1, "nothing refused was written");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_is_sent_as_cbor() {
    let relay = TestRelay::start().await;
    let body = wire::encode(&Batch { epoch: Epoch(1), entries: vec![b"x".to_vec()], stream: None });
    let response = relay
        .client
        .post(relay.url("/v1/streams/notes/entries"))
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    let refused = common::answer::<wire::Written>(response).await.unwrap_err();
    assert_eq!(refused.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(refused.problem.code, ProblemCode::UnsupportedMediaType);

    // Parameters on the media type do not change what it is.
    let response = relay
        .client
        .post(relay.url("/v1/streams/notes/entries"))
        .header("content-type", "Application/CBOR; charset=binary")
        .body(body)
        .send()
        .await
        .unwrap();
    assert!(common::answer::<wire::Written>(response).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_over_the_limit_is_refused_with_the_limit() {
    let relay = TestRelay::start().await;
    let huge = vec![0u8; efema_server::limits::MAX_BODY_BYTES + 1];
    let refused = relay.write("notes", 1, &[&huge]).await.unwrap_err();
    assert_eq!(refused.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(refused.problem.code, ProblemCode::TooLarge);
    assert_eq!(refused.problem.limit, Some(efema_server::limits::MAX_BODY_BYTES as u64));
    assert_eq!(relay.read("notes", None, None).await.unwrap_err().problem.code, ProblemCode::StreamNotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_paths_and_methods_are_refused_in_cbor() {
    let relay = TestRelay::start().await;
    let response = relay.client.get(relay.url("/v1/nowhere")).send().await.unwrap();
    let refused = common::answer::<wire::Page>(response).await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::NOT_FOUND, ProblemCode::NotFound));

    let response = relay.client.delete(relay.url("/v1/streams/notes/entries")).send().await.unwrap();
    let refused = common::answer::<wire::Page>(response).await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::METHOD_NOT_ALLOWED, ProblemCode::MethodNotAllowed));
}

#[tokio::test(flavor = "multi_thread")]
async fn health_answers() {
    let relay = TestRelay::start().await;
    let response = relay.client.get(relay.url("/health")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok\n");
}

fn watch(text: &str) -> Watch {
    text.parse().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_returns_at_once_when_there_is_something_to_read() {
    let relay = TestRelay::start().await;
    let written = relay.write("notes", 1, &[b"a"]).await.unwrap();
    let started = Instant::now();
    let heads = relay.wait(&[watch("notes")], 30).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(heads.heads.len(), 1);
    assert_eq!((heads.heads[0].id, heads.heads[0].head), (Some(written.stream), 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_wakes_on_a_write() {
    let relay = std::sync::Arc::new(TestRelay::start().await);
    let written = relay.write("notes", 1, &[b"a"]).await.unwrap();
    let cursor = relay.read("notes", None, None).await.unwrap().cursor().unwrap();

    let waiter = {
        let relay = relay.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let heads = relay.wait(&[Watch { stream: "notes".parse().unwrap(), cursor: Some(cursor) }], 30).await;
            (heads, started.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished(), "nothing new yet, so the waiter sleeps");
    relay.write("notes", 1, &[b"b"]).await.unwrap();

    let (heads, elapsed) = waiter.await.unwrap();
    let heads = heads.unwrap();
    assert!(elapsed < Duration::from_secs(10), "woken by the write, not by the timeout");
    assert_eq!((heads.heads[0].id, heads.heads[0].head), (Some(written.stream), 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_for_a_stream_nobody_wrote_yet_wakes_on_its_first_entry() {
    let relay = std::sync::Arc::new(TestRelay::start().await);
    let waiter = {
        let relay = relay.clone();
        tokio::spawn(async move { relay.wait(&[watch("later")], 30).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished());
    relay.write("later", 1, &[b"here"]).await.unwrap();
    let heads = waiter.await.unwrap().unwrap();
    assert_eq!(heads.heads[0].head, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_runs_out_with_nothing_to_report() {
    let relay = TestRelay::start().await;
    let started = Instant::now();
    let heads = relay.wait(&[watch("quiet")], 1).await.unwrap();
    assert!(heads.heads.is_empty());
    assert!(started.elapsed() >= Duration::from_millis(900));

    // Zero means "just look".
    let started = Instant::now();
    assert!(relay.wait(&[watch("quiet")], 0).await.unwrap().heads.is_empty());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_reports_every_watched_stream_that_moved_and_only_those() {
    let relay = TestRelay::start().await;
    let a = relay.write("a", 1, &[b"1"]).await.unwrap();
    relay.write("b", 1, &[b"1"]).await.unwrap();
    let at_a = Cursor { stream: a.stream, seq: 1, hash: relay.read("a", None, None).await.unwrap().entries[0].hash };
    // The waiter is up to date on `a`, has nothing of `b` and watches `c`,
    // which does not exist.
    let watches = [Watch { stream: "a".parse().unwrap(), cursor: Some(at_a) }, watch("b"), watch("c")];
    let heads = relay.wait(&watches, 0).await.unwrap();
    let moved: HashSet<String> = heads.heads.iter().map(|h| h.stream.to_string()).collect();
    assert_eq!(moved, HashSet::from(["b".to_string()]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_with_a_cursor_from_a_gone_stream_wakes_at_once() {
    // The relay lost the stream (a wiped directory, say): sleeping on would
    // leave the reader waiting for a stream that will never move. It wakes,
    // reads, and learns what happened.
    let relay = TestRelay::start().await;
    let gone = Cursor::start(StreamId::from_bytes([3; 16]));
    let heads = relay.wait(&[Watch { stream: "notes".parse().unwrap(), cursor: Some(gone) }], 30).await.unwrap();
    assert_eq!(heads.heads.len(), 1);
    assert_eq!((heads.heads[0].id, heads.heads[0].head), (None, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn waits_are_refused_when_malformed() {
    let relay = TestRelay::start().await;
    for query in
        ["", "timeout=5", "watch=Bad", "watch=a&watch=a", "watch=a&timeout=61", "watch=a&timeout=x", "watch=a&other=1"]
    {
        let response = relay.client.get(relay.url(&format!("/v1/wait?{query}"))).send().await.unwrap();
        let refused = common::answer::<wire::Heads>(response).await.unwrap_err();
        assert_eq!(
            (refused.status, refused.problem.code),
            (StatusCode::BAD_REQUEST, ProblemCode::BadRequest),
            "{query}"
        );
    }
    let too_many: Vec<String> = (0..=efema_server::limits::WATCHES_MAX).map(|i| format!("watch=s{i}")).collect();
    let response = relay.client.get(relay.url(&format!("/v1/wait?{}", too_many.join("&")))).send().await.unwrap();
    assert!(common::answer::<wire::Heads>(response).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_percent_encoded_cursor_is_the_same_cursor() {
    let relay = TestRelay::start().await;
    relay.write("notes", 1, &[b"a", b"b"]).await.unwrap();
    let cursor = relay.read("notes", None, Some(1)).await.unwrap().cursor().unwrap();
    let watch = format!("notes%3A{cursor}");
    let response = relay.client.get(relay.url(&format!("/v1/wait?watch={watch}&timeout=0"))).send().await.unwrap();
    let heads = common::answer::<wire::Heads>(response).await.unwrap();
    assert_eq!(heads.heads[0].head, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_relay_answers_waits_instead_of_dropping_them() {
    let mut relay = TestRelay::start().await;
    let client = relay.client.clone();
    let url = relay.url("/v1/wait?watch=quiet&timeout=60");
    let waiter = tokio::spawn(async move { client.get(url).send().await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    relay.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(10), "shutdown did not sit out the wait");
    let response = waiter.await.unwrap().unwrap();
    let heads = common::answer::<wire::Heads>(response).await.unwrap();
    assert!(heads.heads.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_relay_has_everything_and_honours_old_cursors() {
    let relay = TestRelay::start().await;
    let written = relay.write("notes", 3, &[b"a", b"b"]).await.unwrap();
    let cursor = relay.read("notes", None, Some(1)).await.unwrap().cursor().unwrap();
    let dir = relay.stop().await;

    let relay = TestRelay::start_in(dir).await;
    let page = relay.read("notes", Some(&cursor), None).await.unwrap();
    assert_eq!(page.stream, written.stream, "the identity survives a restart");
    assert_eq!(page.epoch, Epoch(3));
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].data, b"b");
    let next = relay.write("notes", 3, &[b"c"]).await.unwrap();
    assert_eq!(next.first, 3, "positions continue where they stopped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_is_served_by_one_relay_at_a_time() {
    let relay = TestRelay::start().await;
    match efema_server::Relay::open(relay.dir.path()) {
        Err(efema_server::OpenError::Busy(_)) => {}
        Err(other) => panic!("expected Busy, got {other}"),
        Ok(_) => panic!("a second relay opened a directory that is being served"),
    }
}
