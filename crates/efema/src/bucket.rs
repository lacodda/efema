//! The transport to an S3 bucket: sync with no server of one's own.
//!
//! A bucket is a relay with nobody running it. Every device writes there and
//! reads from there, and what keeps a stream in one order for all of them is
//! the one thing S3 does atomically: creating an object only if there is
//! none under its key. The layout that rests on it is in [`layout`].

mod layout;
mod sign;
mod xml;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use efema_proto::wire::{self, Batch, Head, Heads, Limits, Page, Problem, ProblemCode, Watch, Written};
use efema_proto::{Cursor, Epoch, Hash, StreamId, StreamName, chain, limits};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use tokio::sync::OnceCell;
use tokio::time::Instant;

use crate::relay::{CONNECT_TIMEOUT, InvalidUrl, address};
use crate::stall::{Pulse, STALL, Upload};
use crate::transport::{Transport, TransportError};
use layout::{Record, Stored, Summary};
pub use sign::Credentials;

/// How many places one batch may try before the append gives up: every
/// place it loses is a batch another writer put there, so this many is a
/// stream far busier than devices syncing make it.
const PLACES: usize = 64;

/// How many times a creation is tried again when S3 says another one of the
/// same object is under way (`409 ConditionalRequestConflict`).
const CONFLICTS: u32 = 8;

/// How many batches a bucket remembers between requests. A batch never
/// changes, so what was read of it once holds for good; the memory is only
/// bounded.
const REMEMBERED: usize = 4096;

/// What a bucket's answers say a key may do, and how much: S3 counts
/// nothing a client could read and limits nothing a client could hit, so
/// the space has no quota (`u64::MAX`) and holds what it holds (`used` is
/// 0 - not counted). The rest are the protocol's defaults.
const LIMITS: Limits = Limits { write: true, quota: u64::MAX, used: 0, rate: u64::MAX, ..Limits::DEFAULT };

/// A bucket of S3 - Amazon's, or any service that speaks its protocol
/// (Cloudflare R2, MinIO, and others) - with the credentials its owner
/// handed out. Streams live in it as objects, sealed like on a relay: the
/// bucket's owner sees names, sizes and times, and none of what was written.
///
/// The bucket must refuse to create an object that is already there
/// (`If-None-Match: *`) and show a write to every read after it. Amazon S3
/// and MinIO do; before its first write a `Bucket` checks the first by
/// creating an object twice, and refuses to write to a bucket that lets the
/// second through - a stream there would fork without a word.
///
/// Nothing in a bucket wakes a device that waits: a wait looks again every
/// [`Bucket::POLL`].
///
/// `url` is the bucket's own address, in whichever of the two forms the
/// service gives: `https://<bucket>.s3.<region>.amazonaws.com`, or the
/// service's address with the bucket as the path,
/// `https://<account>.r2.cloudflarestorage.com/<bucket>`. `region` is the
/// bucket's region: R2 takes `auto`, MinIO `us-east-1` unless set
/// otherwise.
///
/// ```
/// use efema::{Bucket, Credentials};
///
/// let credentials = Credentials::new("AKIDEXAMPLE", "not a real secret");
/// let bucket = Bucket::new("https://notes.s3.eu-central-1.amazonaws.com/", "eu-central-1", credentials.clone())?;
/// assert_eq!(bucket.to_string(), "https://notes.s3.eu-central-1.amazonaws.com");
/// assert!(Bucket::new("http://192.0.2.1:9000/notes", "us-east-1", credentials.clone()).is_err());
/// assert!(Bucket::new("http://127.0.0.1:9000/notes", "us-east-1", credentials).is_ok());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct Bucket(Arc<Inner>);

struct Inner {
    /// `scheme://host[:port]`.
    origin: String,
    /// The bucket's path on the origin: empty for a bucket of its own host.
    path: String,
    /// The host as a request names it, port and all.
    host: String,
    region: String,
    credentials: Credentials,
    http: reqwest::Client,
    /// Set once the bucket was seen to refuse a second creation, and the
    /// credentials to write.
    probed: OnceCell<()>,
    known: Mutex<Known>,
}

/// What a bucket remembers of the batches it read: their summaries, and the
/// last one read whole - a page may end inside it, and the next one goes
/// on from there.
#[derive(Default)]
struct Known {
    summaries: BTreeMap<(StreamId, u64), Summary>,
    body: Option<(StreamId, Arc<Stored>)>,
}

/// An answer, read whole.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// How a creation went.
enum Created {
    Made,
    /// Something was there already.
    Taken,
}

/// A watched stream as a wait last looked at it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Seen {
    id: Option<StreamId>,
    head: u64,
}

