//! A real relay on a real socket, and a client that speaks to it the way any
//! client would: HTTP and CBOR, nothing reached around.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use efema_proto::wire::{self, Batch, Heads, Page, Problem, Watch, Written};
use efema_proto::{AccessKey, Cursor, Epoch, MEDIA_TYPE, StreamId};
use efema_server::store::Store;
use efema_server::{Access, Config, Relay};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// The space a test relay's own key opens.
pub const SPACE: &str = "test";

pub struct TestRelay {
    pub addr: SocketAddr,
    /// Speaks with a key that writes to [`SPACE`].
    pub client: reqwest::Client,
    pub key: AccessKey,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
    // Dropped last, after the relay has stopped and released its files.
    pub dir: TempDir,
}

/// A refusal as the client sees it.
#[derive(Debug)]
pub struct Refused {
    pub status: StatusCode,
    pub problem: Problem,
}

impl TestRelay {
    pub async fn start() -> Self {
        Self::start_in(tempfile::tempdir().unwrap()).await
    }

    pub async fn start_with(config: Config) -> Self {
        Self::start_in_with(tempfile::tempdir().unwrap(), config).await
    }

    /// Starts a relay over an existing directory - how a test restarts one.
    pub async fn start_in(dir: TempDir) -> Self {
        Self::start_in_with(dir, Config::default()).await
    }

    pub async fn start_in_with(dir: TempDir, config: Config) -> Self {
        let key = hand_out(dir.path(), SPACE, Access::Write);
        let relay = Relay::open(dir.path(), config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(relay.serve(listener, async move {
            let _ = stopped.await;
        }));
        let client = client(Some(&key));
        Self { addr, client, key, stop: Some(stop), task: Some(task), dir }
    }

    /// A new key to `space`, handed out the way the operator does: to the
    /// relay's database, while it runs.
    pub fn key(&self, space: &str, access: Access) -> AccessKey {
        hand_out(self.dir.path(), space, access)
    }

    /// Stops the relay and hands back its directory.
    pub async fn stop(mut self) -> TempDir {
        self.shutdown().await;
        let dir = tempfile::tempdir().unwrap();
        std::mem::replace(&mut self.dir, dir)
    }

    pub async fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.unwrap().unwrap();
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// The relay as a device with `key` - or with none - speaks to it.
    pub fn with(&self, key: Option<&AccessKey>) -> Speaker {
        Speaker { base: format!("http://{}", self.addr), client: client(key) }
    }

    fn speaker(&self) -> Speaker {
        Speaker { base: format!("http://{}", self.addr), client: self.client.clone() }
    }

    pub async fn write(&self, stream: &str, epoch: u32, entries: &[&[u8]]) -> Result<Written, Refused> {
        self.speaker().write(stream, epoch, entries).await
    }

    pub async fn write_to(
        &self,
        stream: &str,
        expect: StreamId,
        epoch: u32,
        entries: &[&[u8]],
    ) -> Result<Written, Refused> {
        self.speaker().write_to(stream, expect, epoch, entries).await
    }

    pub async fn write_raw(&self, stream: &str, body: Vec<u8>) -> Result<Written, Refused> {
        self.speaker().write_raw(stream, body).await
    }

    pub async fn read(&self, stream: &str, after: Option<&Cursor>, limit: Option<usize>) -> Result<Page, Refused> {
        self.speaker().read(stream, after, limit).await
    }

    pub async fn read_all(&self, stream: &str, after: Option<Cursor>, page: usize) -> Vec<efema_proto::wire::Entry> {
        self.speaker().read_all(stream, after, page).await
    }

    pub async fn wait(&self, watches: &[Watch], timeout: u64) -> Result<Heads, Refused> {
        self.speaker().wait(watches, timeout).await
    }
}

/// One device's way of speaking to a relay: its address and an HTTP client
/// that shows the device's key.
#[derive(Clone)]
pub struct Speaker {
    base: String,
    pub client: reqwest::Client,
}

impl Speaker {
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn write(&self, stream: &str, epoch: u32, entries: &[&[u8]]) -> Result<Written, Refused> {
        let batch = Batch { epoch: Epoch(epoch), entries: entries.iter().map(|e| e.to_vec()).collect(), stream: None };
        self.write_raw(stream, wire::encode(&batch)).await
    }

