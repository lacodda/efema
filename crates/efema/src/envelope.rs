//! What an entry is, inside and out.
//!
//! A stream holds two kinds of entries, told apart by their first byte:
//!
//! - **key entries** - the stream's key, locked under a passphrase (a
//!   `lacodda-seal` locked key), followed by the key's own seal over the lock
//!   and its generation. The first key entry in a stream is *the* key; each
//!   later one of the same key locks it under a new passphrase;
//! - **sealed entries** - a `lacodda-seal` sealed blob under that key, bound to
//!   the epoch the entry is written in. Inside: the entry format, the writing
//!   device, how the item is compressed, and the item.
//!
//! ```text
//! key entry        = lock || seal(key, "efema/key-entry/v1" || lock, generation: u32, big-endian)
//!
//! sealed plaintext = format: u8 (2) || device: 16 bytes || codec: u8 || body
//! context          = "efema/entry/v1" || epoch: u32, big-endian
//! codec 0          = the body is the item
//! codec 1          = the body is one zstd frame of the item, its size in the frame header
//! ```
//!
//! The epoch is in the context, so a relay that relabels an entry's epoch
//! makes it fail to open rather than be read as another format.

use efema_proto::{Epoch, limits};
use lacodda_seal::{Key, LockedKey, SEALED_OVERHEAD};

use crate::{DeviceId, Part};

/// The context a stream's key is locked for.
pub(crate) const KEY_CONTEXT: &[u8] = b"efema/stream-key/v1";
/// The domain in front of a lock in the context of its key entry's seal.
const KEY_ENTRY_DOMAIN: &[u8] = b"efema/key-entry/v1";
/// The domain in front of an entry's epoch in its context.
const ENTRY_DOMAIN: &[u8] = b"efema/entry/v1";
/// The entry format this release writes, and the only one it reads. Format 1
/// was efema 0.2's, before the codec byte; its number stays taken, and an
/// entry in it does not open.
const FORMAT: u8 = 2;
/// The item as it is.
const CODEC_NONE: u8 = 0;
/// The item as one zstd frame.
const CODEC_ZSTD: u8 = 1;
/// zstd's own default: most of the gain, at a few hundred megabytes a second.
const ZSTD_LEVEL: i32 = 3;
/// The plaintext in front of the body: format, device and codec.
const INNER_HEADER: usize = 1 + 16 + 1;
/// The longest item a compressed body may expand to: what one request of the
/// protocol carries, which no item a client writes is longer than. A frame
/// that claims more is refused before anything is allocated for it.
const MAX_EXPANDED: usize = limits::MAX_BODY_BYTES;
/// The key's seal over a lock: a sealed blob of the four-byte generation.
const KEY_SEAL_LEN: usize = SEALED_OVERHEAD + 4;

/// How many bytes sealing adds to an item it does not compress: the sealed
/// blob's own and the entry's header inside it.
pub const ENTRY_OVERHEAD: usize = SEALED_OVERHEAD + INNER_HEADER;

/// Whether a client compresses items before it seals them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Compression {
    /// zstd, for every item it makes shorter; an item it does not is sealed
    /// as it is. Text - notes, JSON, markdown - usually shrinks to a third.
    #[default]
    Zstd,
    /// Never. Compression makes an entry's length depend on what is in it,
    /// and the relay sees lengths: where an item mixes a secret with text
    /// someone else chose, that is a known way to learn the secret (see the
    /// threat model). Such an app turns compression off.
    Off,
}

fn context(epoch: Epoch) -> Vec<u8> {
    [ENTRY_DOMAIN, &epoch.0.to_be_bytes()].concat()
}

/// Seals `data` as an entry written by `device` in `epoch`.
pub(crate) fn seal(
    key: &Key,
    epoch: Epoch,
    device: DeviceId,
    data: &[u8],
    compression: Compression,
) -> Result<Vec<u8>, lacodda_seal::Error> {
    let compressed = match compression {
        Compression::Zstd => zstd::bulk::compress(data, ZSTD_LEVEL).ok().filter(|frame| frame.len() < data.len()),
        Compression::Off => None,
    };
    let (codec, body) = match &compressed {
        Some(frame) => (CODEC_ZSTD, frame.as_slice()),
        None => (CODEC_NONE, data),
    };
    let mut plaintext = Vec::with_capacity(INNER_HEADER + body.len());
    plaintext.push(FORMAT);
    plaintext.extend_from_slice(device.as_bytes());
    plaintext.push(codec);
    plaintext.extend_from_slice(body);
    key.seal(&context(epoch), &plaintext)
}

