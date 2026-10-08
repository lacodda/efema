//! Where a reader stands in a stream.

use std::fmt;
use std::str::FromStr;

use minicbor::{Decode, Encode};

use crate::{Hash, StreamId, chain};

/// A reader's place in a stream: which stream, how far, and what the history
/// up to there was.
///
/// The position is the last entry the reader has; reading "after" the cursor
/// returns the entries that follow it. The identity and the link make the
/// cursor checkable: the relay answers a cursor only if the stream is the same
/// incarnation and its chain has the same link at that position. Otherwise it
/// says which of the two failed, and the reader knows its copy and the relay's
/// have parted - rather than reading on from a place that no longer exists.
///
/// The text form, used in URLs, is `<position>.<stream>.<link>`:
///
/// ```
/// use efema_proto::{Cursor, StreamId};
///
/// let start = Cursor::start(StreamId::from_bytes([0xab; 16]));
/// let text = start.to_string();
/// assert!(text.starts_with("0.abababab"));
/// assert_eq!(text.parse::<Cursor>().unwrap(), start);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[cbor(map)]
pub struct Cursor {
    /// The stream incarnation this cursor belongs to.
    #[n(0)]
    pub stream: StreamId,
    /// The position of the last entry the reader has; 0 before the first.
    #[n(1)]
    pub seq: u64,
    /// The chain link at `seq`.
    #[n(2)]
    pub hash: Hash,
}

impl Cursor {
    /// The cursor before the first entry of `stream`: the reader knows the
    /// stream and has read nothing from it yet.
    pub fn start(stream: StreamId) -> Self {
        Self { stream, seq: 0, hash: chain::genesis(&stream) }
    }
}

/// Why a text is not a cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseCursorError;

impl fmt::Display for ParseCursorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a cursor is <position>.<stream>.<link>: a number, 32 and 64 lowercase hexadecimal digits")
    }
}

impl std::error::Error for ParseCursorError {}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.seq, self.stream, self.hash)
    }
}

impl FromStr for Cursor {
    type Err = ParseCursorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('.');
        let (Some(seq), Some(stream), Some(hash), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(ParseCursorError);
        };
        // Digits only: `u64::from_str` would also take a leading `+`, and a
        // second spelling of the same cursor is one more thing to compare.
        if seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) || (seq.len() > 1 && seq.starts_with('0')) {
            return Err(ParseCursorError);
        }
        Ok(Self {
            seq: seq.parse().map_err(|_| ParseCursorError)?,
            stream: stream.parse().map_err(|_| ParseCursorError)?,
            hash: hash.parse().map_err(|_| ParseCursorError)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Cursor {
        Cursor { stream: StreamId::from_bytes([0xab; 16]), seq: 42, hash: Hash::from_bytes([0xcd; 32]) }
    }

    #[test]
    fn round_trips_through_text() {
        let cursor = sample();
        let text = cursor.to_string();
        assert_eq!(text, format!("42.{}.{}", "ab".repeat(16), "cd".repeat(32)));
        assert_eq!(text.parse::<Cursor>(), Ok(cursor));
    }

    #[test]
    fn has_one_spelling() {
        let canonical = sample().to_string();
        for wrong in [
            canonical.replacen("42", "+42", 1),
            canonical.replacen("42", "042", 1),
            canonical.replacen("42", "", 1),
            canonical.to_uppercase(),
            format!("{canonical}."),
            format!("{canonical}.0"),
            canonical.replacen('.', ":", 1),
            "18446744073709551616".to_string() + &canonical[2..],
        ] {
            assert_eq!(wrong.parse::<Cursor>(), Err(ParseCursorError), "{wrong:?} should not parse");
        }
    }

    #[test]
    fn the_start_holds_the_genesis_link() {
        let stream = StreamId::from_bytes([5; 16]);
        let start = Cursor::start(stream);
        assert_eq!(start.seq, 0);
        assert_eq!(start.hash, chain::genesis(&stream));
    }

    #[test]
    fn round_trips_through_cbor() {
        let cursor = sample();
        let bytes = minicbor::to_vec(cursor).unwrap();
        assert_eq!(minicbor::decode::<Cursor>(&bytes).unwrap(), cursor);
    }
}
