//! The bodies of requests and responses, and their CBOR encoding.
//!
//! Every body is a CBOR map with small integer keys - the numbers in `#[n(..)]`
//! below - rather than field names. Two rules keep the format open to change
//! without breaking anyone: a decoder ignores keys it does not know, and a
//! field added later is optional, so an older peer that never sends it is
//! still understood. A required field, once here, is here for the life of the
//! protocol version.
//!
//! Entry data is a CBOR byte string, carried as is: no base64, no escaping, no
//! growth. The relay stores those bytes and returns them; it never decodes
//! them, and nothing in this module looks inside them either.

use std::fmt;
use std::str::FromStr;

use minicbor::decode::{Decoder, Error as DecodeError};
use minicbor::encode::{Encoder, Error as EncodeError, Write};
use minicbor::{Decode, Encode};
use sha2::{Digest, Sha256};

use crate::{Cursor, Epoch, Hash, StreamId, StreamName};

/// A write: entries to append to a stream, in this order, as one unit.
///
/// The relay gives the entries consecutive positions or none at all: a batch
/// is never half written, and no other writer's entry lands between two
/// entries of one batch.
///
/// A batch is known by what it holds - its [`key`](Batch::key). Sent again to
/// a stream that already has it, it is not written a second time: the answer
/// says where it landed the first time. So a writer whose answer was lost on
/// the way back sends the same batch again, byte for byte, and the stream
/// holds it once.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Batch {
    /// The epoch the entries are written in. Lower than the stream's current
    /// epoch is refused; higher raises the stream to it.
    #[n(0)]
    pub epoch: Epoch,
    /// The entries, opaque to the relay.
    #[n(1)]
    #[cbor(with = "blobs")]
    pub entries: Vec<Vec<u8>>,
    /// The stream incarnation the writer means to append to. With it, the
    /// relay refuses the batch if the name now belongs to another incarnation
    /// (`stream_replaced`) or to none (`stream_not_found`) - instead of
    /// creating a fresh stream under the name and starting it with entries
    /// that only make sense after a history it no longer has. Without it, the
    /// batch goes to whatever stream has the name, and creates one if none
    /// does: how a stream's first batch is written.
    #[n(2)]
    pub stream: Option<StreamId>,
}

/// Domain separation for a batch's key: no other SHA-256 input in efema
/// starts with these bytes.
const BATCH_DOMAIN: &[u8] = b"efema/batch/v1";

impl Batch {
    /// What the batch is known by: SHA-256 over its epoch and its entries,
    /// each length-prefixed, so no two different batches feed the hash the
    /// same bytes - moving a byte from one entry to the next changes the
    /// input rather than reshuffling it.
    ///
    /// The stream the batch names is not in it: a batch is the same batch
    /// whether its first sending created the stream or named it.
    ///
    /// ```text
    /// key = SHA-256("efema/batch/v1" || epoch: u32 || count: u64 || (length(data): u64 || data) for each entry)
    /// ```
    pub fn key(&self) -> Hash {
        let mut hasher = Sha256::new();
        hasher.update(BATCH_DOMAIN);
        hasher.update(self.epoch.0.to_be_bytes());
        hasher.update((self.entries.len() as u64).to_be_bytes());
        for data in &self.entries {
            hasher.update((data.len() as u64).to_be_bytes());
            hasher.update(data);
        }
        Hash::from_bytes(hasher.finalize().into())
    }
}

/// The answer to a write: where the batch landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Written {
    /// The stream incarnation the batch was written into - new if this batch
    /// created the stream.
    #[n(0)]
    pub stream: StreamId,
    /// The stream's epoch after the write.
    #[n(1)]
    pub epoch: Epoch,
    /// The position of the batch's first entry.
    #[n(2)]
    pub first: u64,
    /// The position of the batch's last entry.
    #[n(3)]
    pub last: u64,
    /// Whether the stream had this batch already, and nothing was written
    /// now: the positions are where it landed the first time. On the wire
    /// only when true.
    #[n(4)]
    #[cbor(skip_if = "is_false")]
    pub repeated: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The answer to a read: the entries after the cursor, in order.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Page {
    /// The stream incarnation that was read.
    #[n(0)]
    pub stream: StreamId,
    /// The stream's current epoch.
    #[n(1)]
    pub epoch: Epoch,
    /// The stream's last position at the moment of the read. When it is past
    /// the last entry of this page, there is more to read.
    #[n(2)]
    pub head: u64,
    /// The entries, in position order, with no gaps.
    #[n(3)]
    pub entries: Vec<Entry>,
}

