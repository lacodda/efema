//! Who the relay answers, and how much: access keys and the spaces they
//! open, the limits a key is held to, and the door that plain HTTP from
//! outside does not pass - checked over a real socket.

mod common;

use std::time::{Duration, Instant};

use common::{SPACE, TestRelay};
use efema_proto::wire::{self, Batch, Limits, ProblemCode, Watch, Written};
use efema_proto::{AccessKey, Epoch};
use efema_server::store::Store;
use efema_server::{Access, Config, Network};
use reqwest::StatusCode;

fn watch(text: &str) -> Watch {
    text.parse().unwrap()
}

fn tight(body: usize, entry: usize, quota: u64, rate: u32) -> Config {
    Config { limits: efema_server::Limits { body, entry, quota, rate }, ..Config::default() }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_key_the_relay_answers_nothing_under_v1() {
    let relay = TestRelay::start().await;
    relay.write("notes", 1, &[b"a"]).await.unwrap();
    let stranger = AccessKey::from_bytes([7; 32]);
    for key in [None, Some(&stranger)] {
        let device = relay.with(key);
        let refusals = [
            device.read("notes", None, None).await.unwrap_err(),
            device.write("notes", 1, &[b"b"]).await.unwrap_err(),
            device.write("fresh", 1, &[b"b"]).await.unwrap_err(),
            device.wait(&[watch("notes")], 0).await.unwrap_err(),
            device.limits().await.unwrap_err(),
        ];
        for refused in refusals {
            assert_eq!((refused.status, refused.problem.code), (StatusCode::UNAUTHORIZED, ProblemCode::Unauthorized));
        }
    }
    let typed_wrong = [
        "Basic dXNlcjpwYXNz".to_string(),
        "Bearer".to_string(),
        "Bearer efema_0123".to_string(),
        format!("Token {}", relay.key.expose()),
        relay.key.expose(),
    ];
    for header in typed_wrong {
        let response =
            relay.with(None).client.get(relay.url("/v1/limits")).header("authorization", &header).send().await.unwrap();
        assert_eq!(
            response.headers().get("www-authenticate").and_then(|v| v.to_str().ok()),
            Some("Bearer realm=\"efema\""),
            "{header}"
        );
        let refused = common::answer::<Limits>(response).await.unwrap_err();
        assert_eq!(refused.problem.code, ProblemCode::Unauthorized, "{header}");
    }
    assert_eq!(relay.read_all("notes", None, 10).await.len(), 1, "a refused write wrote");
    assert_eq!(relay.read("fresh", None, None).await.unwrap_err().problem.code, ProblemCode::StreamNotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_key_opens_nothing_from_its_next_request() {
    let relay = TestRelay::start().await;
    let phone = relay.key(SPACE, Access::Write);
    let device = relay.with(Some(&phone));
    device.write("notes", 1, &[b"a"]).await.unwrap();
    // Revoked the way the operator does it: in the database, while the
    // relay runs.
    Store::open(relay.dir.path()).unwrap().revoke(&phone.id()).unwrap();
    let refused = device.read("notes", None, None).await.unwrap_err();
    assert_eq!(refused.problem.code, ProblemCode::Unauthorized);
    assert_eq!(relay.read_all("notes", None, 10).await.len(), 1, "revoking one key revoked another");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_that_reads_does_not_write() {
    let relay = TestRelay::start().await;
    let written = relay.write("notes", 1, &[b"a"]).await.unwrap();
    let reader = relay.with(Some(&relay.key(SPACE, Access::Read)));
    assert_eq!(reader.read_all("notes", None, 10).await.len(), 1);
    assert_eq!(reader.wait(&[watch("notes")], 0).await.unwrap().heads.len(), 1);
    assert!(!reader.limits().await.unwrap().write, "a key that reads is announced as one");

    for (stream, epoch) in [("notes", 1), ("notes", 9), ("fresh", 1)] {
        let refused = reader.write(stream, epoch, &[b"x"]).await.unwrap_err();
        assert_eq!((refused.status, refused.problem.code), (StatusCode::FORBIDDEN, ProblemCode::Forbidden));
    }
    let page = relay.read("notes", None, None).await.unwrap();
    assert_eq!(
        (page.head, page.epoch, page.stream),
        (1, Epoch(1), written.stream),
        "a key that reads moved the stream"
    );
    assert_eq!(
        relay.read("fresh", None, None).await.unwrap_err().problem.code,
        ProblemCode::StreamNotFound,
        "a key that reads created a stream"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_is_closed_to_the_keys_of_another() {
    let relay = std::sync::Arc::new(TestRelay::start().await);
    let ours = relay.write("notes", 1, &[b"ours"]).await.unwrap();
    let cursor = relay.read("notes", None, None).await.unwrap().cursor().unwrap();
    let theirs = relay.with(Some(&relay.key("other", Access::Write)));

    // Our stream is not there for them: not to read, not from our cursor,
    // not to write into by its identity.
    let not_found = |refused: common::Refused| refused.problem.code;
    assert_eq!(not_found(theirs.read("notes", None, None).await.unwrap_err()), ProblemCode::StreamNotFound);
    assert_eq!(not_found(theirs.read("notes", Some(&cursor), None).await.unwrap_err()), ProblemCode::StreamNotFound);
    assert_eq!(
        not_found(theirs.write_to("notes", ours.stream, 1, &[b"x"]).await.unwrap_err()),
        ProblemCode::StreamNotFound
    );

    // A wait of ours sleeps through their writes to their own `notes`.
    let waiter = {
        let relay = relay.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let heads = relay.wait(&[Watch { stream: "notes".parse().unwrap(), cursor: Some(cursor) }], 2).await;
            (heads, started.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let their = theirs.write("notes", 1, &[b"theirs, longer"]).await.unwrap();
    assert_ne!(their.stream, ours.stream, "a name is its own in each space");
    let (heads, elapsed) = waiter.await.unwrap();
    assert!(heads.unwrap().heads.is_empty(), "a write in another space woke our wait");
    assert!(elapsed >= Duration::from_millis(1900), "woken early: {elapsed:?}");

    let ours_read: Vec<Vec<u8>> = relay.read_all("notes", None, 10).await.into_iter().map(|e| e.data).collect();
    let theirs_read: Vec<Vec<u8>> = theirs.read_all("notes", None, 10).await.into_iter().map(|e| e.data).collect();
    assert_eq!((ours_read, theirs_read), (vec![b"ours".to_vec()], vec![b"theirs, longer".to_vec()]));
    assert_eq!(relay.with(Some(&relay.key)).limits().await.unwrap().used, 4, "space usage leaked across spaces");
    assert_eq!(theirs.limits().await.unwrap().used, 14);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_relay_announces_what_it_holds_a_key_to() {
    let relay = TestRelay::start_with(tight(4096, 1024, 10_000, 100)).await;
    relay.write("a", 1, &[&[0u8; 300]]).await.unwrap();
    relay.write("b", 1, &[&[0u8; 200]]).await.unwrap();
    let limits = relay.with(Some(&relay.key)).limits().await.unwrap();
    assert_eq!(
        limits,
        Limits { write: true, body: 4096, entry: 1024, quota: 10_000, used: 500, rate: 100, ..Limits::DEFAULT }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_over_the_operators_limits_is_refused_with_the_limit() {
    let relay = TestRelay::start_with(tight(4096, 1024, u64::MAX, 600)).await;
    let refused = relay.write("notes", 1, &[&[1u8; 1025]]).await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::PAYLOAD_TOO_LARGE, ProblemCode::TooLarge));
    assert_eq!(refused.problem.limit, Some(1024));
    let entries = [[2u8; 1000]; 5];
    let entries: Vec<&[u8]> = entries.iter().map(|e| e.as_slice()).collect();
    let refused = relay.write("notes", 1, &entries).await.unwrap_err();
    assert_eq!((refused.problem.code, refused.problem.limit), (ProblemCode::TooLarge, Some(4096)));
    assert_eq!(relay.read("notes", None, None).await.unwrap_err().problem.code, ProblemCode::StreamNotFound);
    // Each limit to the byte.
    relay.write("notes", 1, &[&[1u8; 1024]]).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_written_before_the_limits_came_down_is_still_answered() {
    let relay = TestRelay::start().await;
    let batch = Batch { epoch: Epoch(1), entries: vec![vec![1u8; 3000], vec![2u8; 3000]], stream: None };
    let written = relay.write_raw("notes", wire::encode(&batch)).await.unwrap();
    let dir = relay.stop().await;

    // Every limit below what the batch took, and the space full.
    let relay = TestRelay::start_in_with(dir, tight(1000, 500, 100, 600)).await;
    let again = relay.write_raw("notes", wire::encode(&batch)).await.unwrap();
    assert_eq!(again, Written { repeated: true, ..written }, "a repeat was held to limits that came after it");
    let named = Batch { stream: Some(written.stream), ..batch };
    assert!(relay.write_raw("notes", wire::encode(&named)).await.unwrap().repeated);
    // So `too_large` and `quota_exceeded` say the stream does not have it.
    let refused = relay.write_to("notes", written.stream, 1, &[b"new"]).await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::INSUFFICIENT_STORAGE, ProblemCode::QuotaExceeded));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_space_refuses_new_batches_and_no_other_space_feels_it() {
    let relay = TestRelay::start_with(tight(4096, 1024, 1000, 600)).await;
    let first = relay.write("a", 1, &[&[1u8; 600]]).await.unwrap();
    relay.write("b", 1, &[&[2u8; 400]]).await.unwrap();
    let refused = relay.write_to("a", first.stream, 1, &[b"x"]).await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::INSUFFICIENT_STORAGE, ProblemCode::QuotaExceeded));
    assert_eq!(refused.problem.limit, Some(1000));
    let refused = relay.write("c", 1, &[b"x"]).await.unwrap_err();
    assert_eq!(refused.problem.code, ProblemCode::QuotaExceeded, "a new stream got around the quota");
    assert_eq!(relay.read("c", None, None).await.unwrap_err().problem.code, ProblemCode::StreamNotFound);

    let other = relay.with(Some(&relay.key("other", Access::Write)));
    other.write("a", 1, &[&[3u8; 1000]]).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_that_asks_too_often_is_told_when_to_ask_again() {
    let relay = TestRelay::start_with(tight(4096, 1024, u64::MAX, 3)).await;
    let device = relay.with(Some(&relay.key));
    for _ in 0..3 {
        device.limits().await.unwrap();
    }
    let response = device.client.get(relay.url("/v1/limits")).send().await.unwrap();
    let retry_after: u64 = response.headers().get("retry-after").unwrap().to_str().unwrap().parse().unwrap();
    let refused = common::answer::<Limits>(response).await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::TOO_MANY_REQUESTS, ProblemCode::RateLimited));
    assert_eq!(refused.problem.retry, Some(retry_after));
    assert!((1..=20).contains(&retry_after), "one request comes back every 20 seconds: {retry_after}");

    // Another key has a minute of its own; no key is refused for that, not
    // for the rate.
    relay.with(Some(&relay.key(SPACE, Access::Read))).limits().await.unwrap();
    let refused = relay.with(None).limits().await.unwrap_err();
    assert_eq!(refused.problem.code, ProblemCode::Unauthorized);
}

/// A write to `notes` with the relay's key and `headers`, as a proxy in front
/// of it would forward it.
async fn forwarded(relay: &TestRelay, headers: &[(&str, &str)]) -> Result<Written, common::Refused> {
    let batch = Batch { epoch: Epoch(1), entries: vec![b"through the door".to_vec()], stream: None };
    let mut request = relay
        .client
        .post(relay.url("/v1/streams/notes/entries"))
        .header("content-type", efema_proto::MEDIA_TYPE)
        .body(wire::encode(&batch));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    common::answer(request.send().await.unwrap()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_http_from_outside_the_door_is_refused_before_anything_else() {
    let relay = TestRelay::start().await;
    let plain = [
        vec![("x-forwarded-proto", "http"), ("x-forwarded-for", "203.0.113.9")],
        vec![("forwarded", "for=203.0.113.9;proto=http")],
        vec![("x-forwarded-proto", "https, http")],
        // A proxy that forwards without saying how the request reached it.
        vec![("x-forwarded-for", "203.0.113.9")],
    ];
    for headers in &plain {
        let refused = forwarded(&relay, headers).await.unwrap_err();
        assert_eq!(
            (refused.status, refused.problem.code),
            (StatusCode::FORBIDDEN, ProblemCode::HttpsRequired),
            "{headers:?}"
        );
    }
    // Before the key: a wrong one gets the same answer, not `unauthorized`.
    let response = relay
        .with(Some(&AccessKey::from_bytes([1; 32])))
        .client
        .get(relay.url("/health"))
        .header("x-forwarded-proto", "http")
        .send()
        .await
        .unwrap();
    assert_eq!(common::answer::<Limits>(response).await.unwrap_err().problem.code, ProblemCode::HttpsRequired);
    assert_eq!(relay.read("notes", None, None).await.unwrap_err().problem.code, ProblemCode::StreamNotFound);

    let https = [("x-forwarded-proto", "https"), ("x-forwarded-for", "203.0.113.9")];
    forwarded(&relay, &https).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_answers_only_peers_it_trusts() {
    // Trusting nobody, the relay takes even this machine for another one:
    // what it says to a device across the network.
    let relay = TestRelay::start_with(Config { trusted: vec![], ..Config::default() }).await;
    let refused = relay.read("notes", None, None).await.unwrap_err();
    assert_eq!((refused.status, refused.problem.code), (StatusCode::FORBIDDEN, ProblemCode::HttpsRequired));
    assert!(refused.problem.message.contains("plain HTTP from 127.0.0.1"), "{}", refused.problem.message);
    let refused = forwarded(&relay, &[("x-forwarded-proto", "https")]).await.unwrap_err();
    assert_eq!(refused.problem.code, ProblemCode::HttpsRequired, "an untrusted peer was believed");
    let health = relay.client.get(relay.url("/health")).send().await.unwrap();
    assert_eq!(health.status(), StatusCode::FORBIDDEN);

    // A proxy named by its address is trusted with HTTPS.
    let network: Network = "127.0.0.1/32".parse().unwrap();
    let relay = TestRelay::start_with(Config { trusted: vec![network], ..Config::default() }).await;
    forwarded(&relay, &[("x-forwarded-proto", "https")]).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_count_without_contents_and_only_on_this_machine() {
    let relay = TestRelay::start().await;
    relay.write("a-private-name", 1, &[b"what the entry holds"]).await.unwrap();
    relay.key("other", Access::Read);

    // No key: the metrics are the operator's, on the operator's machine.
    let response = relay.with(None).client.get(relay.url("/metrics")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-type"].to_str().unwrap().starts_with("text/plain; version=0.0.4"));
    let text = response.text().await.unwrap();
    for line in [
        "efema_spaces 2",
        "efema_streams{space=\"test\"} 1",
        "efema_entries{space=\"test\"} 1",
        "efema_bytes{space=\"test\"} 20",
        "efema_keys{space=\"test\"} 1",
        "efema_keys{space=\"other\"} 1",
        "efema_streams{space=\"other\"} 0",
        &format!("efema_quota_bytes {}", efema_server::limits::SPACE_QUOTA_DEFAULT),
        &format!("efema_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION")),
    ] {
        assert!(text.lines().any(|l| l == line), "no `{line}` in:\n{text}");
    }
    for name in [
        "efema_last_write_seconds{space=\"test\"}",
        "efema_last_use_seconds{space=\"test\"}",
        "efema_start_time_seconds",
    ] {
        assert!(text.lines().any(|l| l.starts_with(name)), "no `{name}` in:\n{text}");
    }
    assert!(!text.contains("efema_last_write_seconds{space=\"other\"}"), "a space never written has no last write");
    assert!(!text.contains("a-private-name") && !text.contains("what the entry holds"), "{text}");

    // Through the door, refused - with the relay's key or without.
    for client in [relay.client.clone(), relay.with(None).client] {
        let response = client.get(relay.url("/metrics")).header("x-forwarded-proto", "https").send().await.unwrap();
        let refused = common::answer::<Limits>(response).await.unwrap_err();
        assert_eq!((refused.status, refused.problem.code), (StatusCode::FORBIDDEN, ProblemCode::Forbidden));
    }
}
