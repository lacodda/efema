//! How a client reaches a stream.
//!
//! A transport speaks the protocol and nothing more: it appends batches,
//! reads pages and waits, with entries it never looks into. Sealing, cursors
//! and keys live above it, in [`Client`](crate::Client), so a transport is
//! small and a second one - an S3 bucket, a WebDAV share - is the protocol
//! over another medium, not a second client.

use std::future::Future;
use std::time::Duration;

use efema_proto::wire::{Batch, Heads, Limits, Page, Problem, ProblemCode, Watch, Written};
use efema_proto::{Cursor, StreamName};

/// A way to reach streams.
///
/// [`Relay`](crate::Relay) is the one this release ships: an efema relay over
/// HTTP. Every method takes `&self` and may be called from several tasks at
/// once.
pub trait Transport: Send + Sync + 'static {
    /// Where this transport leads, for people: shown by
    /// [`Client::doctor`](crate::Client::doctor).
    fn describe(&self) -> String;

    /// What the relay holds this device's access key to: whether it writes,
    /// what one request may carry and one page may hold, and how much its
    /// space may keep. Asked of the relay; a client keeps the answer in the
    /// device's state and asks again when the relay refuses something as
    /// too large.
    fn limits(&self) -> impl Future<Output = Result<Limits, TransportError>> + Send;

    /// Appends a batch to a stream: all of its entries on consecutive
    /// positions, or none of them.
    fn append(
        &self,
        stream: &StreamName,
        batch: &Batch,
    ) -> impl Future<Output = Result<Written, TransportError>> + Send;

    /// Reads at most `limit` entries after `after`, or from the start.
    fn read(
        &self,
        stream: &StreamName,
        after: Option<&Cursor>,
        limit: usize,
    ) -> impl Future<Output = Result<Page, TransportError>> + Send;

    /// Waits up to `timeout` for any of the watched streams to move.
    fn wait(&self, watches: &[Watch], timeout: Duration) -> impl Future<Output = Result<Heads, TransportError>> + Send;
}

/// Why a transport could not do what it was asked.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The relay refused, and said why.
    #[error("{} refused: {} ({})", .at, .problem.message, .problem.code)]
    Refused {
        /// Where the refusal came from.
        at: String,
        /// The relay's reason - boxed, so an error is small to pass around
        /// when nothing went wrong.
        problem: Box<Problem>,
    },
    /// The relay could not be reached, or the connection broke.
    #[error("cannot reach {at}")]
    Unreachable {
        /// Where the transport tried to go.
        at: String,
        /// What the network said.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The connection went silent: nothing moved on it for as long as the
    /// request allowed - [`STALL`](crate::STALL), or a wait's time and that
    /// on top. A network that dropped without a word, or a proxy whose
    /// upstream hung.
    #[error("{at} went silent: nothing moved on the connection for {} s", .quiet.as_secs())]
    Stalled {
        /// Where the transport tried to go.
        at: String,
        /// How long nothing moved.
        quiet: Duration,
    },
    /// Something answered, but not in the protocol: not an efema relay, or a
    /// proxy in front of it answering for itself.
    #[error("{at} answered something that is not the efema protocol: {reason}")]
    NotProtocol {
        /// Where the answer came from.
        at: String,
        /// What was wrong with it.
        reason: String,
    },
}

impl TransportError {
    /// The refusal's code, if the relay refused.
    pub fn code(&self) -> Option<&ProblemCode> {
        match self {
            Self::Refused { problem, .. } => Some(&problem.code),
            _ => None,
        }
    }
}
