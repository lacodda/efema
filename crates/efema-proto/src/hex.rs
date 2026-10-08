//! Lowercase hexadecimal, the text form of identities and hashes.
//!
//! Twenty lines rather than a dependency: the alphabet is fixed, the inputs
//! are short and the only thing to get right is refusing what is not hex.

use std::fmt::{self, Write};

pub(crate) fn write(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Decodes exactly `N` bytes from lowercase hex. Uppercase is refused: one
/// spelling per value keeps cursors comparable as text.
pub(crate) fn decode<const N: usize>(text: &str) -> Option<[u8; N]> {
    let digits = text.as_bytes();
    if digits.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, [high, low]) in out.iter_mut().zip(pairs) {
        *byte = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Some(out)
}

fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let bytes = [0x00, 0x7f, 0x80, 0xff, 0x0a];
        assert_eq!(encode(&bytes), "007f80ff0a");
        assert_eq!(decode::<5>("007f80ff0a"), Some(bytes));
    }

    #[test]
    fn refuses_the_wrong_length_uppercase_and_strangers() {
        assert_eq!(decode::<2>("abc"), None);
        assert_eq!(decode::<2>("abcdef"), None);
        assert_eq!(decode::<2>("ABCD"), None);
        assert_eq!(decode::<2>("zz00"), None);
        assert_eq!(decode::<2>("+1ff"), None);
    }
}
