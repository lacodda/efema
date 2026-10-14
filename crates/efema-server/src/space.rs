//! Spaces, and what an access key may do in one.

use std::fmt;
use std::str::FromStr;

use efema_proto::{InvalidName, MAX_NAME_LEN, StreamName};

/// The name of a space: a set of streams, and the access keys that open it.
///
/// The relay's operator names spaces; devices never see the name - an access
/// key opens its space, and a device addresses streams within it. The
/// alphabet is a stream name's: lowercase ASCII letters, digits, `-`, `_` and
/// `.`, starting with a letter or a digit, at most 64 characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpaceName(String);

/// Why a text is not a space name.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidSpace(String);

impl SpaceName {
    /// Checks `name` and wraps it.
    ///
    /// # Errors
    ///
    /// The first reason `name` is not a valid space name.
    pub fn new(name: &str) -> Result<Self, InvalidSpace> {
        // The same rules as a stream name, said about a space.
        StreamName::new(name).map(|_| Self(name.to_owned())).map_err(|e| {
            InvalidSpace(match e {
                InvalidName::Empty => "a space name cannot be empty".into(),
                InvalidName::TooLong(len) => {
                    format!("a space name is at most {MAX_NAME_LEN} characters, this one is {len}")
                }
                InvalidName::BadStart(c) => format!("a space name starts with a letter or a digit, not {c:?}"),
                InvalidName::BadCharacter(c) => format!(
                    "a space name is made of lowercase letters, digits, '-', '_' and '.', and {c:?} is none of them"
                ),
            })
        })
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SpaceName {
    type Err = InvalidSpace;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl fmt::Display for SpaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What an access key may do in its space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// Read the space's streams and wait on them.
    Read,
    /// Read, and write: append batches, which creates a stream the first
    /// time. A device that writes has to read too - it finds the stream's
    /// key there - so there is no key that only writes.
    Write,
}

impl Access {
    /// The access as the relay's database and its listing spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    /// Whether a key with this access may write.
    pub fn writes(self) -> bool {
        self == Self::Write
    }
}

impl FromStr for Access {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            other => Err(format!("access is `read` or `write`, not `{other}`")),
        }
    }
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_is_named_like_a_stream_and_said_to_be_a_space() {
        assert_eq!(SpaceName::new("alice").unwrap().as_str(), "alice");
        let error = SpaceName::new("Alice").unwrap_err().to_string();
        assert!(error.starts_with("a space name"), "{error}");
        assert!(SpaceName::new("a/b").is_err());
        assert!(SpaceName::new("").is_err());
    }

    #[test]
    fn access_is_spelled_one_way() {
        for access in [Access::Read, Access::Write] {
            assert_eq!(access.as_str().parse::<Access>(), Ok(access));
        }
        assert!("Write".parse::<Access>().is_err());
        assert!(Access::Write.writes() && !Access::Read.writes());
    }
}
