//! The relay's HTTP surface: protocol version 1.
//!
//! | Method | Path | What |
//! | --- | --- | --- |
//! | `POST` | `/v1/streams/{name}/entries` | append a batch |
//! | `GET` | `/v1/streams/{name}/entries?after=<cursor>&limit=<n>` | read after a cursor |
//! | `GET` | `/v1/wait?watch=<name>[:<cursor>]&timeout=<seconds>` | sleep until a stream moves |
//! | `GET` | `/health` | is the relay up |
//!
//! Bodies are CBOR (`efema_proto::wire`), errors included. Nothing here reads
//! entry data: a batch is decoded into a list of byte strings, and those bytes
//! go to the store as they came.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use efema_proto::wire::{self, Batch, Head, Heads, Problem, ProblemCode, Watch};
use efema_proto::{Cursor, MEDIA_TYPE, StreamName};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio::sync::watch;
use tracing::{debug, error, info};

use crate::Shared;
use crate::limits;
use crate::store::{AppendError, PageLimit, ReadError, StreamState};

pub(crate) fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/streams/{name}/entries", get(read).post(write))
        .route("/v1/wait", get(wait))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(limits::MAX_BODY_BYTES))
        .layer(middleware::map_response(no_store))
        .with_state(shared)
}

async fn health() -> &'static str {
    "ok\n"
}

async fn write(
    State(shared): State<Arc<Shared>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let name = stream_name(&name)?;
    require_cbor(&headers)?;
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Failure::too_large()
        } else {
            Failure::bad_request(format!("the body could not be read: {rejection}"))
        }
    })?;
    let batch: Batch =
        wire::decode(&body).map_err(|e| Failure::bad_request(format!("the body is not a batch: {e}")))?;
    if batch.entries.is_empty() {
        return Err(Failure::bad_request("a batch carries at least one entry".into()));
    }

    let size: usize = batch.entries.iter().map(Vec::len).sum();
    let count = batch.entries.len();
    let epoch = batch.epoch;
    let written = blocking({
        let shared = shared.clone();
        let name = name.clone();
        move || {
            shared.store.append(&name, batch.epoch, &batch.entries, |id, head| shared.heads.publish(&name, id, head))
        }
    })
    .await?
    .map_err(|e| match e {
        AppendError::EpochBehind { current, .. } => Failure {
            status: StatusCode::CONFLICT,
            problem: problem(ProblemCode::EpochBehind, e.to_string()).with_epoch(current),
        },
        AppendError::Sqlite(e) => Failure::internal(&e),
    })?;

    let at = if written.first == written.last {
        written.first.to_string()
    } else {
        format!("{}..{}", written.first, written.last)
    };
    info!("{name}: wrote {count} {} ({size} bytes) at {at}, epoch {epoch}", plural(count, "entry", "entries"));
    Ok(cbor(StatusCode::OK, wire::encode(&written)))
}