impl Bucket {
    /// How often a wait looks at the bucket again.
    pub const POLL: Duration = Duration::from_secs(5);

    /// The bucket at `url`, in `region`, reached with `credentials`:
    /// `https://`, or `http://` to this machine.
    ///
    /// # Errors
    ///
    /// [`InvalidUrl`] when `url` is not an HTTP address, is plain HTTP to
    /// another machine, or `region` is not a region's name.
    pub fn new(url: &str, region: &str, credentials: Credentials) -> Result<Self, InvalidUrl> {
        const WHAT: &str = "a bucket address";
        let parsed = address(url, WHAT)?;
        if region.is_empty() || !region.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(InvalidUrl::new(url, WHAT, &format!("`{region}` is not a region: letters, digits and '-'")));
        }
        let host = match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            (None, _) => unreachable!("an address has a host"),
        };
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("efema/", env!("CARGO_PKG_VERSION")))
            // A signature names its host: an answer that sends the request
            // elsewhere is a refusal to read, not an address to follow.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| InvalidUrl::new(url, WHAT, &e.to_string()))?;
        Ok(Self(Arc::new(Inner {
            origin: format!("{}://{host}", parsed.scheme()),
            path: parsed.path().trim_end_matches('/').to_string(),
            host,
            region: region.to_string(),
            credentials,
            http,
            probed: OnceCell::new(),
            known: Mutex::new(Known::default()),
        })))
    }

    fn base(&self) -> String {
        format!("{}{}", self.0.origin, self.0.path)
    }

    /// Sends a signed request - to the object `key`, or to the bucket - and
    /// reads the answer whole, giving up once nothing has moved for
    /// [`STALL`].
    async fn call(
        &self,
        method: Method,
        key: Option<&str>,
        query: &[(&str, &str)],
        mut headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> Result<Answer, TransportError> {
        let path = match key {
            Some(key) => format!("{}/{}", self.0.path, sign::encode(key, true)),
            None if self.0.path.is_empty() => "/".to_string(),
            None => self.0.path.clone(),
        };
        let query = sign::query(query);
        let payload = body.as_deref().map_or_else(|| sign::EMPTY_PAYLOAD.to_string(), sign::payload_hash);
        let stamp = sign::Stamp::at(SystemTime::now());
        headers.push(("x-amz-date".to_string(), stamp.time.clone()));
        headers.push(("x-amz-content-sha256".to_string(), payload.clone()));
        headers.push(("host".to_string(), self.0.host.clone()));
        let signed = sign::authorization(
            &self.0.credentials,
            &self.0.region,
            &stamp,
            &sign::Request {
                method: method.as_str(),
                path: &path,
                query: &query,
                headers: &headers,
                payload: &payload,
            },
        );
        let url = if query.is_empty() {
            format!("{}{path}", self.0.origin)
        } else {
            format!("{}{path}?{query}", self.0.origin)
        };

        let mut request = self.0.http.request(method, url);
        for (name, value) in headers.iter().filter(|(name, _)| name != "host") {
            request = request.header(name, value);
        }
        let mut authorization = HeaderValue::from_str(&signed).expect("a signature is printable ASCII");
        authorization.set_sensitive(true);
        request = request.header(AUTHORIZATION, authorization);
        let (request, pulse) = match body {
            Some(body) => {
                let pulse = Pulse::new(STALL);
                let upload = Upload::new(body, pulse.clone(), STALL);
                (request.body(reqwest::Body::wrap(upload)), pulse)
            }
            None => (request, Pulse::new(STALL)),
        };
        let talk = async {
            let mut response = request.send().await.map_err(|e| self.unreachable(e))?;
            pulse.beat(STALL);
            let status = response.status();
            let headers = response.headers().clone();
            let mut body = Vec::new();
            while let Some(piece) = response.chunk().await.map_err(|e| self.unreachable(e))? {
                pulse.beat(STALL);
                body.extend_from_slice(&piece);
            }
            Ok(Answer { status, headers, body })
        };
        tokio::select! {
            answer = talk => answer,
            quiet = pulse.silence() => Err(TransportError::Stalled { at: self.base(), quiet }),
        }
    }

    fn unreachable(&self, source: reqwest::Error) -> TransportError {
        TransportError::Unreachable { at: self.base(), source: Box::new(source.without_url()) }
    }

    fn not_protocol(&self, reason: String) -> TransportError {
        TransportError::NotProtocol { at: self.base(), reason }
    }

    fn refusal(&self, code: ProblemCode, message: String) -> TransportError {
        TransportError::Refused { at: self.base(), problem: Box::new(problem(code, message)) }
    }

    /// What an answer that is not a success says, in the protocol's terms.
    fn refused(&self, answer: &Answer) -> TransportError {
        let text = std::str::from_utf8(&answer.body).unwrap_or_default();
        let code = xml::first(text, "Code");
        let message = xml::first(text, "Message").unwrap_or_else(|| format!("{} from the bucket", answer.status));
        let status = answer.status;
        let ours = match code.as_deref() {
            Some("NoSuchBucket") => ProblemCode::NotFound,
            Some("InvalidAccessKeyId" | "SignatureDoesNotMatch" | "RequestTimeTooSkewed") => ProblemCode::Unauthorized,
            Some("AccessDenied" | "AllAccessDisabled") => ProblemCode::Forbidden,
            Some("SlowDown" | "TooManyRequests") => ProblemCode::RateLimited,
            Some(other) if !status.is_server_error() => ProblemCode::Other(other.to_string()),
            _ if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE => {
                ProblemCode::RateLimited
            }
            _ if status.is_server_error() => ProblemCode::Internal,
            _ if status == StatusCode::UNAUTHORIZED => ProblemCode::Unauthorized,
            _ if status == StatusCode::FORBIDDEN => ProblemCode::Forbidden,
            _ => return self.not_protocol(format!("a {status} answer that is not S3's")),
        };
        let mut problem = problem(ours, message);
        if problem.code == ProblemCode::RateLimited {
            problem.retry = Some(1);
        }
        TransportError::Refused { at: self.base(), problem: Box::new(problem) }
    }

    /// The object under `key` - its headers only, for `HEAD` - or `None`
    /// when there is none.
    async fn get(&self, method: Method, key: &str) -> Result<Option<Answer>, TransportError> {
        let head = method == Method::HEAD;
        let answer = self.call(method, Some(key), &[], Vec::new(), None).await?;
        if answer.status.is_success() {
            return Ok(Some(answer));
        }
        // An answer to HEAD has no body to say which thing is missing; a
        // missing bucket shows on the GETs every read and wait begins with.
        let missing = answer.status == StatusCode::NOT_FOUND
            && (head
                || xml::first(std::str::from_utf8(&answer.body).unwrap_or_default(), "Code").as_deref()
                    == Some("NoSuchKey"));
        if missing { Ok(None) } else { Err(self.refused(&answer)) }
    }

    /// Writes `body` under `key`, over whatever is there.
    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), TransportError> {
        let answer = self.call(Method::PUT, Some(key), &[], Vec::new(), Some(body)).await?;
        if answer.status.is_success() { Ok(()) } else { Err(self.refused(&answer)) }
    }

    /// Creates `key` with `body` if there is nothing under it yet.
    async fn create(
        &self,
        key: &str,
        body: Vec<u8>,
        metadata: Vec<(String, String)>,
    ) -> Result<Created, TransportError> {
        let mut headers = metadata;
        headers.push(("if-none-match".to_string(), "*".to_string()));
        for attempt in 1..=CONFLICTS {
            let answer = self.call(Method::PUT, Some(key), &[], headers.clone(), Some(body.clone())).await?;
            match answer.status {
                status if status.is_success() => return Ok(Created::Made),
                StatusCode::PRECONDITION_FAILED => return Ok(Created::Taken),
                // Another creation of the same key is under way: one of the
                // two will have it, and the next try learns which.
                StatusCode::CONFLICT => tokio::time::sleep(Duration::from_millis(50 * u64::from(attempt))).await,
                _ => return Err(self.refused(&answer)),
            }
        }
        Err(self
            .refusal(ProblemCode::RateLimited, format!("{key} was being created by someone else {CONFLICTS} times")))
    }

    /// The keys under `prefix` that come after `after`, in key order: at
    /// most `max`, or all of them.
    async fn list(&self, prefix: &str, after: Option<&str>, max: Option<usize>) -> Result<Vec<String>, TransportError> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let want = max.map_or(1000, |max| (max - keys.len()).min(1000)).to_string();
            let mut query = vec![("list-type", "2"), ("prefix", prefix), ("max-keys", want.as_str())];
            if let Some(after) = after {
                query.push(("start-after", after));
            }
            if let Some(token) = &token {
                query.push(("continuation-token", token.as_str()));
            }
            let answer = self.call(Method::GET, None, &query, Vec::new(), None).await?;
            if !answer.status.is_success() {
                return Err(self.refused(&answer));
            }
            let text = std::str::from_utf8(&answer.body)
                .map_err(|_| self.not_protocol("a listing that is not text".to_string()))?;
            let found =
                xml::all(text, "Key").ok_or_else(|| self.not_protocol("a listing that does not read".into()))?;
            if let Some(stray) = found.iter().find(|key| !key.starts_with(prefix)) {
                return Err(self.not_protocol(format!("a listing of {prefix} with {stray} in it")));
            }
            keys.extend(found);
            let more = xml::first(text, "IsTruncated").as_deref() == Some("true");
            if !more || max.is_some_and(|max| keys.len() >= max) {
                return Ok(keys);
            }
            token = Some(
                xml::first(text, "NextContinuationToken")
                    .ok_or_else(|| self.not_protocol("a listing that goes on and says not where".into()))?,
            );
        }
    }

    /// Whether the credentials write, checked by creating the probe object
    /// - twice, if it was not there: the second creation must be refused.
    async fn probe(&self) -> Result<bool, TransportError> {
        let create = || self.create(layout::PROBE, layout::PROBE_TEXT.as_bytes().to_vec(), Vec::new());
        let first = match create().await {
            Ok(created) => created,
            Err(TransportError::Refused { problem, .. }) if problem.code == ProblemCode::Forbidden => return Ok(false),
            Err(e) => return Err(e),
        };
        if matches!(first, Created::Made) && matches!(create().await?, Created::Made) {
            return Err(self.not_protocol(
                "the bucket let one object be created twice: it does not honour If-None-Match, and the order of a stream cannot be kept there".into(),
            ));
        }
        let _ = self.0.probed.set(());
        Ok(true)
    }

    /// Ready to write: probed once, and found writable.
    async fn writable(&self) -> Result<(), TransportError> {
        self.0
            .probed
            .get_or_try_init(|| async {
                match self.probe().await? {
                    true => Ok(()),
                    false => {
                        Err(self.refusal(ProblemCode::Forbidden, "the credentials do not write to the bucket".into()))
                    }
                }
            })
            .await
            .map(|_| ())
    }

    /// Which incarnation of `stream` the bucket has, if any.
    async fn record(&self, stream: &StreamName) -> Result<Option<StreamId>, TransportError> {
        let Some(answer) = self.get(Method::GET, &layout::record(stream)).await? else {
            return Ok(None);
        };
        let record: Record = wire::decode(&answer.body)
            .map_err(|e| self.not_protocol(format!("the record of stream {stream} does not read: {e}")))?;
        if record.format != layout::FORMAT {
            return Err(self.not_protocol(format!(
                "stream {stream} is laid out in format {}, and this efema reads format {}: it was written by a newer release",
                record.format,
                layout::FORMAT
            )));
        }
        Ok(Some(record.id))
    }

    /// Makes `stream` an incarnation - or finds the one another device made
    /// first.
    async fn create_record(&self, stream: &StreamName) -> Result<StreamId, TransportError> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|e| TransportError::Unreachable {
            at: "the system's random number generator".into(),
            source: Box::new(e),
        })?;
        let record = Record { id: StreamId::from_bytes(bytes), format: layout::FORMAT };
        match self.create(&layout::record(stream), wire::encode(&record), Vec::new()).await? {
            Created::Made => Ok(record.id),
            Created::Taken => self
                .record(stream)
                .await?
                .ok_or_else(|| self.not_protocol(format!("the record of stream {stream} was there, and is not"))),
        }
    }

    fn known(&self) -> MutexGuard<'_, Known> {
        self.0.known.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn remember(&self, id: StreamId, summary: Summary) {
        let mut known = self.known();
        if known.summaries.len() >= REMEMBERED {
            known.summaries.clear();
        }
        known.summaries.insert((id, summary.first), summary);
    }

    /// The batch at `first`, as its metadata tells it; `None` if there is
    /// none.
    async fn summary(&self, stream: &StreamName, id: StreamId, first: u64) -> Result<Option<Summary>, TransportError> {
        if let Some(summary) = self.known().summaries.get(&(id, first)) {
            return Ok(Some(*summary));
        }
        let key = layout::batch(stream, id, first);
        let Some(answer) = self.get(Method::HEAD, &key).await? else {
            return Ok(None);
        };
        let summary = Summary::from_headers(first, |name| answer.header(name)).ok_or_else(|| {
            self.not_protocol(format!("{key} is not a batch: its metadata does not say where it ends"))
        })?;
        self.remember(id, summary);
        Ok(Some(summary))
    }

    /// The batch at `first`, whole; `None` if there is none. Checked against
    /// its key and its metadata.
    async fn fetch(
        &self,
        stream: &StreamName,
        id: StreamId,
        first: u64,
    ) -> Result<Option<Arc<Stored>>, TransportError> {
        if let Some((known, stored)) = &self.known().body
            && *known == id
            && stored.first == first
        {
            return Ok(Some(stored.clone()));
        }
        let key = layout::batch(stream, id, first);
        let Some(answer) = self.get(Method::GET, &key).await? else {
            return Ok(None);
        };
        let stored: Stored =
            wire::decode(&answer.body).map_err(|e| self.not_protocol(format!("{key} is not a batch: {e}")))?;
        if stored.first != first || stored.batch.entries.is_empty() {
            return Err(self.not_protocol(format!("{key} holds a batch that does not start there")));
        }
        let summary = stored.summary();
        if Summary::from_headers(first, |name| answer.header(name)) != Some(summary) {
            return Err(self.not_protocol(format!("{key} says one thing in its metadata and holds another")));
        }
        self.remember(id, summary);
        let stored = Arc::new(stored);
        self.known().body = Some((id, stored.clone()));
        Ok(Some(stored))
    }

    /// The stream's newest batch, if it has any: the first key of a listing.
    async fn newest(&self, stream: &StreamName, id: StreamId) -> Result<Option<Summary>, TransportError> {
        let prefix = layout::batches(stream, id);
        let Some(key) = self.list(&prefix, None, Some(1)).await?.pop() else {
            return Ok(None);
        };
        self.listed(stream, id, &prefix, &key).await.map(Some)
    }

    /// The batch that holds position `seq`, if one does: the newest batch
    /// that starts at it or before it - the first key of a listing after the
    /// key of `seq + 1` - if it reaches that far.
    async fn holding(&self, stream: &StreamName, id: StreamId, seq: u64) -> Result<Option<Summary>, TransportError> {
        let remembered = self
            .known()
            .summaries
            .range(..=(id, seq))
            .next_back()
            .filter(|((known, _), summary)| *known == id && summary.last >= seq)
            .map(|(_, summary)| *summary);
        if remembered.is_some() {
            return Ok(remembered);
        }
        let prefix = layout::batches(stream, id);
        let after = seq.checked_add(1).map(|next| layout::batch(stream, id, next));
        let Some(key) = self.list(&prefix, after.as_deref(), Some(1)).await?.pop() else {
            return Ok(None);
        };
        let summary = self.listed(stream, id, &prefix, &key).await?;
        Ok((summary.last >= seq).then_some(summary))
    }

    /// The batch a listing named.
    async fn listed(
        &self,
        stream: &StreamName,
        id: StreamId,
        prefix: &str,
        key: &str,
    ) -> Result<Summary, TransportError> {
        let first = layout::first_of(key, prefix)
            .ok_or_else(|| self.not_protocol(format!("{key} is among the batches, and is not one")))?;
        self.summary(stream, id, first)
            .await?
            .ok_or_else(|| self.not_protocol(format!("{key} was listed, and is not there")))
    }

    /// Where the batch with `key` landed, if it did: at one of the places it
    /// was offered at.
    async fn landed(&self, stream: &StreamName, id: StreamId, key: &Hash) -> Result<Option<Summary>, TransportError> {
        let prefix = layout::offers(stream, id, key);
        for offer in self.list(&prefix, None, None).await? {
            let first = layout::offered_at(&offer, &prefix)
                .ok_or_else(|| self.not_protocol(format!("{offer} is among the offers, and is not one")))?;
            if let Some(summary) = self.summary(stream, id, first).await?
                && summary.key == *key
            {
                return Ok(Some(summary));
            }
        }
        Ok(None)
    }

    /// How a watched stream stands now. Looked at before: the next place
    /// only - a `HEAD` - unless something is there.
    async fn look(&self, stream: &StreamName, before: Option<Seen>) -> Result<Seen, TransportError> {
        let Some(id) = self.record(stream).await? else {
            return Ok(Seen { id: None, head: 0 });
        };
        if let Some(before) = before
            && before.id == Some(id)
            && self.summary(stream, id, before.head + 1).await?.is_none()
        {
            return Ok(before);
        }
        Ok(Seen { id: Some(id), head: self.newest(stream, id).await?.map_or(0, |newest| newest.last) })
    }
}

