//! The relay's HTTP surface: protocol version 1.
//!
//! | Method | Path | What |
//! | --- | --- | --- |
//! | `POST` | `/v1/streams/{name}/entries` | append a batch |
//! | `GET` | `/v1/streams/{name}/entries?after=<cursor>&limit=<n>` | read after a cursor |
//! | `GET` | `/v1/wait?watch=<name>[:<cursor>]&timeout=<seconds>` | sleep until a stream moves |
//! | `GET` | `/v1/limits` | what the key may do, and how much |
//! | `GET` | `/health` | is the relay up |
//! | `GET` | `/metrics` | counts and times, on the relay's own machine only |
//!
//! Every request passes the door first ([`crate::door`]): plain HTTP from
//! outside is refused before anything else is read. Everything under `/v1`
//! then needs an access key, which opens one space; names in the path and
//! the query are streams of that space.
//!
//! Bodies are CBOR (`efema_proto::wire`), errors included. Nothing here reads
//! entry data: a batch is decoded into a list of byte strings, and those bytes
//! go to the store as they came. A batch arrives whole or not at all - the
//! body is read to its end before it is decoded - so a connection torn in
//! the middle of one writes nothing.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, RawQuery, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use efema_proto::wire::{self, Batch, Head, Heads, Limits, Problem, ProblemCode, Watch};
use efema_proto::{AccessKey, Cursor, MEDIA_TYPE, StreamName};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio::sync::watch;
use tracing::{debug, error, info};

use crate::Shared;
use crate::door::{self, Origin};
use crate::limits;
use crate::store::{AppendError, Grant, PageLimit, ReadError, StreamState, WriteLimits, unix_now};

pub(crate) fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/streams/{name}/entries", get(read).post(write))
        .route("/v1/wait", get(wait))
        .route("/v1/limits", get(announce))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        // The protocol's ceiling, while the body is read; the operator's own
        // limit is the store's, after it knows whether the batch is a repeat.
        .layer(DefaultBodyLimit::max(limits::MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(shared.clone(), at_the_door))
        .layer(middleware::map_response(no_store))
        .with_state(shared)
}

/// Lets in what came from the relay's own machine or through the proxy over
/// HTTPS, and says which; refuses everything else before it is read.
async fn at_the_door(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    match door::origin(peer.ip(), request.headers(), &shared.config.trusted) {
        Ok(origin) => {
            request.extensions_mut().insert(origin);
            next.run(request).await
        }
        Err(reason) => {
            info!("refused {} {}: {reason}", request.method(), request.uri().path());
            Failure { status: StatusCode::FORBIDDEN, problem: problem(ProblemCode::HttpsRequired, reason) }
                .into_response()
        }
    }
}

async fn health() -> &'static str {
    "ok\n"
}

/// What a key shows the relay, once the relay has found it: the space it
/// opens and what it may do there. Every request under `/v1` takes one.
pub(crate) struct Authorized(Grant);

impl FromRequestParts<Arc<Shared>> for Authorized {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, shared: &Arc<Shared>) -> Result<Self, Failure> {
        let key = bearer(&parts.headers)?;
        let digest = key.digest();
        let grant = blocking({
            let shared = shared.clone();
            move || shared.store.grant(&digest)
        })
        .await?
        .map_err(|e| Failure::internal(&e))?
        .ok_or_else(|| {
            Failure::unauthorized("the relay does not know this access key: it was revoked, or never handed out")
        })?;
        shared.rates.take(&digest).map_err(Failure::rate_limited)?;
        seen(shared, &digest).await;
        Ok(Self(grant))
    }
}

/// The access key of an `Authorization: Bearer` header.
fn bearer(headers: &HeaderMap) -> Result<AccessKey, Failure> {
    let value = headers.get(header::AUTHORIZATION).ok_or_else(|| {
        Failure::unauthorized("a request to the relay carries an access key: `Authorization: Bearer <key>`")
    })?;
    let text = value.to_str().unwrap_or_default();
    let (scheme, key) = text.split_once(' ').unwrap_or((text, ""));
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(Failure::unauthorized("the access key goes in the `Bearer` scheme: `Authorization: Bearer <key>`"));
    }
    key.trim().parse().map_err(|e: efema_proto::ParseAccessKeyError| Failure::unauthorized(&e.to_string()))
}

/// Writes down that a key was used - at most once a minute per key, so a
/// device that syncs does not turn every read into a write on the disk.
async fn seen(shared: &Arc<Shared>, digest: &[u8; 32]) {
    let now = unix_now();
    {
        let mut seen = shared.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if seen.get(digest).is_some_and(|at| now - at < 60) {
            return;
        }
        seen.insert(*digest, now);
    }
    let shared = shared.clone();
    let digest = *digest;
    // A failure costs the listing a time, not the request its answer.
    if let Ok(Err(e)) = blocking(move || shared.store.touch(&digest, now)).await {
        error!("cannot write down when a key was used: {e}");
    }
}

