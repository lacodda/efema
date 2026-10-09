//! What an entry is, inside and out.
//!
//! A stream holds two kinds of entries, told apart by their first byte:
//!
//! - **the stream's key, locked** - a `lacodda-seal` locked key, made by the
//!   device that created the stream and unlocked by every device that joins
//!   it with the passphrase. The first one in the stream is *the* key;
//! - **sealed entries** - a `lacodda-seal` sealed blob under that key, bound to
//!   the epoch the entry is written in. Inside: the entry format, the writing
//!   device, and the app's bytes.
//!
//! ```text
//! sealed plaintext = format: u8 (1) || device: 16 bytes || data
//! context          = "efema/entry/v1" || epoch: u32, big-endian
//! ```
//!
//! The epoch is in the context, so a relay that relabels an entry's epoch
//! makes it fail to open rather than be read as another format.

use efema_proto::Epoch;
use lacodda_seal::{Key, SEALED_OVERHEAD};

use crate::DeviceId;

/// The context a stream's key is locked for.
pub(crate) const KEY_CONTEXT: &[u8] = b"efema/stream-key/v1";
/// The domain in front of an entry's epoch in its context.
const ENTRY_DOMAIN: &[u8] = b"efema/entry/v1";
/// The entry format this release writes and the newest it reads.
const FORMAT: u8 = 1;
/// The plaintext in front of the app's bytes: format and device.
const INNER_HEADER: usize = 1 + 16;

/// How many bytes sealing adds to an item: the sealed blob's own and the
/// entry's header inside it.
pub const ENTRY_OVERHEAD: usize = SEALED_OVERHEAD + INNER_HEADER;

fn context(epoch: Epoch) -> Vec<u8> {
    [ENTRY_DOMAIN, &epoch.0.to_be_bytes()].concat()
}

/// Seals `data` as an entry written by `device` in `epoch`.
pub(crate) fn seal(key: &Key, epoch: Epoch, device: DeviceId, data: &[u8]) -> Result<Vec<u8>, lacodda_seal::Error> {
    let mut plaintext = Vec::with_capacity(INNER_HEADER + data.len());
    plaintext.push(FORMAT);
    plaintext.extend_from_slice(device.as_bytes());
    plaintext.extend_from_slice(data);
    key.seal(&context(epoch), &plaintext)
}

/// What a sealed entry holds.
pub(crate) enum Opened {
    /// The device that wrote it, and the app's bytes.
    Entry { device: DeviceId, data: Vec<u8> },
    /// An entry format newer than this release reads.
    Newer(u8),
}

/// Opens a sealed entry written in `epoch`.
pub(crate) fn open(key: &Key, epoch: Epoch, sealed: &[u8]) -> Result<Opened, lacodda_seal::Error> {
    let plaintext = key.open(&context(epoch), sealed)?;
    match plaintext.first() {
        Some(&FORMAT) if plaintext.len() >= INNER_HEADER => {
            let device = DeviceId::from_bytes(plaintext[1..INNER_HEADER].try_into().expect("sixteen bytes"));
            Ok(Opened::Entry { device, data: plaintext[INNER_HEADER..].to_vec() })
        }
        Some(&version) if version > FORMAT => Ok(Opened::Newer(version)),
        // A sealed entry that opens under the stream's key was written by a
        // device holding that key; one that opens but makes no sense is a
        // defect in that device, and reads as tampering would.
        _ => Err(lacodda_seal::Error::Inauthentic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_opens_in_its_epoch_only() {
        let key = Key::from_bytes([1; 32]);
        let device = DeviceId::from_bytes([2; 16]);
        let sealed = seal(&key, Epoch(3), device, b"data").unwrap();
        assert_eq!(sealed.len(), b"data".len() + ENTRY_OVERHEAD);
        match open(&key, Epoch(3), &sealed).unwrap() {
            Opened::Entry { device: from, data } => assert_eq!((from, data.as_slice()), (device, &b"data"[..])),
            Opened::Newer(_) => panic!("written in this format"),
        }
        assert!(open(&key, Epoch(4), &sealed).is_err(), "an epoch relabelled on the relay must not open");
    }

    #[test]
    fn a_newer_format_is_named() {
        let key = Key::from_bytes([1; 32]);
        let mut plaintext = vec![FORMAT + 1];
        plaintext.extend_from_slice(&[0; 16]);
        let sealed = key.seal(&context(Epoch(1)), &plaintext).unwrap();
        assert!(matches!(open(&key, Epoch(1), &sealed), Ok(Opened::Newer(v)) if v == FORMAT + 1));
        let sealed = key.seal(&context(Epoch(1)), &[]).unwrap();
        assert!(open(&key, Epoch(1), &sealed).is_err(), "an entry with no header");
    }
}