fn problem(code: ProblemCode, message: String) -> Problem {
    Problem { code, message, head: None, epoch: None, limit: None, retry: None }
}

/// Whether a watched stream is worth reading now - the relay's rule.
fn moved(watch: &Watch, seen: Seen) -> bool {
    match &watch.cursor {
        None => seen.head > 0,
        Some(cursor) => seen.id != Some(cursor.stream) || seen.head != cursor.seq,
    }
}

impl fmt::Display for Bucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base())
    }
}

impl fmt::Debug for Bucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bucket({}, {:?})", self.base(), self.0.credentials)
    }
}

impl Transport for Bucket {
    fn describe(&self) -> String {
        self.base()
    }

    /// The protocol's defaults, with no quota and nothing counted - and
    /// whether the credentials write, learnt by creating the probe object.
    async fn limits(&self) -> Result<Limits, TransportError> {
        Ok(Limits { write: self.probe().await?, ..LIMITS })
    }

    async fn append(&self, stream: &StreamName, batch: &Batch) -> Result<Written, TransportError> {
        if batch.entries.is_empty() {
            return Err(self.refusal(ProblemCode::BadRequest, "a batch has at least one entry".into()));
        }
        self.writable().await?;
        let id = match (self.record(stream).await?, batch.stream) {
            (Some(id), Some(expected)) if id != expected => {
                return Err(self.refusal(
                    ProblemCode::StreamReplaced,
                    format!("the batch expects stream {expected}, and {stream} is now stream {id}"),
                ));
            }
            (Some(id), _) => id,
            (None, Some(expected)) => {
                return Err(self.refusal(
                    ProblemCode::StreamNotFound,
                    format!("there is no stream named {stream}, and the batch expects stream {expected}"),
                ));
            }
            (None, None) => self.create_record(stream).await?,
        };
        // What a bucket takes never changes, so a batch it would refuse now
        // it refused when first sent: unlike on a relay, the size can be
        // looked at before the stream is.
        let size = wire::encode(batch).len() as u64;
        if size > LIMITS.body {
            let mut refusal = problem(
                ProblemCode::TooLarge,
                format!("the batch is {size} bytes, and a bucket takes at most {} in one", LIMITS.body),
            );
            refusal.limit = Some(LIMITS.body);
            return Err(TransportError::Refused { at: self.base(), problem: Box::new(refusal) });
        }
        if let Some(entry) = batch.entries.iter().find(|entry| entry.len() as u64 > LIMITS.entry) {
            let mut refusal = problem(
                ProblemCode::TooLarge,
                format!(
                    "an entry of the batch is {} bytes, and a bucket takes entries of at most {}",
                    entry.len(),
                    LIMITS.entry
                ),
            );
            refusal.limit = Some(LIMITS.entry);
            return Err(TransportError::Refused { at: self.base(), problem: Box::new(refusal) });
        }

        let key = batch.key();
        let mut stored = Stored {
            first: 0,
            previous: chain::genesis(&id),
            batch: Batch { epoch: batch.epoch, entries: batch.entries.clone(), stream: None },
        };
        let mut newest = self.newest(stream, id).await?;
        for _ in 0..PLACES {
            // A batch the stream has is answered, before the epoch is looked
            // at - and looked for after the head, every time: a batch at or
            // below the head seen was put after its offer, so the offer is
            // there to be found. Looked for first, a sending of the same batch
            // from elsewhere could land in between, and this one after it.
            if let Some(landed) = self.landed(stream, id, &key).await? {
                let epoch = newest.map_or(landed.epoch, |newest| newest.epoch.max(landed.epoch));
                return Ok(Written { stream: id, epoch, first: landed.first, last: landed.last, repeated: true });
            }
            let (after, epoch, previous) =
                newest.map_or((0, Epoch(0), chain::genesis(&id)), |n| (n.last, n.epoch, n.link));
            if batch.epoch < epoch {
                let mut refusal = problem(
                    ProblemCode::EpochBehind,
                    format!("the stream is at epoch {epoch}, and the batch was written in epoch {}", batch.epoch),
                );
                refusal.epoch = Some(epoch);
                return Err(TransportError::Refused { at: self.base(), problem: Box::new(refusal) });
            }
            stored.first = after + 1;
            stored.previous = previous;
            let summary = stored.summary();
            // The offer first: wherever the batch may land, an offer says so.
            self.put(&layout::offer(stream, id, &key, stored.first), Vec::new()).await?;
            let place = layout::batch(stream, id, stored.first);
            match self.create(&place, wire::encode(&stored), summary.headers()).await? {
                Created::Made => {
                    self.remember(id, summary);
                    return Ok(Written {
                        stream: id,
                        epoch: epoch.max(batch.epoch),
                        first: summary.first,
                        last: summary.last,
                        repeated: false,
                    });
                }
                // Another batch has the place - or this one, sent from
                // elsewhere, which the next round finds by its offer. The
                // stream went on at least to the place lost.
                Created::Taken => {
                    newest = match self.newest(stream, id).await? {
                        Some(now) if now.last >= stored.first => Some(now),
                        _ => Some(
                            self.summary(stream, id, stored.first)
                                .await?
                                .ok_or_else(|| self.not_protocol(format!("{place} was taken, and is not there")))?,
                        ),
                    };
                }
            }
        }
        Err(self.refusal(
            ProblemCode::RateLimited,
            format!("other writers took {PLACES} places in stream {stream} while this batch looked for one"),
        ))
    }

