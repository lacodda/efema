//! The transport to an efema relay, over HTTP.

use std::fmt;
use std::time::Duration;

use efema_proto::wire::{self, Batch, Heads, Page, Problem, Watch, Written};
use efema_proto::{Cursor, MEDIA_TYPE, StreamName};
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use reqwest::{Response, Url};

use crate::transport::{Limits, Transport, TransportError};

/// How long a write or a read may take as a whole. Generous: a full request
/// body is 16 MiB, and a phone on a poor network moves that slowly.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// How long reaching the relay may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How much longer than the wait itself a wait request may take, for the
/// round trip around it.
const WAIT_SLACK: Duration = Duration::from_secs(30);

/// An efema relay, reached over HTTP or HTTPS.
///
/// HTTPS is verified against the platform's trust store, so a relay behind a
/// proxy with a private certificate authority is trusted once that authority
/// is installed on the device.
///
/// ```
/// let relay = efema::Relay::new("https://sync.example.com/efema/")?;
/// assert_eq!(relay.to_string(), "https://sync.example.com/efema");
/// # Ok::<(), efema::InvalidUrl>(())
/// ```
#[derive(Clone)]
pub struct Relay {
    base: String,
    http: reqwest::Client,
}

/// Why a text is not the address of a relay.
#[derive(Debug, thiserror::Error)]
#[error("`{url}` is not a relay address: {reason}")]
pub struct InvalidUrl {
    url: String,
    reason: String,
}

impl Relay {
    /// The relay at `url`: `http://` or `https://`, and a path if the relay is
    /// served under one.
    ///
    /// # Errors
    ///
    /// [`InvalidUrl`] when `url` is not an HTTP address.
    pub fn new(url: &str) -> Result<Self, InvalidUrl> {
        let invalid = |reason: &str| InvalidUrl { url: url.to_string(), reason: reason.to_string() };
        let parsed = Url::parse(url).map_err(|e| invalid(&e.to_string()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(invalid("the scheme must be http or https"));
        }
        if parsed.host().is_none() {
            return Err(invalid("there is no host"));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(invalid("a relay address has no query and no fragment"));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("efema/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| invalid(&e.to_string()))?;
        Ok(Self { base: parsed.as_str().trim_end_matches('/').to_string(), http })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn entries(&self, stream: &StreamName) -> String {
        self.url(&format!("{}/streams/{stream}/entries", efema_proto::API_PREFIX))
    }

    fn unreachable(&self, source: reqwest::Error) -> TransportError {
        TransportError::Unreachable { at: self.base.clone(), source: Box::new(source.without_url()) }
    }

    async fn answer<T: for<'b> minicbor::Decode<'b, ()>>(&self, response: Response) -> Result<T, TransportError> {
        let status = response.status();
        let cbor =
            response.headers().get(CONTENT_TYPE).and_then(|value| value.to_str().ok()).is_some_and(|value| {
                value.split(';').next().unwrap_or_default().trim().eq_ignore_ascii_case(MEDIA_TYPE)
            });
        if !cbor {
            return Err(TransportError::NotProtocol {
                at: self.base.clone(),
                reason: format!("a {status} answer that is not {MEDIA_TYPE}"),
            });
        }
        let body = response.bytes().await.map_err(|e| self.unreachable(e))?;
        let not_protocol =
            |e: wire::WireError| TransportError::NotProtocol { at: self.base.clone(), reason: e.to_string() };
        if status.is_success() {
            wire::decode(&body).map_err(not_protocol)
        } else {
            let problem: Problem = wire::decode(&body).map_err(not_protocol)?;
            Err(TransportError::Refused { at: self.base.clone(), problem })
        }
    }
}

impl fmt::Display for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base)
    }
}

impl fmt::Debug for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Relay({})", self.base)
    }
}

impl Transport for Relay {
    fn describe(&self) -> String {
        self.base.clone()
    }

    async fn limits(&self) -> Result<Limits, TransportError> {
        Ok(Limits::V1)
    }

    async fn append(&self, stream: &StreamName, batch: &Batch) -> Result<Written, TransportError> {
        let response = self
            .http
            .post(self.entries(stream))
            .header(CONTENT_TYPE, HeaderValue::from_static(MEDIA_TYPE))
            .timeout(REQUEST_TIMEOUT)
            .body(wire::encode(batch))
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        self.answer(response).await
    }

    async fn read(&self, stream: &StreamName, after: Option<&Cursor>, limit: usize) -> Result<Page, TransportError> {
        // Cursors, numbers and names are all URL-safe as they are spelled.
        let mut url = format!("{}?limit={limit}", self.entries(stream));
        if let Some(cursor) = after {
            url.push_str(&format!("&after={cursor}"));
        }
        let response = self.http.get(url).timeout(REQUEST_TIMEOUT).send().await.map_err(|e| self.unreachable(e))?;
        self.answer(response).await
    }

    async fn wait(&self, watches: &[Watch], timeout: Duration) -> Result<Heads, TransportError> {
        let mut url = self.url(&format!("{}/wait?timeout={}", efema_proto::API_PREFIX, timeout.as_secs()));
        for watch in watches {
            url.push_str(&format!("&watch={watch}"));
        }
        let response =
            self.http.get(url).timeout(timeout + WAIT_SLACK).send().await.map_err(|e| self.unreachable(e))?;
        self.answer(response).await
    }
}