impl Page {
    /// The cursor after the last entry of this page, or `None` for an empty
    /// page - the reader's cursor is unchanged then.
    pub fn cursor(&self) -> Option<Cursor> {
        self.entries.last().map(|entry| entry.cursor(self.stream))
    }
}

/// One entry of a stream, as a reader receives it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Entry {
    /// The entry's position.
    #[n(0)]
    pub seq: u64,
    /// The epoch the entry was written in.
    #[n(1)]
    pub epoch: Epoch,
    /// The chain link at this position.
    #[n(2)]
    pub hash: Hash,
    /// The bytes as the writer sent them.
    #[n(3)]
    #[cbor(with = "minicbor::bytes")]
    pub data: Vec<u8>,
}

impl Entry {
    /// The cursor of a reader that has this entry and everything before it.
    pub fn cursor(&self, stream: StreamId) -> Cursor {
        Cursor { stream, seq: self.seq, hash: self.hash }
    }
}

/// What a waiting reader knows about one stream, and asks to be woken about.
///
/// The text form is the value of a `watch` query parameter: the stream's name,
/// then `:` and a cursor if the reader has one. Without a cursor the reader is
/// woken by the stream's first entry; with one, by any change from what the
/// cursor says - new entries, or a stream that is not the one the cursor
/// belongs to any more.
///
/// ```
/// use efema_proto::wire::Watch;
///
/// let watch: Watch = "notes".parse().unwrap();
/// assert!(watch.cursor.is_none());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watch {
    /// The stream to watch.
    pub stream: StreamName,
    /// Where the reader stands in it, if anywhere.
    pub cursor: Option<Cursor>,
}

impl fmt::Display for Watch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cursor {
            Some(cursor) => write!(f, "{}:{cursor}", self.stream),
            None => write!(f, "{}", self.stream),
        }
    }
}

/// Why a text is not a watch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseWatchError(String);

impl fmt::Display for ParseWatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseWatchError {}

impl FromStr for Watch {
    type Err = ParseWatchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, cursor) = match s.split_once(':') {
            Some((name, cursor)) => (name, Some(cursor)),
            None => (s, None),
        };
        let stream = name.parse().map_err(|e: crate::InvalidName| ParseWatchError(e.to_string()))?;
        let cursor =
            cursor.map(|text| text.parse::<Cursor>().map_err(|e| ParseWatchError(e.to_string()))).transpose()?;
        Ok(Self { stream, cursor })
    }
}

/// The answer to a wait: the watched streams that differ from what the
/// waiter said it knew. Empty when the wait ran out with nothing to report.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Heads {
    /// One record per stream worth reading now.
    #[n(0)]
    pub heads: Vec<Head>,
}

/// A stream's state as a wait reports it. Positions only - a wait never
/// carries entries.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Head {
    /// The stream's name.
    #[n(0)]
    pub stream: StreamName,
    /// The stream's current incarnation, or `None` if no stream has that name.
    #[n(1)]
    pub id: Option<StreamId>,
    /// The stream's last position; 0 if there is no stream.
    #[n(2)]
    pub head: u64,
}

/// What a relay accepts from one access key, and what the key may do: the
/// answer to `GET /v1/limits`.
///
/// A client cuts its batches and pages to these rather than to the
/// protocol's ceilings in [`crate::limits`]: an operator may have set any of
/// them lower. Sizes are in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Limits {
    /// Whether the key writes. A key that does not only reads: its writes are
    /// refused with `forbidden`.
    #[n(0)]
    pub write: bool,
    /// The largest request body - a batch, as CBOR.
    #[n(1)]
    pub body: u64,
    /// The largest entry.
    #[n(2)]
    pub entry: u64,
    /// Bytes of entry data the key's space may hold, all its streams together.
    #[n(3)]
    pub quota: u64,
    /// Bytes of entry data the space holds at the moment of the answer.
    #[n(4)]
    pub used: u64,
    /// Requests the key may make in a minute.
    #[n(5)]
    pub rate: u64,
    /// Entries in a page when the reader does not say.
    #[n(6)]
    pub page_entries: u64,
    /// The most entries a reader may ask for.
    #[n(7)]
    pub page_entries_max: u64,
    /// Bytes of entry data in a page, beyond which it ends early.
    #[n(8)]
    pub page_bytes: u64,
    /// The longest wait, in seconds.
    #[n(9)]
    pub wait_max: u64,
    /// The most streams one wait may watch.
    #[n(10)]
    pub watches: u64,
}

