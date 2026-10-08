//! The hash chain that links every entry of a stream to the one before it.
//!
//! A position alone does not say *which* history it belongs to. A relay
//! restored from yesterday's backup still has a position 120 once enough new
//! entries arrive - but not the same entry 120 a reader already has. Reading
//! on from 120 would skip, silently and forever, everything between
//! yesterday's backup and the moment the relay was restored.
//!
//! So every entry carries a link: SHA-256 over the link before it and the
//! entry itself, starting from a link derived from the stream's identity. A
//! cursor holds the link at its position, and the relay refuses a cursor whose
//! link it does not have there. The chain makes a cursor name a history, not
//! a number.
//!
//! The chain guards against accidents - a restore, a bug, a disk that lied -
//! not against a dishonest relay, which can compute links as well as anyone.
//! What a relay can and cannot do is the threat model's subject, not the
//! chain's.

use sha2::{Digest, Sha256};

use crate::{Epoch, Hash, StreamId};

/// Domain separation for the first link: no other SHA-256 input in efema
/// starts with these bytes, so the genesis link of a stream cannot collide
/// with an entry's link by construction.
const GENESIS_DOMAIN: &[u8] = b"efema/chain/v1/genesis";

/// The link before the first entry of `stream`: what a cursor at position 0
/// holds.
pub fn genesis(stream: &StreamId) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(GENESIS_DOMAIN);
    hasher.update(stream.as_bytes());
    Hash::from_bytes(hasher.finalize().into())
}

/// The link of the entry at `seq`, written in `epoch` with `data`, following
/// the link `previous`.
///
/// Every field is fixed-width or length-prefixed, so no two different entries
/// feed the hash the same bytes: moving a byte from the data into the position
/// changes the input rather than reshuffling it.
pub fn link(previous: &Hash, seq: u64, epoch: Epoch, data: &[u8]) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(previous.as_bytes());
    hasher.update(seq.to_be_bytes());
    hasher.update(epoch.0.to_be_bytes());
    hasher.update((data.len() as u64).to_be_bytes());
    hasher.update(data);
    Hash::from_bytes(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_genesis_link_depends_on_the_stream() {
        let a = StreamId::from_bytes([1; 16]);
        let b = StreamId::from_bytes([2; 16]);
        assert_ne!(genesis(&a), genesis(&b));
        assert_eq!(genesis(&a), genesis(&a));
    }

    #[test]
    fn every_field_of_an_entry_moves_its_link() {
        let start = genesis(&StreamId::from_bytes([0; 16]));
        let base = link(&start, 1, Epoch(1), b"abc");
        assert_ne!(base, link(&genesis(&StreamId::from_bytes([9; 16])), 1, Epoch(1), b"abc"));
        assert_ne!(base, link(&start, 2, Epoch(1), b"abc"));
        assert_ne!(base, link(&start, 1, Epoch(2), b"abc"));
        assert_ne!(base, link(&start, 1, Epoch(1), b"abd"));
        assert_ne!(base, link(&start, 1, Epoch(1), b"ab"));
    }

    // Frozen values, computed outside Rust (Python's hashlib over the same
    // byte layout) so they check the layout rather than echo the code. The
    // chain is part of the wire format, and a change here invalidates every
    // cursor every client holds: if this test fails, the change is a new
    // protocol version, not a fix.
    #[test]
    fn the_chain_is_frozen() {
        let stream = StreamId::from_bytes(*b"0123456789abcdef");
        let start = genesis(&stream);
        assert_eq!(start.to_string(), "78e6c7beebfabda54547131dd021d12e587a91f113db763dc44cc0170ae18482");
        let first = link(&start, 1, Epoch(1), b"hello");
        assert_eq!(first.to_string(), "1338b06d59cf4930e7d0d151251285b1f1b2f824f29dff4b036d7f38f0c42a20");
    }
}
