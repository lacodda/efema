//! Identities and numbers that travel on the wire: which incarnation of a
//! stream, which history, which format.

use std::fmt;
use std::str::FromStr;

use minicbor::decode::{Decoder, Error as DecodeError};
use minicbor::encode::{Encoder, Error as EncodeError, Write};
use minicbor::{Decode, Encode};

use crate::hex;

/// Why a text is not an identity or a hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseIdError {
    what: &'static str,
    digits: usize,
}

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a {} is {} lowercase hexadecimal digits", self.what, self.digits)
    }
}

impl std::error::Error for ParseIdError {}

/// Which incarnation of a stream name this is.
///
/// The relay draws sixteen random bytes when a stream is created and never
/// changes them. A name can outlive its stream - a relay's data wiped, a stream
/// deleted and written again - and a position means nothing across that gap:
/// position 40 of the old stream and position 40 of the new one are unrelated
/// entries. The identity is what lets a reader tell the two apart, instead of
/// reading on from a cursor that points into a history that no longer exists.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId([u8; 16]);

impl StreamId {
    /// Wraps sixteen bytes. The relay draws them at random; anything else is
    /// for tests.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The sixteen bytes.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        hex::write(&self.0, f)
    }
}

impl fmt::Debug for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StreamId({self})")
    }
}

impl FromStr for StreamId {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        hex::decode::<16>(s).map(Self).ok_or(ParseIdError { what: "stream identity", digits: 32 })
    }
}

/// A link in a stream's hash chain: SHA-256 over the previous link and the
/// entry at this position (see [`crate::chain`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash([u8; 32]);

impl Hash {
    /// Wraps thirty-two bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The thirty-two bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The first eight hex digits - enough to tell hashes apart in a listing,
    /// never enough to compare them.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..4])
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        hex::write(&self.0, f)
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({self})")
    }
}

impl FromStr for Hash {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        hex::decode::<32>(s).map(Self).ok_or(ParseIdError { what: "hash", digits: 64 })
    }
}

/// The version of the format an app writes into a stream.
///
/// The relay does not know what an epoch means - for it an epoch is a number
/// that only goes up. An app raises it when it changes the format of its own
/// batches; from then on the relay refuses writes from anyone still on the
/// older epoch, and every entry says which epoch it was written in, so a reader
/// that does not know a newer format can stop before it instead of
/// misreading it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
#[cbor(transparent)]
pub struct Epoch(pub u32);

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// Identities and hashes travel as CBOR byte strings of exactly their length.
// A shorter or longer one is an error at the decoder, not a value padded or
// cut to fit.
macro_rules! fixed_bytes {
    ($type:ident, $len:literal) => {
        impl<C> Encode<C> for $type {
            fn encode<W: Write>(&self, e: &mut Encoder<W>, _: &mut C) -> Result<(), EncodeError<W::Error>> {
                e.bytes(&self.0)?.ok()
            }
        }

        impl<'b, C> Decode<'b, C> for $type {
            fn decode(d: &mut Decoder<'b>, _: &mut C) -> Result<Self, DecodeError> {
                let position = d.position();
                let bytes = d.bytes()?;
                <[u8; $len]>::try_from(bytes).map(Self).map_err(|_| {
                    DecodeError::message(concat!("expected exactly ", stringify!($len), " bytes")).at(position)
                })
            }
        }
    };
}

fixed_bytes!(StreamId, 16);
fixed_bytes!(Hash, 32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_and_hashes_round_trip_through_text() {
        let id = StreamId::from_bytes([0xab; 16]);
        assert_eq!(id.to_string(), "ab".repeat(16));
        assert_eq!(id.to_string().parse::<StreamId>(), Ok(id));

        let hash = Hash::from_bytes([0x01; 32]);
        assert_eq!(hash.to_string().parse::<Hash>(), Ok(hash));
        assert_eq!(hash.short(), "01010101");
    }

    #[test]
    fn a_byte_string_of_the_wrong_length_is_refused() {
        let bytes = |len: usize| {
            let mut e = Encoder::new(Vec::new());
            e.bytes(&vec![0u8; len]).unwrap();
            e.into_writer()
        };
        assert!(minicbor::decode::<StreamId>(&bytes(15)).is_err());
        assert!(minicbor::decode::<StreamId>(&bytes(16)).is_ok());
        assert!(minicbor::decode::<Hash>(&bytes(33)).is_err());
        assert!(minicbor::decode::<Hash>(&bytes(32)).is_ok());
    }

    #[test]
    fn an_array_of_numbers_is_not_a_byte_string() {
        // What a careless encoder makes of `[u8; 16]`: an array of sixteen
        // integers. It must not decode as an identity.
        let array = minicbor::to_vec([0u8; 16]).unwrap();
        assert!(minicbor::decode::<StreamId>(&array).is_err());
    }
}