async fn write(
    State(shared): State<Arc<Shared>>,
    Authorized(grant): Authorized,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let name = stream_name(&name)?;
    if !grant.access.writes() {
        return Err(Failure {
            status: StatusCode::FORBIDDEN,
            problem: problem(ProblemCode::Forbidden, "the access key reads this space and does not write to it".into()),
        });
    }
    require_cbor(&headers)?;
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Failure::too_large(
                format!("a request body is at most {} bytes", limits::MAX_BODY_BYTES),
                limits::MAX_BODY_BYTES as u64,
            )
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
    let limits = shared.config.limits;
    let admit = WriteLimits { body: limits.body as u64, entry: limits.entry as u64, quota: limits.quota };
    let address = (grant.space, name.clone());
    let written = blocking({
        let shared = shared.clone();
        let body = body.len() as u64;
        move || {
            shared
                .store
                .append(address.0, &address.1, &batch, body, admit, |id, head| shared.heads.publish(&address, id, head))
        }
    })
    .await?
    .map_err(|e| match e {
        AppendError::EpochBehind { current, .. } => Failure {
            status: StatusCode::CONFLICT,
            problem: problem(ProblemCode::EpochBehind, e.to_string()).with_epoch(current),
        },
        AppendError::NotFound { .. } => {
            Failure { status: StatusCode::NOT_FOUND, problem: problem(ProblemCode::StreamNotFound, e.to_string()) }
        }
        AppendError::Replaced { .. } => {
            Failure { status: StatusCode::CONFLICT, problem: problem(ProblemCode::StreamReplaced, e.to_string()) }
        }
        AppendError::BodyTooLarge { limit, .. } | AppendError::EntryTooLarge { limit, .. } => {
            Failure::too_large(e.to_string(), limit)
        }
        AppendError::QuotaExceeded { quota, .. } => {
            let mut problem = problem(ProblemCode::QuotaExceeded, e.to_string());
            problem.limit = Some(quota);
            Failure { status: StatusCode::INSUFFICIENT_STORAGE, problem }
        }
        AppendError::OutOfPlace { head, .. } => Failure {
            status: StatusCode::CONFLICT,
            problem: problem(ProblemCode::OutOfPlace, e.to_string()).with_head(head),
        },
        AppendError::Sqlite(e) => Failure::internal(&e),
    })?;

    let space = &grant.space_name;
    let at = if written.first == written.last {
        written.first.to_string()
    } else {
        format!("{}..{}", written.first, written.last)
    };
    if written.repeated {
        // The writer did not hear the first answer; nothing is written now.
        info!(
            "{space}/{name}: {count} {} ({size} bytes) sent again, written before at {at}",
            plural(count, "entry", "entries")
        );
    } else {
        info!(
            "{space}/{name}: wrote {count} {} ({size} bytes) at {at}, epoch {epoch}",
            plural(count, "entry", "entries")
        );
    }
    Ok(cbor(StatusCode::OK, wire::encode(&written)))
}

