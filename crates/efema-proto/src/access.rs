//! The key a device shows a relay to reach the streams of a space.

use std::fmt;
use std::str::FromStr;

use sha2::{Digest, Sha256};

use crate::hex;

/// What every access key starts with: it says what the string is to a person
/// and to a secret scanner, and lets a key be told from a passphrase at a
/// glance.
pub const ACCESS_KEY_PREFIX: &str = "efema_";

/// Domain separation for a key's digest: no other SHA-256 input in efema
/// starts with these bytes.
const ACCESS_DOMAIN: &[u8] = b"efema/access/v1";

/// An access key: 32 random bytes the relay's operator handed out, which open
/// one space of the relay - to read its streams, or to read and write them.
///
/// The text form is [`ACCESS_KEY_PREFIX`] and 64 lowercase hexadecimal
/// digits; it travels as `Authorization: Bearer <key>`. A relay keeps only
/// the key's [`digest`](AccessKey::digest), so its database gives no keys
/// away. An access key is not the stream's key: it decides who may reach the
/// sealed entries, not who can open them.
///
/// There is no `Display`: printing a key is spelled
/// [`expose`](AccessKey::expose), so it does not end up in a log by way of a
/// format string. `Debug` shows only the [`id`](AccessKey::id).
///
/// ```
/// use efema_proto::AccessKey;
///
/// let text = format!("efema_{}", "ab".repeat(32));
/// let key: AccessKey = text.parse().unwrap();
/// assert_eq!(key.expose(), text);
/// assert!(!format!("{key:?}").contains(&"ab".repeat(32)));
/// assert!("efema_ABCD".parse::<AccessKey>().is_err());
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct AccessKey([u8; 32]);

/// Why a text is not an access key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseAccessKeyError;

impl fmt::Display for ParseAccessKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "an access key is `{ACCESS_KEY_PREFIX}` and 64 lowercase hexadecimal digits")
    }
}

impl std::error::Error for ParseAccessKeyError {}

impl AccessKey {
    /// Wraps 32 bytes. A relay draws them at random; anything else is for
    /// tests.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The key as text, to hand to a person once or to send to the relay.
    pub fn expose(&self) -> String {
        format!("{ACCESS_KEY_PREFIX}{}", hex::encode(&self.0))
    }

    /// What a relay keeps instead of the key: SHA-256 over a domain and the
    /// key's bytes. One way: the digest does not give the key back, and a
    /// key of 32 random bytes is not found by trying.
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(ACCESS_DOMAIN);
        hasher.update(self.0);
        hasher.finalize().into()
    }

    /// How a relay names the key in a listing and a log: the first eight
    /// hexadecimal digits of its digest - enough to tell keys apart, nothing
    /// of the key itself.
    pub fn id(&self) -> String {
        access_id(&self.digest())
    }
}

/// The name of the key with this digest, as [`AccessKey::id`] gives it.
pub fn access_id(digest: &[u8; 32]) -> String {
    hex::encode(&digest[..4])
}

impl FromStr for AccessKey {
    type Err = ParseAccessKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digits = s.strip_prefix(ACCESS_KEY_PREFIX).ok_or(ParseAccessKeyError)?;
        hex::decode::<32>(digits).map(Self).ok_or(ParseAccessKeyError)
    }
}

impl fmt::Debug for AccessKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AccessKey({})", self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_form_round_trips_and_nothing_else_parses() {
        let key = AccessKey::from_bytes([0x5a; 32]);
        assert_eq!(key.expose().parse::<AccessKey>(), Ok(key.clone()));
        assert_eq!(key.expose().len(), ACCESS_KEY_PREFIX.len() + 64);
        let digits = "5a".repeat(32);
        for text in [
            digits.clone(),
            format!("EFEMA_{digits}"),
            format!("efema_{}", "5A".repeat(32)),
            format!("efema_{digits}0"),
        ] {
            assert_eq!(text.parse::<AccessKey>(), Err(ParseAccessKeyError), "{text}");
        }
    }

    // Frozen, computed outside Rust (Python's hashlib): a relay keeps the
    // digests of the keys it handed out, and a change here locks every
    // device out after an upgrade.
    #[test]
    fn the_digest_is_frozen() {
        let key = AccessKey::from_bytes([0x5a; 32]);
        assert_eq!(hex::encode(&key.digest()), "fc0ce5c50ac84804106aaa3e65af08e5f682c6659cb3bf046584068d868ed3d5");
        assert_eq!(key.id(), "fc0ce5c5");
    }

    #[test]
    fn debug_names_the_key_without_showing_it() {
        let key = AccessKey::from_bytes([0x5a; 32]);
        let shown = format!("{key:?}");
        assert!(!shown.contains("5a5a"), "{shown}");
        assert!(shown.contains(&key.id()), "{shown}");
    }
}
