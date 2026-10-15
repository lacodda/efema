//! The transport to an efema relay, over HTTP.

use std::fmt;
use std::time::Duration;

use efema_proto::wire::{self, Batch, Heads, Limits, Page, Problem, Watch, Written};
use efema_proto::{AccessKey, Cursor, MEDIA_TYPE, StreamName};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{RequestBuilder, Response, Url};

use crate::transport::{Transport, TransportError};

/// How long a write or a read may take as a whole. Generous: a full request
/// body is 16 MiB, and a phone on a poor network moves that slowly.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// How long reaching the relay may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How much longer than the wait itself a wait request may take, for the
/// round trip around it.
const WAIT_SLACK: Duration = Duration::from_secs(30);

/// An efema relay, reached over HTTPS - or over plain HTTP on this machine -
/// with an access key that opens one of its spaces.
///
/// HTTPS is verified against the platform's trust store, so a relay behind a
/// proxy with a private certificate authority is trusted once that authority
/// is installed on the device. Plain HTTP is for a relay on the device
/// itself: the key goes with every request, and over plain HTTP across a
/// network anyone on the way would read it. The relay refuses such a request
/// too; this refuses to send it.
///
/// ```
/// let key: efema::AccessKey = std::env::var("EFEMA_KEY")
///     .unwrap_or_else(|_| format!("efema_{}", "0".repeat(64)))
///     .parse()?;
/// let relay = efema::Relay::new("https://sync.example.com/efema/", key.clone())?;
/// assert_eq!(relay.to_string(), "https://sync.example.com/efema");
/// assert!(efema::Relay::new("http://sync.example.com", key.clone()).is_err());
/// assert!(efema::Relay::new("http://127.0.0.1:2451", key).is_ok());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct Relay {
    base: String,
    http: reqwest::Client,
    /// `Bearer <key>`, marked sensitive so no HTTP layer prints it.
    authorization: HeaderValue,
}

/// Why a text is not the address of a relay.
#[derive(Debug, thiserror::Error)]
#[error("`{url}` is not a relay address: {reason}")]
pub struct InvalidUrl {
    url: String,
    reason: String,
}

impl Relay {
    /// The relay at `url`, reached with `key`: `https://`, or `http://` to
    /// this machine (`localhost`, `127.0.0.1`, `::1`), and a path if the relay
    /// is served under one.
    ///
    /// # Errors
    ///
    /// [`InvalidUrl`] when `url` is not an HTTP address, or is plain HTTP to
    /// another machine.
    pub fn new(url: &str, key: AccessKey) -> Result<Self, InvalidUrl> {
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
        if parsed.scheme() == "http" && !on_this_machine(&parsed) {
            return Err(invalid(
                "plain http goes only to this machine (localhost, 127.0.0.1, ::1): to another, the access key would cross the network in the clear - use https",
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("efema/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| invalid(&e.to_string()))?;
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {}", key.expose())).expect("an access key is printable ASCII");
        authorization.set_sensitive(true);
        Ok(Self { base: parsed.as_str().trim_end_matches('/').to_string(), http, authorization })
    }

    /// A request with the key on it.
    fn authorized(&self, request: RequestBuilder) -> RequestBuilder {
        request.header(AUTHORIZATION, self.authorization.clone())
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
            Err(TransportError::Refused { at: self.base.clone(), problem: Box::new(problem) })
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
        let request = self.http.get(self.url(&format!("{}/limits", efema_proto::API_PREFIX))).timeout(REQUEST_TIMEOUT);
        let response = self.authorized(request).send().await.map_err(|e| self.unreachable(e))?;
        self.answer(response).await
    }

    async fn append(&self, stream: &StreamName, batch: &Batch) -> Result<Written, TransportError> {
        let request = self.http.post(self.entries(stream));
        let response = self
            .authorized(request)
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
        let request = self.http.get(url).timeout(REQUEST_TIMEOUT);
        let response = self.authorized(request).send().await.map_err(|e| self.unreachable(e))?;
        self.answer(response).await
    }

    async fn wait(&self, watches: &[Watch], timeout: Duration) -> Result<Heads, TransportError> {
        let mut url = self.url(&format!("{}/wait?timeout={}", efema_proto::API_PREFIX, timeout.as_secs()));
        for watch in watches {
            url.push_str(&format!("&watch={watch}"));
        }
        let request = self.http.get(url).timeout(timeout + WAIT_SLACK);
        let response = self.authorized(request).send().await.map_err(|e| self.unreachable(e))?;
        self.answer(response).await
    }
}

/// Whether `url` leads to this machine: the only place plain HTTP may carry
/// a key.
fn on_this_machine(url: &Url) -> bool {
    // An IPv6 host comes in brackets.
    let Some(host) = url.host_str().map(|host| host.trim_start_matches('[').trim_end_matches(']')) else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> AccessKey {
        format!("efema_{}", "0".repeat(64)).parse().unwrap()
    }

    #[test]
    fn plain_http_carries_a_key_only_to_this_machine() {
        for url in [
            "https://sync.example.com",
            "https://sync.example.com/efema/",
            "http://localhost:2451",
            "http://LOCALHOST",
            "http://127.0.0.1:2451",
            "http://127.0.0.2",
            "http://[::1]:2451",
        ] {
            assert!(Relay::new(url, key()).is_ok(), "{url}");
        }
        for url in [
            "http://sync.example.com",
            "http://localhost.example.com",
            "http://127.0.0.1.example.com",
            "http://192.0.2.1:2451",
            "http://[2001:db8::1]:2451",
            "http://0.0.0.0:2451",
            "ftp://127.0.0.1",
            "https://sync.example.com/?space=home",
            "sync.example.com",
        ] {
            let refused = Relay::new(url, key()).err().unwrap_or_else(|| panic!("{url} is taken"));
            assert!(!refused.to_string().contains("efema_"), "{refused}");
        }
    }
}