async fn read(
    State(shared): State<Arc<Shared>>,
    Authorized(grant): Authorized,
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
        move || shared.store.read(grant.space, &name, after.as_ref(), limit)
    })
    .await?
    .map_err(read_failure)?;

    info!(
        "{}/{name}: read {} {} after {}",
        grant.space_name,
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

async fn wait(
    State(shared): State<Arc<Shared>>,
    Authorized(grant): Authorized,
    RawQuery(query): RawQuery,
) -> Result<Response, Failure> {
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
                .into_iter()
                .map(|name| {
                    let address = (grant.space, name);
                    shared.heads.subscribe(&address, || shared.store.state(address.0, &address.1))
                })
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

/// What the key may do, and how much: the operator's limits, and what its
/// space holds now.
async fn announce(State(shared): State<Arc<Shared>>, Authorized(grant): Authorized) -> Result<Response, Failure> {
    let used = blocking({
        let shared = shared.clone();
        move || shared.store.used(grant.space)
    })
    .await?
    .map_err(|e| Failure::internal(&e))?;
    let set = shared.config.limits;
    let limits = Limits {
        write: grant.access.writes(),
        body: set.body as u64,
        entry: set.entry as u64,
        quota: set.quota,
        used,
        rate: u64::from(set.rate),
        page_entries: limits::PAGE_ENTRIES_DEFAULT as u64,
        page_entries_max: limits::PAGE_ENTRIES_MAX as u64,
        page_bytes: limits::PAGE_BYTES as u64,
        wait_max: limits::WAIT_MAX.as_secs(),
        watches: limits::WATCHES_MAX as u64,
    };
    Ok(cbor(StatusCode::OK, wire::encode(&limits)))
}

/// Counts and times, in the text format Prometheus reads: spaces, streams,
/// entries, bytes, keys, the last write and the last use of a key. Never a
/// name of a stream, never anything an entry holds - and only on the relay's
/// own machine: through the door, it is the operator's business, not that
/// of whoever reached the door.
async fn metrics(State(shared): State<Arc<Shared>>, Extension(origin): Extension<Origin>) -> Result<Response, Failure> {
    if origin != Origin::Local {
        return Err(Failure {
            status: StatusCode::FORBIDDEN,
            problem: problem(ProblemCode::Forbidden, "the metrics are served on the relay's own machine only".into()),
        });
    }
    let spaces = blocking({
        let shared = shared.clone();
        move || shared.store.spaces()
    })
    .await?
    .map_err(|e| Failure::internal(&e))?;

    let mut text = String::new();
    let mut family = |name: &str, help: &str, values: &mut dyn Iterator<Item = (Option<&str>, i128)>| {
        let _ = writeln!(text, "# HELP {name} {help}");
        let _ = writeln!(text, "# TYPE {name} gauge");
        for (space, value) in values {
            match space {
                Some(space) => {
                    let _ = writeln!(text, "{name}{{space=\"{space}\"}} {value}");
                }
                None => {
                    let _ = writeln!(text, "{name} {value}");
                }
            }
        }
    };
    family(
        "efema_start_time_seconds",
        "When the relay started, in seconds since the Unix epoch.",
        &mut std::iter::once((None, i128::from(shared.started))),
    );
    family("efema_spaces", "Spaces on the relay.", &mut std::iter::once((None, spaces.len() as i128)));
    family(
        "efema_quota_bytes",
        "Bytes of entry data one space may hold.",
        &mut std::iter::once((None, i128::from(shared.config.limits.quota))),
    );
    family(
        "efema_streams",
        "Streams in a space.",
        &mut spaces.iter().map(|s| (Some(s.name.as_str()), i128::from(s.streams))),
    );
    family(
        "efema_entries",
        "Entries in a space, all its streams together.",
        &mut spaces.iter().map(|s| (Some(s.name.as_str()), i128::from(s.entries))),
    );
    family(
        "efema_bytes",
        "Bytes of entry data a space holds.",
        &mut spaces.iter().map(|s| (Some(s.name.as_str()), i128::from(s.bytes))),
    );
    family(
        "efema_keys",
        "Access keys to a space.",
        &mut spaces.iter().map(|s| (Some(s.name.as_str()), i128::from(s.keys))),
    );
    family(
        "efema_last_write_seconds",
        "When a stream of the space was last written, in seconds since the Unix epoch.",
        &mut spaces.iter().filter_map(|s| s.written_at.map(|at| (Some(s.name.as_str()), i128::from(at)))),
    );
    family(
        "efema_last_use_seconds",
        "When a key to the space was last used, to the minute, in seconds since the Unix epoch.",
        &mut spaces.iter().filter_map(|s| s.used_at.map(|at| (Some(s.name.as_str()), i128::from(at)))),
    );
    let _ = writeln!(text, "# HELP efema_info The relay's release.");
    let _ = writeln!(text, "# TYPE efema_info gauge");
    let _ = writeln!(text, "efema_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"))],
        text,
    )
        .into_response())
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
    Problem { code, message, head: None, epoch: None, limit: None, retry: None }
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

    fn unauthorized(message: &str) -> Self {
        Self { status: StatusCode::UNAUTHORIZED, problem: problem(ProblemCode::Unauthorized, message.to_string()) }
    }

    fn too_large(message: String, limit: u64) -> Self {
        let mut problem = problem(ProblemCode::TooLarge, message);
        problem.limit = Some(limit);
        Self { status: StatusCode::PAYLOAD_TOO_LARGE, problem }
    }

    fn rate_limited(wait: Duration) -> Self {
        // Whole seconds, rounded up: an answer to come back in 0 seconds
        // invites the request that is refused again.
        let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        let mut problem = problem(
            ProblemCode::RateLimited,
            format!("too many requests with this access key: try again in {seconds} s"),
        );
        problem.retry = Some(seconds.max(1));
        Self { status: StatusCode::TOO_MANY_REQUESTS, problem }
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
        let mut response = cbor(self.status, wire::encode(&self.problem));
        // What HTTP itself says about these two, next to what the body says.
        if self.problem.code == ProblemCode::Unauthorized {
            response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"efema\""));
        }
        if let Some(seconds) = self.problem.retry {
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}