async fn read(
    State(shared): State<Arc<Shared>>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, Failure> {
    let name = stream_name(&name)?;
    let mut after = None;
    let mut entries = limits::PAGE_ENTRIES_DEFAULT;
    for (key, value) in parameters(query.as_deref()) {
        match key.as_ref() {
            "after" => {
                after = Some(
                    value
                        .parse::<Cursor>()
                        .map_err(|e| Failure::bad_request(format!("`after` is not a cursor: {e}")))?,
                )
            }
            "limit" => {
                entries =
                    value.parse::<usize>().ok().filter(|n| (1..=limits::PAGE_ENTRIES_MAX).contains(n)).ok_or_else(
                        || {
                            Failure::bad_request(format!(
                                "`limit` is a number of entries from 1 to {}",
                                limits::PAGE_ENTRIES_MAX
                            ))
                        },
                    )?
            }
            other => return Err(Failure::bad_request(format!("unknown query parameter `{other}`"))),
        }
    }

    let limit = PageLimit { entries, bytes: limits::PAGE_BYTES };
    let page = blocking({
        let shared = shared.clone();
        let name = name.clone();
        move || shared.store.read(&name, after.as_ref(), limit)
    })
    .await?
    .map_err(read_failure)?;

    info!(
        "{name}: read {} {} after {}",
        page.entries.len(),
        plural(page.entries.len(), "entry", "entries"),
        after.map_or(0, |cursor| cursor.seq)
    );
    Ok(cbor(StatusCode::OK, wire::encode(&page)))
}

fn read_failure(e: ReadError) -> Failure {
    let message = e.to_string();
    match e {
        ReadError::NotFound(_) => {
            Failure { status: StatusCode::NOT_FOUND, problem: problem(ProblemCode::StreamNotFound, message) }
        }
        ReadError::Replaced { .. } => {
            Failure { status: StatusCode::CONFLICT, problem: problem(ProblemCode::StreamReplaced, message) }
        }
        ReadError::Ahead { head, .. } => Failure {
            status: StatusCode::CONFLICT,
            problem: problem(ProblemCode::CursorAhead, message).with_head(head),
        },
        ReadError::Diverged { .. } => {
            Failure { status: StatusCode::CONFLICT, problem: problem(ProblemCode::CursorDiverged, message) }
        }
        // A gap is the relay contradicting its own schema; the reader is told
        // something failed, and the log gets the details.
        ReadError::Gap { .. } => Failure::internal(&e),
        ReadError::Sqlite(e) => Failure::internal(&e),
    }
}

async fn wait(State(shared): State<Arc<Shared>>, RawQuery(query): RawQuery) -> Result<Response, Failure> {
    let mut watches: Vec<Watch> = Vec::new();
    let mut timeout = limits::WAIT_DEFAULT;
    for (key, value) in parameters(query.as_deref()) {
        match key.as_ref() {
            "watch" => {
                let watch: Watch = value
                    .parse()
                    .map_err(|e| Failure::bad_request(format!("`watch` is not a stream to watch: {e}")))?;
                if watches.iter().any(|w| w.stream == watch.stream) {
                    return Err(Failure::bad_request(format!("stream {} is watched twice", watch.stream)));
                }
                watches.push(watch);
            }
            "timeout" => {
                timeout =
                    value.parse::<u64>().ok().map(Duration::from_secs).filter(|t| *t <= limits::WAIT_MAX).ok_or_else(
                        || {
                            Failure::bad_request(format!(
                                "`timeout` is a number of seconds from 0 to {}",
                                limits::WAIT_MAX.as_secs()
                            ))
                        },
                    )?
            }
            other => return Err(Failure::bad_request(format!("unknown query parameter `{other}`"))),
        }
    }
    if watches.is_empty() {
        return Err(Failure::bad_request("a wait names at least one stream to watch".into()));
    }
    if watches.len() > limits::WATCHES_MAX {
        return Err(Failure::bad_request(format!("a wait watches at most {} streams", limits::WATCHES_MAX)));
    }

    let receivers = blocking({
        let shared = shared.clone();
        let streams: Vec<StreamName> = watches.iter().map(|w| w.stream.clone()).collect();
        move || {
            streams
                .iter()
                .map(|name| shared.heads.subscribe(name, || shared.store.state(name)))
                .collect::<Result<Vec<_>, _>>()
        }
    })
    .await?
    .map_err(|e| Failure::internal(&e))?;

    let report = |receivers: &[watch::Receiver<StreamState>]| -> Vec<Head> {
        watches
            .iter()
            .zip(receivers)
            .filter_map(|(watch, rx)| {
                let state = *rx.borrow();
                moved(watch, state).then(|| Head { stream: watch.stream.clone(), id: state.id, head: state.head })
            })
            .collect()
    };

    let mut heads = report(&receivers);
    if heads.is_empty() && !timeout.is_zero() {
        let mut shutdown = shared.shutdown.clone();
        let mut changes: FuturesUnordered<_> = watches
            .iter()
            .zip(receivers.iter().cloned())
            .map(|(watch, mut rx)| async move {
                // An error means the sender is gone, which only happens when
                // the relay itself is going away: wake and report what is known.
                let _ = rx.wait_for(|state| moved(watch, *state)).await;
            })
            .collect();
        tokio::select! {
            _ = changes.next() => {}
            _ = tokio::time::sleep(timeout) => {}
            _ = shutdown.wait_for(|stopping| *stopping) => {}
        }
        heads = report(&receivers);
    }

    debug!("wait on {} {}: {} moved", watches.len(), plural(watches.len(), "stream", "streams"), heads.len());
    Ok(cbor(StatusCode::OK, wire::encode(&Heads { heads })))
}

/// Whether a stream is no longer where the waiter left it.
fn moved(watch: &Watch, state: StreamState) -> bool {
    match &watch.cursor {
        // A reader with no cursor is waiting for the stream to have anything.
        None => state.head > 0,
        // A reader with a cursor wakes for new entries - and also for a stream
        // that is not the one its cursor belongs to, or one that lost entries:
        // reading then tells it which, instead of it sleeping on forever.
        Some(cursor) => state.id != Some(cursor.stream) || state.head != cursor.seq,
    }
}

async fn not_found() -> Failure {
    Failure { status: StatusCode::NOT_FOUND, problem: problem(ProblemCode::NotFound, "no such endpoint".into()) }
}

async fn method_not_allowed() -> Failure {
    Failure {
        status: StatusCode::METHOD_NOT_ALLOWED,
        problem: problem(ProblemCode::MethodNotAllowed, "this endpoint does not take that method".into()),
    }
}

/// Nothing the relay answers may be served from a cache: a stale page is a
/// reader told the stream ended where it did not.
async fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn stream_name(text: &str) -> Result<StreamName, Failure> {
    text.parse().map_err(|e: efema_proto::InvalidName| Failure::bad_request(e.to_string()))
}

fn require_cbor(headers: &HeaderMap) -> Result<(), Failure> {
    let essence = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().trim().to_ascii_lowercase());
    if essence.as_deref() == Some(MEDIA_TYPE) {
        Ok(())
    } else {
        Err(Failure {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            problem: problem(ProblemCode::UnsupportedMediaType, format!("a batch is sent as {MEDIA_TYPE}")),
        })
    }
}