impl Limits {
    /// What a relay with its operator's defaults announces to a key that
    /// writes, in a space that holds nothing yet.
    pub const DEFAULT: Self = Self {
        write: true,
        body: crate::limits::MAX_BODY_BYTES as u64,
        entry: crate::limits::ENTRY_BYTES_DEFAULT as u64,
        quota: crate::limits::SPACE_QUOTA_DEFAULT,
        used: 0,
        rate: crate::limits::RATE_DEFAULT as u64,
        page_entries: crate::limits::PAGE_ENTRIES_DEFAULT as u64,
        page_entries_max: crate::limits::PAGE_ENTRIES_MAX as u64,
        page_bytes: crate::limits::PAGE_BYTES as u64,
        wait_max: crate::limits::WAIT_MAX.as_secs(),
        watches: crate::limits::WATCHES_MAX as u64,
    };
}

/// Why the relay refused a request: the body of every error response.
///
/// `code` is for programs and never changes its spelling; `message` is for
/// people and may. The optional fields carry what a program needs to act on a
/// particular code.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Problem {
    /// What went wrong, as a stable code.
    #[n(0)]
    pub code: ProblemCode,
    /// What went wrong, in a sentence.
    #[n(1)]
    pub message: String,
    /// The stream's last position - with `cursor_ahead`.
    #[n(2)]
    pub head: Option<u64>,
    /// The stream's epoch - with `epoch_behind`.
    #[n(3)]
    pub epoch: Option<Epoch>,
    /// The limit that was exceeded, in bytes - with `too_large` and
    /// `quota_exceeded`.
    #[n(4)]
    pub limit: Option<u64>,
    /// Seconds until the next request is let through - with `rate_limited`.
    #[n(5)]
    pub retry: Option<u64>,
}

/// The reasons a relay refuses a request. Each has one spelling on the wire,
/// returned by [`ProblemCode::as_str`].
///
/// A code this version does not know decodes as [`ProblemCode::Other`]: a
/// newer relay may add reasons, and an older client must still be able to read
/// the response that carries one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProblemCode {
    /// The request is malformed: a body that is not the expected CBOR, a query
    /// parameter that does not parse, an empty batch.
    BadRequest,
    /// No stream has that name.
    StreamNotFound,
    /// The cursor belongs to another incarnation of the stream.
    StreamReplaced,
    /// The cursor is past the stream's last position: the relay lost entries
    /// the reader has.
    CursorAhead,
    /// The stream's chain has a different link at the cursor's position: the
    /// relay's history and the reader's have parted.
    CursorDiverged,
    /// The batch is written in an epoch older than the stream's.
    EpochBehind,
    /// The body is larger than the relay accepts.
    TooLarge,
    /// The body is not `application/cbor`.
    UnsupportedMediaType,
    /// No access key, or one the relay does not know - never handed out, or
    /// revoked.
    Unauthorized,
    /// The access key does not allow this: a key that only reads, sent to
    /// write. Also a request for something the relay serves only on its own
    /// machine.
    Forbidden,
    /// The request reached the relay over plain HTTP from outside: not
    /// through a proxy that took it over HTTPS, and not from the relay's own
    /// machine.
    HttpsRequired,
    /// The access key has made more requests than the relay allows in a
    /// minute.
    RateLimited,
    /// The batch would take the space past the bytes it may hold.
    QuotaExceeded,
    /// No such endpoint.
    NotFound,
    /// The endpoint exists, but not for this method.
    MethodNotAllowed,
    /// Something failed on the relay's side.
    Internal,
    /// A code this version of the protocol does not know.
    Other(String),
}

impl ProblemCode {
    /// Every code this version of the protocol defines - all but
    /// [`ProblemCode::Other`].
    pub const KNOWN: [ProblemCode; 16] = [
        Self::BadRequest,
        Self::StreamNotFound,
        Self::StreamReplaced,
        Self::CursorAhead,
        Self::CursorDiverged,
        Self::EpochBehind,
        Self::TooLarge,
        Self::UnsupportedMediaType,
        Self::Unauthorized,
        Self::Forbidden,
        Self::HttpsRequired,
        Self::RateLimited,
        Self::QuotaExceeded,
        Self::NotFound,
        Self::MethodNotAllowed,
        Self::Internal,
    ];