    async fn read(&self, stream: &StreamName, after: Option<&Cursor>, limit: usize) -> Result<Page, TransportError> {
        if !(1..=limits::PAGE_ENTRIES_MAX).contains(&limit) {
            return Err(self.refusal(
                ProblemCode::BadRequest,
                format!("a page is from 1 to {} entries", limits::PAGE_ENTRIES_MAX),
            ));
        }
        let id = self
            .record(stream)
            .await?
            .ok_or_else(|| self.refusal(ProblemCode::StreamNotFound, format!("there is no stream named {stream}")))?;
        if let Some(cursor) = after
            && cursor.stream != id
        {
            return Err(self.refusal(
                ProblemCode::StreamReplaced,
                format!("the cursor belongs to stream {}, and {stream} is now stream {id}", cursor.stream),
            ));
        }
        let newest = self.newest(stream, id).await?;
        let (head, epoch) = newest.map_or((0, Epoch(0)), |newest| (newest.last, newest.epoch));

        // Where the reader stands, checked against the history there.
        let mut holder = None;
        let from = match after {
            None => 0,
            Some(cursor) => {
                if cursor.seq > head {
                    let mut refusal = problem(
                        ProblemCode::CursorAhead,
                        format!("the cursor is at position {}, past the stream's last position {head}", cursor.seq),
                    );
                    refusal.head = Some(head);
                    return Err(TransportError::Refused { at: self.base(), problem: Box::new(refusal) });
                }
                let link = if cursor.seq == 0 {
                    chain::genesis(&id)
                } else {
                    let found = self.holding(stream, id, cursor.seq).await?.ok_or_else(|| {
                        self.not_protocol(format!("position {} of {stream} is in no batch", cursor.seq))
                    })?;
                    holder = Some(found);
                    if found.last == cursor.seq {
                        found.link
                    } else {
                        let stored = self.fetch(stream, id, found.first).await?.ok_or_else(|| {
                            self.not_protocol(format!("the batch at {} of {stream} was there, and is not", found.first))
                        })?;
                        stored.links().find(|(seq, ..)| *seq == cursor.seq).map(|(_, link, _)| link).ok_or_else(
                            || self.not_protocol(format!("the batch at {} does not reach {}", found.first, cursor.seq)),
                        )?
                    }
                };
                if link != cursor.hash {
                    return Err(self.refusal(
                        ProblemCode::CursorDiverged,
                        format!("the stream's history at position {} is not the one the cursor holds", cursor.seq),
                    ));
                }
                cursor.seq
            }
        };

        let mut entries = Vec::new();
        let mut bytes = 0;
        let mut next = from + 1;
        // The reader's own batch, if it stopped inside one; else the one
        // starting right after it.
        let mut start = match holder {
            Some(holder) if next <= holder.last => holder.first,
            _ => next,
        };
        'page: while next <= head && entries.len() < limit {
            let stored = self.fetch(stream, id, start).await?.ok_or_else(|| {
                self.not_protocol(format!("stream {stream} has no batch at {start}, below its last position {head}"))
            })?;
            for entry in stored.entries_from(next) {
                if entries.len() == limit || (!entries.is_empty() && bytes + entry.data.len() > limits::PAGE_BYTES) {
                    break 'page;
                }
                bytes += entry.data.len();
                next = entry.seq + 1;
                entries.push(entry);
            }
            start = stored.summary().last + 1;
        }
        Ok(Page { stream: id, epoch, head, entries })
    }

    async fn wait(&self, watches: &[Watch], timeout: Duration) -> Result<Heads, TransportError> {
        let bad = |message: String| self.refusal(ProblemCode::BadRequest, message);
        if watches.is_empty() {
            return Err(bad("a wait names at least one stream to watch".into()));
        }
        if watches.len() > limits::WATCHES_MAX {
            return Err(bad(format!("a wait watches at most {} streams", limits::WATCHES_MAX)));
        }
        if let Some((twice, _)) =
            watches.iter().enumerate().find(|(i, watch)| watches[..*i].iter().any(|w| w.stream == watch.stream))
        {
            return Err(bad(format!("stream {} is watched twice", watches[twice].stream)));
        }
        if timeout > limits::WAIT_MAX {
            return Err(bad(format!("a wait lasts at most {} s", limits::WAIT_MAX.as_secs())));
        }
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::with_capacity(watches.len());
        for watch in watches {
            seen.push(self.look(&watch.stream, None).await?);
        }
        loop {
            let heads: Vec<Head> = watches
                .iter()
                .zip(&seen)
                .filter(|(watch, seen)| moved(watch, **seen))
                .map(|(watch, seen)| Head { stream: watch.stream.clone(), id: seen.id, head: seen.head })
                .collect();
            let now = Instant::now();
            if !heads.is_empty() || now >= deadline {
                return Ok(Heads { heads });
            }
            tokio::time::sleep(Self::POLL.min(deadline - now)).await;
            for (watch, seen) in watches.iter().zip(seen.iter_mut()) {
                *seen = self.look(&watch.stream, Some(*seen)).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY")
    }

    #[test]
    fn a_bucket_is_named_by_its_own_address() {
        let virtual_host =
            Bucket::new("https://notes.s3.eu-central-1.amazonaws.com", "eu-central-1", credentials()).unwrap();
        assert_eq!(
            (virtual_host.0.path.as_str(), virtual_host.0.host.as_str()),
            ("", "notes.s3.eu-central-1.amazonaws.com")
        );
        let path = Bucket::new("http://127.0.0.1:9000/notes/", "us-east-1", credentials()).unwrap();
        assert_eq!((path.0.path.as_str(), path.0.host.as_str()), ("/notes", "127.0.0.1:9000"));
        assert_eq!(path.describe(), "http://127.0.0.1:9000/notes");
        let ipv6 = Bucket::new("http://[::1]:9000/notes", "us-east-1", credentials()).unwrap();
        assert_eq!(ipv6.0.host, "[::1]:9000");
        for (url, region) in [
            ("http://s3.example.com/notes", "us-east-1"),
            ("https://s3.example.com/notes?versioning", "us-east-1"),
            ("https://s3.example.com/notes", ""),
            ("https://s3.example.com/notes", "eu central"),
            ("ftp://127.0.0.1/notes", "us-east-1"),
        ] {
            assert!(Bucket::new(url, region, credentials()).is_err(), "{url} in {region:?} is taken");
        }
    }

    #[test]
    fn the_secret_is_not_in_what_a_bucket_prints() {
        let bucket = Bucket::new("https://notes.example.com", "auto", credentials()).unwrap();
        let printed = format!("{bucket:?} {bucket}");
        assert!(!printed.contains("EXAMPLEKEY"), "{printed}");
        assert!(printed.contains("AKIDEXAMPLE"), "{printed}");
    }

    fn answer(status: u16, body: &str) -> Answer {
        Answer {
            status: StatusCode::from_u16(status).unwrap(),
            headers: HeaderMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn code(bucket: &Bucket, status: u16, body: &str) -> Option<ProblemCode> {
        bucket.refused(&answer(status, body)).code().cloned()
    }

    #[test]
    fn a_refusal_of_s3_is_said_in_the_protocol() {
        let bucket = Bucket::new("https://notes.example.com", "auto", credentials()).unwrap();
        let error = |code: &str| format!("<Error><Code>{code}</Code><Message>m</Message></Error>");
        assert_eq!(code(&bucket, 403, &error("AccessDenied")), Some(ProblemCode::Forbidden));
        assert_eq!(code(&bucket, 403, &error("SignatureDoesNotMatch")), Some(ProblemCode::Unauthorized));
        assert_eq!(code(&bucket, 403, &error("InvalidAccessKeyId")), Some(ProblemCode::Unauthorized));
        assert_eq!(code(&bucket, 403, &error("RequestTimeTooSkewed")), Some(ProblemCode::Unauthorized));
        assert_eq!(code(&bucket, 404, &error("NoSuchBucket")), Some(ProblemCode::NotFound));
        assert_eq!(code(&bucket, 503, &error("SlowDown")), Some(ProblemCode::RateLimited));
        assert_eq!(code(&bucket, 501, &error("NotImplemented")), Some(ProblemCode::Internal));
        assert_eq!(code(&bucket, 400, &error("InvalidArgument")), Some(ProblemCode::Other("InvalidArgument".into())));
        // An answer without a body, as to HEAD: the status says it.
        assert_eq!(code(&bucket, 403, ""), Some(ProblemCode::Forbidden));
        assert_eq!(code(&bucket, 500, ""), Some(ProblemCode::Internal));
        assert_eq!(code(&bucket, 429, ""), Some(ProblemCode::RateLimited));
        // Something that is not S3 at all.
        assert!(matches!(bucket.refused(&answer(418, "<html>teapot</html>")), TransportError::NotProtocol { .. }));
        let TransportError::Refused { problem, .. } = bucket.refused(&answer(503, &error("SlowDown"))) else {
            panic!("a refusal");
        };
        assert_eq!(problem.retry, Some(1));
    }

    #[test]
    fn a_bucket_announces_no_quota_and_counts_nothing() {
        assert_eq!((LIMITS.quota, LIMITS.used, LIMITS.rate), (u64::MAX, 0, u64::MAX));
        assert_eq!(LIMITS.body, Limits::DEFAULT.body);
        assert_eq!(LIMITS.entry, Limits::DEFAULT.entry);
    }
}
