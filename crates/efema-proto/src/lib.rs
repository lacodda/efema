//! The wire format of [efema](https://github.com/lacodda/efema): what a client
//! and a relay say to each other.
//!
//! efema is a sync relay for local-first apps. A device writes its changes as
//! a *batch* of opaque entries into a named *stream*; the relay gives each
//! entry the next *position* and keeps it; every other device reads what came
//! after its *cursor*. The relay never looks inside an entry - it cannot tell
//! a note from a password, and merging is left to the app.
//!
//! This crate is that conversation and nothing else: the messages, their CBOR
//! encoding, the stream's names and identities, the cursor and the hash chain
//! that makes a cursor checkable. The relay (`efema-server`) and the client
//! library both build on it, so the two cannot drift apart: there is one
//! definition of each message, and it lives here.
//!
//! # The pieces
//!
//! - [`StreamName`] - the name a stream is addressed by, in the URL.
//! - [`StreamId`] - which *incarnation* of that name: a fresh random value
//!   when the stream is created, so a cursor from a stream that was wiped and
//!   started again is recognised as foreign instead of silently reused.
//! - [`Epoch`] - a number the app raises when it changes the format of what
//!   it writes. The relay only compares epochs; it never knows what they mean.
//! - [`Hash`](struct@Hash) and [`chain`] - every entry is linked to the one before it, so a
//!   cursor names not just a position but the history up to it.
//! - [`Cursor`] - where a reader stands: stream, position and the hash there.
//! - [`wire`] - the request and response bodies, encoded as CBOR.
//!
//! ```
//! use efema_proto::{chain, Epoch, StreamId};
//!
//! let stream = StreamId::from_bytes([7; 16]);
//! let start = chain::genesis(&stream);
//! let first = chain::link(&start, 1, Epoch(1), b"sealed bytes");
//! // The same history always gives the same hash...
//! assert_eq!(first, chain::link(&start, 1, Epoch(1), b"sealed bytes"));
//! // ...and a different one never does.
//! assert_ne!(first, chain::link(&start, 1, Epoch(2), b"sealed bytes"));
//! ```

pub mod chain;
mod cursor;
mod hex;
mod ids;
mod name;
pub mod wire;

pub use cursor::{Cursor, ParseCursorError};
pub use ids::{Epoch, Hash, ParseIdError, StreamId};
pub use name::{InvalidName, MAX_NAME_LEN, StreamName};

/// The media type of every request and response body in protocol version 1.
pub const MEDIA_TYPE: &str = "application/cbor";

/// The path every endpoint of protocol version 1 lives under.
///
/// A later major version of the protocol gets a path of its own, so a relay can
/// serve two versions while its clients move from one to the other.
pub const API_PREFIX: &str = "/v1";