    /// The code's spelling on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            Self::BadRequest => "bad_request",
            Self::StreamNotFound => "stream_not_found",
            Self::StreamReplaced => "stream_replaced",
            Self::CursorAhead => "cursor_ahead",
            Self::CursorDiverged => "cursor_diverged",
            Self::EpochBehind => "epoch_behind",
            Self::TooLarge => "too_large",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::HttpsRequired => "https_required",
            Self::RateLimited => "rate_limited",
            Self::QuotaExceeded => "quota_exceeded",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::Internal => "internal",
            Self::Other(code) => code,
        }
    }

    /// The code for a spelling. Never fails: an unknown spelling is
    /// [`ProblemCode::Other`].
    pub fn from_wire(code: &str) -> Self {
        match code {
            "bad_request" => Self::BadRequest,
            "stream_not_found" => Self::StreamNotFound,
            "stream_replaced" => Self::StreamReplaced,
            "cursor_ahead" => Self::CursorAhead,
            "cursor_diverged" => Self::CursorDiverged,
            "epoch_behind" => Self::EpochBehind,
            "too_large" => Self::TooLarge,
            "unsupported_media_type" => Self::UnsupportedMediaType,
            "unauthorized" => Self::Unauthorized,
            "forbidden" => Self::Forbidden,
            "https_required" => Self::HttpsRequired,
            "rate_limited" => Self::RateLimited,
            "quota_exceeded" => Self::QuotaExceeded,
            "not_found" => Self::NotFound,
            "method_not_allowed" => Self::MethodNotAllowed,
            "internal" => Self::Internal,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl fmt::Display for ProblemCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<C> Encode<C> for ProblemCode {
    fn encode<W: Write>(&self, e: &mut Encoder<W>, _: &mut C) -> Result<(), EncodeError<W::Error>> {
        e.str(self.as_str())?.ok()
    }
}

impl<'b, C> Decode<'b, C> for ProblemCode {
    fn decode(d: &mut Decoder<'b>, _: &mut C) -> Result<Self, DecodeError> {
        Ok(Self::from_wire(d.str()?))
    }
}

/// Why bytes are not the message they were read as.
#[derive(Debug)]
pub struct WireError(DecodeError);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a valid efema message: {}", self.0)
    }
}

impl std::error::Error for WireError {}

/// Encodes a message.
pub fn encode<T: Encode<()>>(message: &T) -> Vec<u8> {
    // Encoding into a vector cannot fail: the only error an encoder reports is
    // its writer's, and a vector's writer has none.
    minicbor::to_vec(message).expect("encoding into a vector is infallible")
}

/// Decodes a message, and refuses anything after it.
///
/// A body with bytes left over is not "the message plus noise" - it is a body
/// the sender did not mean to send, and the safe reading of it is none.
///
/// # Errors
///
/// [`WireError`] when the bytes are not one complete message of type `T`.
pub fn decode<'b, T: Decode<'b, ()>>(bytes: &'b [u8]) -> Result<T, WireError> {
    let mut decoder = Decoder::new(bytes);
    let message = decoder.decode().map_err(WireError)?;
    if decoder.position() != bytes.len() {
        return Err(WireError(DecodeError::message("trailing bytes after the message").at(decoder.position())));
    }
    Ok(message)
}

/// A list of blobs as a CBOR array of byte strings - what `Vec<Vec<u8>>`
/// would not be by default (an array of arrays of integers).
mod blobs {
    use minicbor::decode::{Decoder, Error as DecodeError};
    use minicbor::encode::{Encoder, Error as EncodeError, Write};

    pub fn encode<C, W: Write>(blobs: &[Vec<u8>], e: &mut Encoder<W>, _: &mut C) -> Result<(), EncodeError<W::Error>> {
        e.array(blobs.len() as u64)?;
        for blob in blobs {
            e.bytes(blob)?;
        }
        Ok(())
    }