/// What a sealed entry holds.
pub(crate) enum Opened {
    /// The device that wrote it, and the item.
    Entry { device: DeviceId, data: Vec<u8> },
    /// A version of a part - the entry's format, or its codec - newer than
    /// this release reads.
    Newer(Part, u8),
}

/// Opens a sealed entry written in `epoch`.
pub(crate) fn open(key: &Key, epoch: Epoch, sealed: &[u8]) -> Result<Opened, lacodda_seal::Error> {
    let plaintext = key.open(&context(epoch), sealed)?;
    // A sealed entry that opens under the stream's key was written by a
    // device holding that key; one that opens but makes no sense is a defect
    // in that device, and reads as tampering would.
    let nonsense = lacodda_seal::Error::Inauthentic;
    let device = |bytes: &[u8]| DeviceId::from_bytes(bytes[1..17].try_into().expect("sixteen bytes"));
    match plaintext.first() {
        Some(&FORMAT) if plaintext.len() >= INNER_HEADER => {
            let body = &plaintext[INNER_HEADER..];
            let data = match plaintext[INNER_HEADER - 1] {
                CODEC_NONE => body.to_vec(),
                CODEC_ZSTD => expand(body).ok_or(nonsense)?,
                codec => return Ok(Opened::Newer(Part::Codec, codec)),
            };
            Ok(Opened::Entry { device: device(&plaintext), data })
        }
        Some(&version) if version > FORMAT => Ok(Opened::Newer(Part::Entry, version)),
        _ => Err(nonsense),
    }
}

/// The item in a zstd frame, if the frame names its size, the size is within
/// bounds, and the frame holds exactly that much.
fn expand(frame: &[u8]) -> Option<Vec<u8>> {
    let size = zstd::zstd_safe::get_frame_content_size(frame).ok().flatten()?;
    let size = usize::try_from(size).ok().filter(|size| *size <= MAX_EXPANDED)?;
    // The capacity is the frame's own claim: a frame - or a second one behind
    // it - that expands to more fails rather than grows. One that holds less
    // than it claims, zstd refuses on its own against the header.
    zstd::bulk::decompress(frame, size).ok()
}

/// A key entry: a lock of the stream's key, and its generation - 0 for the
/// lock that created the stream, one more for each change of passphrase.
#[derive(Clone, Debug)]
pub(crate) struct KeyEntry {
    pub(crate) lock: LockedKey,
    seal: Vec<u8>,
}

impl KeyEntry {
    /// Writes `lock` of `key` as the key entry of `generation`.
    pub(crate) fn new(key: &Key, lock: LockedKey, generation: u32) -> Result<Self, lacodda_seal::Error> {
        let seal = key.seal(&key_entry_context(&lock.to_bytes()), &generation.to_be_bytes())?;
        Ok(Self { lock, seal })
    }

    /// Reads a key entry; its seal is checked by [`KeyEntry::generation`],
    /// with the key.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, Unread> {
        let at = bytes.len().checked_sub(KEY_SEAL_LEN).ok_or(Unread::Malformed(lacodda_seal::Error::Malformed {
            expected: "key entry",
            reason: "shorter than the key's seal",
        }))?;
        let (lock, seal) = bytes.split_at(at);
        let lock = LockedKey::from_bytes(lock).map_err(|e| match e {
            lacodda_seal::Error::NewerFormat { found, .. } => Unread::Newer(Part::Lock, found),
            e => Unread::Malformed(e),
        })?;
        // Any other fault in the seal is the key's to find: a seal it does
        // not open was not written by a device holding it.
        if let Err(lacodda_seal::Error::NewerFormat { found, .. }) = lacodda_seal::sealed_key_id(seal) {
            return Err(Unread::Newer(Part::Seal, found));
        }
        Ok(Self { lock, seal: seal.to_vec() })
    }

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        [self.lock.to_bytes(), self.seal.clone()].concat()
    }

    /// The entry's generation, as sealed by `key`; `None` when the seal is
    /// not the key's over this lock - the entry was not written by a device
    /// holding the key.
    pub(crate) fn generation(&self, key: &Key) -> Option<u32> {
        let generation = key.open(&key_entry_context(&self.lock.to_bytes()), &self.seal).ok()?;
        Some(u32::from_be_bytes(generation.as_slice().try_into().ok()?))
    }
}

/// Why bytes that start as a key entry are not one this release reads.
#[derive(Debug)]
pub(crate) enum Unread {
    /// A part of it - the lock, or the key's seal - is in a version newer
    /// than this release reads.
    Newer(Part, u8),
    /// It is not a key entry.
    Malformed(lacodda_seal::Error),
}