    /// Writes only into the incarnation `expect`.
    pub async fn write_to(
        &self,
        stream: &str,
        expect: StreamId,
        epoch: u32,
        entries: &[&[u8]],
    ) -> Result<Written, Refused> {
        let batch =
            Batch { epoch: Epoch(epoch), entries: entries.iter().map(|e| e.to_vec()).collect(), stream: Some(expect) };
        self.write_raw(stream, wire::encode(&batch)).await
    }

    pub async fn write_raw(&self, stream: &str, body: Vec<u8>) -> Result<Written, Refused> {
        let response = self
            .client
            .post(self.url(&format!("/v1/streams/{stream}/entries")))
            .header("content-type", MEDIA_TYPE)
            .body(body)
            .send()
            .await
            .unwrap();
        answer(response).await
    }

    pub async fn read(&self, stream: &str, after: Option<&Cursor>, limit: Option<usize>) -> Result<Page, Refused> {
        let mut query = Vec::new();
        if let Some(cursor) = after {
            query.push(format!("after={cursor}"));
        }
        if let Some(limit) = limit {
            query.push(format!("limit={limit}"));
        }
        let response = self
            .client
            .get(self.url(&format!("/v1/streams/{stream}/entries?{}", query.join("&"))))
            .send()
            .await
            .unwrap();
        answer(response).await
    }

    /// Reads a stream to its end, page by page, from `after`.
    pub async fn read_all(
        &self,
        stream: &str,
        mut after: Option<Cursor>,
        page: usize,
    ) -> Vec<efema_proto::wire::Entry> {
        let mut entries = Vec::new();
        loop {
            let got = self.read(stream, after.as_ref(), Some(page)).await.unwrap();
            if let Some(cursor) = got.cursor() {
                after = Some(cursor);
            }
            let last = got.entries.last().map(|e| e.seq);
            entries.extend(got.entries);
            if last.is_none() || last == Some(got.head) {
                return entries;
            }
        }
    }

    pub async fn wait(&self, watches: &[Watch], timeout: u64) -> Result<Heads, Refused> {
        let mut query: Vec<String> = watches.iter().map(|w| format!("watch={w}")).collect();
        query.push(format!("timeout={timeout}"));
        let response = self.client.get(self.url(&format!("/v1/wait?{}", query.join("&")))).send().await.unwrap();
        answer(response).await
    }

    pub async fn limits(&self) -> Result<wire::Limits, Refused> {
        let response = self.client.get(self.url("/v1/limits")).send().await.unwrap();
        answer(response).await
    }
}

fn hand_out(dir: &std::path::Path, space: &str, access: Access) -> AccessKey {
    Store::open(dir).unwrap().add_key(&space.parse().unwrap(), access, "").unwrap()
}

/// An HTTP client that shows `key` with every request, or none.
pub fn client(key: Option<&AccessKey>) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    if let Some(key) = key {
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", key.expose())).unwrap());
    }
    reqwest::Client::builder().timeout(Duration::from_secs(90)).default_headers(headers).build().unwrap()
}

pub async fn answer<T: for<'b> minicbor::Decode<'b, ()>>(response: reqwest::Response) -> Result<T, Refused> {
    let status = response.status();
    assert_eq!(
        response.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some(MEDIA_TYPE),
        "every answer is CBOR, refusals included"
    );
    assert_eq!(
        response.headers().get("cache-control").and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "no answer may be cached"
    );
    let body = response.bytes().await.unwrap();
    if status.is_success() {
        Ok(wire::decode(&body).unwrap())
    } else {
        Err(Refused { status, problem: wire::decode(&body).unwrap() })
    }
}