    pub fn decode<C>(d: &mut Decoder<'_>, _: &mut C) -> Result<Vec<Vec<u8>>, DecodeError> {
        let position = d.position();
        let Some(len) = d.array()? else {
            return Err(DecodeError::message("expected a definite-length array of entries").at(position));
        };
        // The length is the sender's claim, not an allocation the receiver
        // owes it: a few bytes claiming a billion entries must not reserve
        // a billion slots. Each real entry takes at least one byte of input.
        let mut blobs = Vec::with_capacity(usize::try_from(len).unwrap_or(0).min(d.input().len()));
        for _ in 0..len {
            blobs.push(d.bytes()?.to_vec());
        }
        Ok(blobs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> StreamId {
        StreamId::from_bytes(*b"0123456789abcdef")
    }

    #[test]
    fn a_batch_round_trips() {
        let batch = Batch {
            epoch: Epoch(3),
            entries: vec![b"one".to_vec(), Vec::new(), vec![0xff; 300]],
            stream: Some(stream()),
        };
        assert_eq!(decode::<Batch>(&encode(&batch)).unwrap(), batch);
    }

    #[test]
    fn entry_data_travels_as_byte_strings() {
        // 0xa2 map(2) · 0x00 0x01 epoch 1 · 0x01 0x82 entries: array(2) ·
        // 0x42 "hi" as bytes(2) · 0x40 empty bytes
        let batch = Batch { epoch: Epoch(1), entries: vec![b"hi".to_vec(), Vec::new()], stream: None };
        assert_eq!(encode(&batch), [0xa2, 0x00, 0x01, 0x01, 0x82, 0x42, b'h', b'i', 0x40]);
    }

    #[test]
    fn a_batch_names_the_stream_it_expects_as_an_identity() {
        // {0: 1, 1: [h'78'], 2: h'30..66'}: key 2 is a 16-byte string.
        let batch = Batch { epoch: Epoch(1), entries: vec![b"x".to_vec()], stream: Some(stream()) };
        let mut expected = vec![0xa3, 0x00, 0x01, 0x01, 0x81, 0x41, b'x', 0x02, 0x50];
        expected.extend_from_slice(b"0123456789abcdef");
        assert_eq!(encode(&batch), expected);
        assert_eq!(decode::<Batch>(&expected).unwrap(), batch);
    }

    // Frozen values, computed outside Rust (Python's hashlib over the same
    // byte layout). A relay keeps the keys of the batches it wrote; a change
    // here makes every batch sent again after an upgrade land twice, so it is
    // a new protocol version, not a fix.
    #[test]
    fn the_batch_key_is_frozen() {
        let batch = |epoch, entries: &[&[u8]]| Batch {
            epoch: Epoch(epoch),
            entries: entries.iter().map(|e| e.to_vec()).collect(),
            stream: None,
        };
        let key = batch(1, &[b"hello", b""]).key();
        assert_eq!(key.to_string(), "4ba9b77af7cf7cca7343bb1a0a429ea31dab2b745ca3999908c1be168e8a98fc");
        assert_eq!(
            batch(1, &[b"hel", b"lo"]).key().to_string(),
            "2a371f288c3417da02122d1ca0e97fe8779f9da7266d959fb6ce9b71659aee76",
            "entry boundaries are part of the key"
        );
        assert_eq!(
            batch(2, &[b"hello", b""]).key().to_string(),
            "61cf1033ed28c74426786ffa3a7afaecd765381d57c7633dc7b3cbcfe86244ab",
            "the epoch is part of the key"
        );
        let named = Batch { stream: Some(stream()), ..batch(1, &[b"hello", b""]) };
        assert_eq!(named.key(), key, "the stream a batch names is not part of it");
    }

    #[test]
    fn a_repeat_is_said_only_when_it_is_one() {
        let written = Written { stream: stream(), epoch: Epoch(1), first: 1, last: 2, repeated: false };
        let plain = encode(&written);
        // {0: h'..', 1: 1, 2: 1, 3: 2}: four keys, no fifth.
        assert_eq!(plain[0], 0xa4);
        assert_eq!(decode::<Written>(&plain).unwrap(), written);
        let again = Written { repeated: true, ..written };
        let bytes = encode(&again);
        assert_eq!(bytes[0], 0xa5);
        assert_eq!(&bytes[bytes.len() - 2..], [0x04, 0xf5], "key 4, true");
        assert_eq!(decode::<Written>(&bytes).unwrap(), again);
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = encode(&Batch { epoch: Epoch(1), entries: vec![b"x".to_vec()], stream: None });
        bytes.push(0x00);
        assert!(decode::<Batch>(&bytes).is_err());
    }

    #[test]
    fn unknown_keys_are_ignored_and_missing_required_ones_are_not() {
        // {0: 1, 1: [h'78'], 9: "from a newer client"}
        let mut newer = vec![0xa3, 0x00, 0x01, 0x01, 0x81, 0x41, b'x', 0x09, 0x73];
        newer.extend_from_slice(b"from a newer client");
        assert_eq!(decode::<Batch>(&newer).unwrap().entries, vec![b"x".to_vec()]);

        // {0: 1} - no entries
        assert!(decode::<Batch>(&[0xa1, 0x00, 0x01]).is_err());
    }

    #[test]
    fn a_claimed_length_is_not_an_allocation() {
        // {0: 1, 1: array(2^32)} and then nothing: must fail, not reserve.
        let bytes = [0xa2, 0x00, 0x01, 0x01, 0x9a, 0xff, 0xff, 0xff, 0xff];
        assert!(decode::<Batch>(&bytes).is_err());
    }

    #[test]
    fn an_indefinite_array_of_entries_is_refused() {
        // {0: 1, 1: [_ h'78']}
        let bytes = [0xa2, 0x00, 0x01, 0x01, 0x9f, 0x41, b'x', 0xff];
        assert!(decode::<Batch>(&bytes).is_err());
    }

    #[test]
    fn a_page_names_the_cursor_after_its_last_entry() {
        let entry = |seq: u64| Entry { seq, epoch: Epoch(1), hash: Hash::from_bytes([seq as u8; 32]), data: vec![] };
        let page = Page { stream: stream(), epoch: Epoch(1), head: 9, entries: vec![entry(4), entry(5)] };
        assert_eq!(page.cursor(), Some(Cursor { stream: stream(), seq: 5, hash: Hash::from_bytes([5; 32]) }));
        assert_eq!(Page { entries: vec![], ..page.clone() }.cursor(), None);
        assert_eq!(decode::<Page>(&encode(&page)).unwrap(), page);
    }

    #[test]
    fn a_problem_keeps_codes_it_does_not_know() {
        let problem = Problem {
            code: ProblemCode::Other("a_newer_reason".into()),
            message: "not today".into(),
            head: None,
            epoch: None,
            limit: None,
            retry: None,
        };
        let decoded = decode::<Problem>(&encode(&problem)).unwrap();
        assert_eq!(decoded.code.as_str(), "a_newer_reason");
        assert_eq!(ProblemCode::from_wire("cursor_ahead"), ProblemCode::CursorAhead);
    }

    #[test]
    fn every_known_code_round_trips_through_its_spelling() {
        for code in ProblemCode::KNOWN {
            assert_eq!(ProblemCode::from_wire(code.as_str()), code);
        }
    }

    #[test]
    fn the_list_of_known_codes_is_complete() {
        // Adding a variant breaks this match, and the message says where else
        // to add it.
        for code in ProblemCode::KNOWN {
            match code {
                ProblemCode::BadRequest
                | ProblemCode::StreamNotFound
                | ProblemCode::StreamReplaced
                | ProblemCode::CursorAhead
                | ProblemCode::CursorDiverged
                | ProblemCode::EpochBehind
                | ProblemCode::TooLarge
                | ProblemCode::UnsupportedMediaType
                | ProblemCode::Unauthorized
                | ProblemCode::Forbidden
                | ProblemCode::HttpsRequired
                | ProblemCode::RateLimited
                | ProblemCode::QuotaExceeded
                | ProblemCode::NotFound
                | ProblemCode::MethodNotAllowed
                | ProblemCode::Internal => {}
                ProblemCode::Other(_) => panic!("Other is not a code of its own"),
            }
        }
        let distinct: std::collections::HashSet<&str> = ProblemCode::KNOWN.iter().map(ProblemCode::as_str).collect();
        assert_eq!(distinct.len(), ProblemCode::KNOWN.len(), "two codes share a spelling");
    }

    #[test]
    fn limits_round_trip_with_every_field_on_the_wire() {
        let limits = Limits { write: false, used: 12, ..Limits::DEFAULT };
        let bytes = encode(&limits);
        assert_eq!(bytes[0], 0xab, "a map of eleven keys");
        assert_eq!(decode::<Limits>(&bytes).unwrap(), limits);
    }

    #[test]
    fn a_watch_reads_with_and_without_a_cursor() {
        let cursor = Cursor::start(stream());
        let with: Watch = format!("notes:{cursor}").parse().unwrap();
        assert_eq!(with, Watch { stream: "notes".parse().unwrap(), cursor: Some(cursor) });
        assert_eq!(with.to_string().parse::<Watch>().unwrap(), with);
        assert!("Notes".parse::<Watch>().is_err());
        assert!("notes:".parse::<Watch>().is_err());
        assert!("notes:12".parse::<Watch>().is_err());
    }
}