/// The query string's pairs, percent-decoded. A client library may encode
/// `:` as `%3A`; both spellings mean the same parameter.
fn parameters(query: Option<&str>) -> impl Iterator<Item = (std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>)> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
}

/// Runs store work on the blocking pool: SQLite calls block, and an async
/// worker thread stuck in one stalls every other request it was serving.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, Failure> {
    tokio::task::spawn_blocking(work).await.map_err(|e| Failure::internal(&e))
}

fn cbor(status: StatusCode, body: Vec<u8>) -> Response {
    (status, [(header::CONTENT_TYPE, HeaderValue::from_static(MEDIA_TYPE))], body).into_response()
}

fn problem(code: ProblemCode, message: String) -> Problem {
    Problem { code, message, head: None, epoch: None, limit: None }
}

trait ProblemDetails {
    fn with_head(self, head: u64) -> Self;
    fn with_epoch(self, epoch: efema_proto::Epoch) -> Self;
}

impl ProblemDetails for Problem {
    fn with_head(mut self, head: u64) -> Self {
        self.head = Some(head);
        self
    }

    fn with_epoch(mut self, epoch: efema_proto::Epoch) -> Self {
        self.epoch = Some(epoch);
        self
    }
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

/// A refusal: a status and the problem that explains it.
pub(crate) struct Failure {
    status: StatusCode,
    problem: Problem,
}

impl Failure {
    fn bad_request(message: String) -> Self {
        Self { status: StatusCode::BAD_REQUEST, problem: problem(ProblemCode::BadRequest, message) }
    }

    fn too_large() -> Self {
        let mut problem =
            problem(ProblemCode::TooLarge, format!("a request body is at most {} bytes", limits::MAX_BODY_BYTES));
        problem.limit = Some(limits::MAX_BODY_BYTES as u64);
        Self { status: StatusCode::PAYLOAD_TOO_LARGE, problem }
    }

    /// The client hears that something failed; the operator's log hears what.
    fn internal(cause: &dyn std::fmt::Display) -> Self {
        error!("internal error: {cause}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            problem: problem(ProblemCode::Internal, "the relay failed to handle the request".into()),
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        debug!("{} {}: {}", self.status.as_u16(), self.problem.code, self.problem.message);
        cbor(self.status, wire::encode(&self.problem))
    }
}
