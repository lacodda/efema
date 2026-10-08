//! The name a stream is addressed by.

use std::fmt;
use std::str::FromStr;

use minicbor::decode::{Decoder, Error as DecodeError};
use minicbor::encode::{Encoder, Error as EncodeError, Write};
use minicbor::{Decode, Encode};

/// The longest name a stream may have, in bytes.
pub const MAX_NAME_LEN: usize = 64;

/// The name of a stream: what goes into the URL, and what a person reads in
/// the relay's listing.
///
/// One to [`MAX_NAME_LEN`] characters, lowercase ASCII letters, digits, `-`,
/// `_` and `.`, starting with a letter or a digit. Narrow on purpose: a name
/// travels in a URL path, in a log line and in a file listing, and each of
/// those has its own idea of what is special. Widening the set later breaks
/// nobody; narrowing it would.
///
/// The relay sees names - they are not sealed. A name that says more than it
/// should (`bank-passwords`) says it to whoever runs the relay.
///
/// ```
/// use efema_proto::StreamName;
///
/// assert!("notes".parse::<StreamName>().is_ok());
/// assert!("vault.2026-10".parse::<StreamName>().is_ok());
/// assert!("Notes".parse::<StreamName>().is_err());
/// assert!("-notes".parse::<StreamName>().is_err());
/// assert!("a/b".parse::<StreamName>().is_err());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamName(String);

/// Why a text is not a stream name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvalidName {
    /// The name is empty.
    Empty,
    /// The name is longer than [`MAX_NAME_LEN`] bytes.
    TooLong(usize),
    /// The name starts with something other than a letter or a digit.
    BadStart(char),
    /// The name contains a character outside the allowed set.
    BadCharacter(char),
}

impl fmt::Display for InvalidName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a stream name cannot be empty"),
            Self::TooLong(len) => {
                write!(f, "a stream name is at most {MAX_NAME_LEN} characters, this one is {len}")
            }
            Self::BadStart(c) => write!(f, "a stream name starts with a letter or a digit, not {c:?}"),
            Self::BadCharacter(c) => write!(
                f,
                "a stream name is made of lowercase letters, digits, '-', '_' and '.', and {c:?} is none of them"
            ),
        }
    }
}

impl std::error::Error for InvalidName {}

impl StreamName {
    /// Checks `name` and wraps it.
    ///
    /// # Errors
    ///
    /// The first reason `name` is not a valid stream name.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidName> {
        let name = name.into();
        validate(&name)?;
        Ok(Self(name))
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate(name: &str) -> Result<(), InvalidName> {
    let mut chars = name.chars();
    let first = chars.next().ok_or(InvalidName::Empty)?;
    if name.len() > MAX_NAME_LEN {
        return Err(InvalidName::TooLong(name.len()));
    }
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(InvalidName::BadStart(first));
    }
    match chars.find(|&c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))) {
        Some(c) => Err(InvalidName::BadCharacter(c)),
        None => Ok(()),
    }
}

impl FromStr for StreamName {
    type Err = InvalidName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl fmt::Display for StreamName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for StreamName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<C> Encode<C> for StreamName {
    fn encode<W: Write>(&self, e: &mut Encoder<W>, _: &mut C) -> Result<(), EncodeError<W::Error>> {
        e.str(&self.0)?.ok()
    }
}

// Decoding checks the name as well: a name that would be refused in a URL is
// refused in a body too, so no code path holds one it could not have parsed.
impl<'b, C> Decode<'b, C> for StreamName {
    fn decode(d: &mut Decoder<'b>, _: &mut C) -> Result<Self, DecodeError> {
        let position = d.position();
        let text = d.str()?;
        Self::new(text).map_err(|e| DecodeError::message(e.to_string()).at(position))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_documented_alphabet() {
        for name in ["a", "0", "notes", "vault-2026", "a_b", "x.y.z", "9lives", &"a".repeat(MAX_NAME_LEN)] {
            assert!(StreamName::new(name).is_ok(), "{name:?} should be a valid name");
        }
    }

    #[test]
    fn refuses_with_the_first_reason() {
        assert_eq!(StreamName::new(""), Err(InvalidName::Empty));
        assert_eq!(StreamName::new("a".repeat(MAX_NAME_LEN + 1)), Err(InvalidName::TooLong(MAX_NAME_LEN + 1)));
        assert_eq!(StreamName::new("-a"), Err(InvalidName::BadStart('-')));
        assert_eq!(StreamName::new(".a"), Err(InvalidName::BadStart('.')));
        assert_eq!(StreamName::new("Notes"), Err(InvalidName::BadStart('N')));
        assert_eq!(StreamName::new("no tes"), Err(InvalidName::BadCharacter(' ')));
        assert_eq!(StreamName::new("a/b"), Err(InvalidName::BadCharacter('/')));
        assert_eq!(StreamName::new("a:b"), Err(InvalidName::BadCharacter(':')));
        assert_eq!(StreamName::new("заметки"), Err(InvalidName::BadStart('з')));
        assert_eq!(StreamName::new("aé"), Err(InvalidName::BadCharacter('é')));
    }

    #[test]
    fn decoding_refuses_what_parsing_refuses() {
        let encoded = minicbor::to_vec("Bad Name").unwrap();
        assert!(minicbor::decode::<StreamName>(&encoded).is_err());
        let encoded = minicbor::to_vec("good-name").unwrap();
        assert_eq!(minicbor::decode::<StreamName>(&encoded).unwrap().as_str(), "good-name");
    }
}