fn key_entry_context(lock: &[u8]) -> Vec<u8> {
    [KEY_ENTRY_DOMAIN, lock].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: DeviceId = DeviceId::from_bytes([2; 16]);

    fn opened(key: &Key, epoch: Epoch, sealed: &[u8]) -> (DeviceId, Vec<u8>) {
        match open(key, epoch, sealed) {
            Ok(Opened::Entry { device, data }) => (device, data),
            Ok(Opened::Newer(part, v)) => panic!("written in this format, read as {part} {v}"),
            Err(e) => panic!("does not open: {e}"),
        }
    }

    /// Seals a plaintext laid out by hand.
    fn sealed_as(key: &Key, epoch: Epoch, head: &[u8], body: &[u8]) -> Vec<u8> {
        key.seal(&context(epoch), &[head, DEVICE.as_bytes(), body].concat()).unwrap()
    }

    #[test]
    fn an_entry_opens_in_its_epoch_only() {
        let key = Key::from_bytes([1; 32]);
        let sealed = seal(&key, Epoch(3), DEVICE, b"data", Compression::Zstd).unwrap();
        assert_eq!(sealed.len(), b"data".len() + ENTRY_OVERHEAD, "four bytes do not compress");
        assert_eq!(opened(&key, Epoch(3), &sealed), (DEVICE, b"data".to_vec()));
        assert!(open(&key, Epoch(4), &sealed).is_err(), "an epoch relabelled on the relay must not open");
    }

    #[test]
    fn what_compresses_is_sealed_compressed_and_comes_back_whole() {
        let key = Key::from_bytes([1; 32]);
        let text = "# Notes\n\nA line of markdown that repeats itself.\n".repeat(200).into_bytes();
        let sealed = seal(&key, Epoch(1), DEVICE, &text, Compression::Zstd).unwrap();
        assert!(sealed.len() < text.len() / 3, "{} bytes of text sealed into {}", text.len(), sealed.len());
        assert_eq!(opened(&key, Epoch(1), &sealed), (DEVICE, text.clone()));

        let off = seal(&key, Epoch(1), DEVICE, &text, Compression::Off).unwrap();
        assert_eq!(off.len(), text.len() + ENTRY_OVERHEAD, "compression off still compressed");
        assert_eq!(opened(&key, Epoch(1), &off), (DEVICE, text));
    }

    #[test]
    fn what_does_not_compress_is_sealed_as_it_is() {
        let key = Key::from_bytes([1; 32]);
        let mut random = vec![0; 256];
        getrandom::fill(&mut random).unwrap();
        let sealed = seal(&key, Epoch(1), DEVICE, &random, Compression::Zstd).unwrap();
        assert_eq!(sealed.len(), random.len() + ENTRY_OVERHEAD);
        assert_eq!(opened(&key, Epoch(1), &sealed).1, random);
    }

    // zstd frames written by hand from RFC 8878, not by the library. A frame
    // is the magic number, a header descriptor, the content size, then
    // blocks: a three-byte little-endian header (last-block bit, type, size)
    // and the block's bytes. Raw blocks carry the bytes; RLE blocks one byte,
    // repeated `size` times.
    const MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

    /// A frame with a one-byte content size (descriptor 0x20: single
    /// segment) and the given blocks.
    fn small_frame(size: u8, blocks: &[u8]) -> Vec<u8> {
        [&MAGIC[..], &[0x20, size], blocks].concat()
    }

    fn block_header(last: bool, kind: u32, size: u32) -> [u8; 3] {
        let value = u32::from(last) | kind << 1 | size << 3;
        let bytes = value.to_le_bytes();
        [bytes[0], bytes[1], bytes[2]]
    }

    fn raw_block(bytes: &[u8]) -> Vec<u8> {
        [&block_header(true, 0, bytes.len() as u32)[..], bytes].concat()
    }

    fn rle_block(last: bool, byte: u8, size: u32) -> Vec<u8> {
        [&block_header(last, 1, size)[..], &[byte]].concat()
    }

    /// A frame of `size` bytes of `z`, claiming `claim` in an eight-byte
    /// content size (descriptor 0xe0), in RLE blocks of at most 128 KiB.
    fn rle_frame(claim: u64, size: u64) -> Vec<u8> {
        let mut frame = [&MAGIC[..], &[0xe0], &claim.to_le_bytes()].concat();
        let mut left = size;
        while left > 0 {
            let block = left.min(128 * 1024);
            left -= block;
            frame.extend(rle_block(left == 0, b'z', block as u32));
        }
        frame
    }

    #[test]
    fn the_formats_are_frozen() {
        let key = Key::from_bytes([1; 32]);
        let raw = small_frame(5, &raw_block(b"hello"));
        let rle = small_frame(6, &rle_block(true, b'z', 6));
        let cases: [(u8, Vec<u8>, &[u8]); 3] = [
            (2, [&[0][..], b"as-is"].concat(), b"as-is"),
            (2, [&[1][..], &raw].concat(), b"hello"),
            (2, [&[1][..], &rle].concat(), b"zzzzzz"),
        ];
        for (format, body, item) in cases {
            let sealed = sealed_as(&key, Epoch(1), &[format], &body);
            assert_eq!(opened(&key, Epoch(1), &sealed), (DEVICE, item.to_vec()), "{format} {body:?}");
        }
    }

    #[test]
    fn a_frame_that_lies_about_its_size_is_refused() {
        let key = Key::from_bytes([1; 32]);
        let refused = [
            // Six bytes claimed, five in the block - and the other way round.
            small_frame(6, &raw_block(b"hello")),
            small_frame(4, &raw_block(b"hello")),
            // No size in the header: descriptor 0x00, then a window byte.
            [&MAGIC[..], &[0x00, 0x00], &rle_block(true, b'z', 6)].concat(),
            // A second frame behind the first.
            [rle_frame(6, 6), rle_frame(6, 6)].concat(),
        ];
        for frame in refused {
            let sealed = sealed_as(&key, Epoch(1), &[2], &[&[1][..], &frame].concat());
            assert!(open(&key, Epoch(1), &sealed).is_err(), "{frame:?} opened");
        }

        // The largest item expands; one byte more is refused for its claim,
        // before anything is allocated for it.
        let max = MAX_EXPANDED as u64;
        assert_eq!(expand(&rle_frame(max, max)).map(|data| data.len()), Some(MAX_EXPANDED));
        assert!(expand(&rle_frame(max + 1, max + 1)).is_none());
    }

    #[test]
    fn a_newer_format_or_codec_is_named() {
        let key = Key::from_bytes([1; 32]);
        let sealed = sealed_as(&key, Epoch(1), &[FORMAT + 1], &[]);
        assert!(matches!(open(&key, Epoch(1), &sealed), Ok(Opened::Newer(Part::Entry, v)) if v == FORMAT + 1));
        let sealed = sealed_as(&key, Epoch(1), &[FORMAT], &[7, 1, 2, 3]);
        assert!(matches!(open(&key, Epoch(1), &sealed), Ok(Opened::Newer(Part::Codec, 7))));
        // Format 1 - efema 0.2's: a device, then the item - opens as nothing.
        let format_one = [&[1][..], DEVICE.as_bytes(), b"format one"].concat();
        for plaintext in [&[][..], &[0], &[FORMAT, 1, 2], &format_one] {
            let sealed = key.seal(&context(Epoch(1)), plaintext).unwrap();
            assert!(open(&key, Epoch(1), &sealed).is_err(), "{plaintext:?} opened");
        }
    }

    #[test]
    fn a_key_entry_is_sealed_by_its_key() {
        let key = Key::from_bytes([1; 32]);
        let lock = LockedKey::lock_with(&key, b"passphrase", KEY_CONTEXT, lacodda_seal::KdfParams::MIN).unwrap();
        let entry = KeyEntry::new(&key, lock.clone(), 3).unwrap();
        let bytes = entry.to_bytes();
        assert!(lacodda_seal::is_locked_key(&bytes));
        assert_eq!(bytes.len(), lacodda_seal::LOCKED_LEN + KEY_SEAL_LEN);

        let read = KeyEntry::from_bytes(&bytes).unwrap();
        assert_eq!(read.lock, lock);
        assert_eq!(read.generation(&key), Some(3));
        assert_eq!(read.generation(&Key::from_bytes([9; 32])), None, "another key's seal");

        // The seal covers the lock: another lock under the same seal fails.
        let other = LockedKey::lock_with(&key, b"another", KEY_CONTEXT, lacodda_seal::KdfParams::MIN).unwrap();
        let swapped = [other.to_bytes(), bytes[lacodda_seal::LOCKED_LEN..].to_vec()].concat();
        assert_eq!(KeyEntry::from_bytes(&swapped).unwrap().generation(&key), None);
        assert!(KeyEntry::from_bytes(&bytes[..10]).is_err());
    }
}
