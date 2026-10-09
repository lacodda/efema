//! Sealing for the lacodda line: bytes sealed under a key, and a key locked
//! under a passphrase.
//!
//! Two formats, each a few bytes of header in front of an XChaCha20-Poly1305
//! ciphertext. The header says which format and which version it is, and
//! everything a reader needs to open it except the secret - so a blob written
//! today opens with a later release of this crate, and a blob from a newer
//! format is refused by name instead of misread.
//!
//! - A [`Key`] is 32 random bytes. [`Key::seal`] turns bytes into a *sealed
//!   blob*, [`Key::open`] turns it back - or fails, if the blob was sealed
//!   under another key, for another context, or changed on the way.
//! - A [`LockedKey`] is a key wrapped under a passphrase: Argon2id stretches
//!   the passphrase into a key-encryption key, which seals the key. The
//!   Argon2id parameters and the salt travel in the header, so the lock can be
//!   stored anywhere - next to the data it protects, on a server that must not
//!   read it - and opened on any device that knows the passphrase.
//!
//! Every blob is bound to a *context* chosen by the caller - a few bytes that
//! say what the blob is for. Opening it under any other context fails, so a
//! blob cannot be moved from one use to another and accepted there.
//!
//! ```
//! use lacodda_seal::{Key, LockedKey};
//!
//! let key = Key::generate()?;
//! let sealed = key.seal(b"notes/v1", b"the plan for Tuesday")?;
//! assert_eq!(key.open(b"notes/v1", &sealed)?.as_slice(), b"the plan for Tuesday");
//! assert!(key.open(b"photos/v1", &sealed).is_err());
//!
//! // The key itself, locked under a passphrase, can be stored in the open.
//! let lock = LockedKey::lock(&key, b"correct horse battery staple", b"notes/key")?;
//! let again = LockedKey::from_bytes(&lock.to_bytes())?.unlock(b"correct horse battery staple", b"notes/key")?;
//! assert_eq!(again.id(), key.id());
//! # Ok::<(), lacodda_seal::Error>(())
//! ```
//!
//! The byte layouts are on the efema documentation site:
//! <https://lacodda.github.io/efema/concepts/sealing/>.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

/// Length of a key, in bytes.
pub const KEY_LEN: usize = 32;
/// Length of a key's identity, in bytes.
pub const KEY_ID_LEN: usize = 8;
/// Length of an Argon2id salt, in bytes.
pub const SALT_LEN: usize = 16;
/// Length of an XChaCha20-Poly1305 nonce, in bytes.
pub const NONCE_LEN: usize = 24;
/// Length of a Poly1305 tag, in bytes.
pub const TAG_LEN: usize = 16;

/// The first byte of a sealed blob.
const SEALED_KIND: u8 = b'S';
/// The first byte of a locked key.
const LOCKED_KIND: u8 = b'L';
/// The format version this release writes, of both formats.
const VERSION: u8 = 1;

/// A sealed blob's header: kind, version, key identity, nonce.
const SEALED_HEADER_LEN: usize = 2 + KEY_ID_LEN + NONCE_LEN;
/// How many bytes sealing adds to the plaintext: the header and the tag.
pub const SEALED_OVERHEAD: usize = SEALED_HEADER_LEN + TAG_LEN;

/// A locked key's header: kind, version, three Argon2id parameters, the time
/// of locking, salt, key identity, nonce.
const LOCKED_HEADER_LEN: usize = 2 + 4 + 4 + 4 + 8 + SALT_LEN + KEY_ID_LEN + NONCE_LEN;
/// Length of a locked key, in bytes: the header, the wrapped key and its tag.
pub const LOCKED_LEN: usize = LOCKED_HEADER_LEN + KEY_LEN + TAG_LEN;

// Domain separation: every SHA-256 and every AEAD input here starts with a
// string no other one starts with, so a value made for one purpose is never
// accepted for another.
const KEY_ID_DOMAIN: &[u8] = b"lacodda-seal/v1/key-id";
const SEALED_DOMAIN: &[u8] = b"lacodda-seal/v1/sealed";
const LOCKED_DOMAIN: &[u8] = b"lacodda-seal/v1/locked";

/// What can go wrong sealing, opening, locking or unlocking.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The bytes are not a blob of the expected kind: too short, too long, or
    /// with another first byte.
    #[error("not a {expected}: {reason}")]
    Malformed {
        /// What the bytes were read as.
        expected: &'static str,
        /// What is wrong with them.
        reason: &'static str,
    },
    /// The blob is of a format version this release does not know - written
    /// by a newer one.
    #[error("a {kind} of format version {found}, and this release reads up to {VERSION}")]
    NewerFormat {
        /// What the bytes were read as.
        kind: &'static str,
        /// The version they carry.
        found: u8,
    },
    /// The blob was sealed under another key; it names that key.
    #[error("sealed under key {found}, not under key {expected}")]
    WrongKey {
        /// The key the blob was opened with.
        expected: KeyId,
        /// The key the blob says it was sealed under.
        found: KeyId,
    },
    /// The blob does not open under this key and this context: it was sealed
    /// for another context, or changed after sealing. The two cannot be told
    /// apart, and this error does not try.
    #[error("the blob does not open: it was sealed for another purpose, or changed after sealing")]
    Inauthentic,
    /// The lock does not open with this passphrase: the passphrase is wrong,
    /// the context is another one, or the lock was changed. Indistinguishable
    /// on purpose.
    #[error("the passphrase does not unlock this key")]
    WrongPassphrase,
    /// Argon2id parameters outside what this release accepts.
    #[error("Argon2id parameters out of bounds: {0}")]
    Parameters(String),
    /// The operating system could not supply random bytes.
    #[error("the operating system's random source failed")]
    Random,
}

/// The identity of a key: eight bytes of a hash over it.
///
/// Travels in the clear in every header, so a reader holding several keys
/// knows which one a blob needs without trying each. It reveals nothing about
/// the key - only whether two blobs were sealed under the same one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyId([u8; KEY_ID_LEN]);

impl KeyId {
    /// Wraps eight bytes.
    pub const fn from_bytes(bytes: [u8; KEY_ID_LEN]) -> Self {
        Self(bytes)
    }

    /// The eight bytes.
    pub const fn as_bytes(&self) -> &[u8; KEY_ID_LEN] {
        &self.0
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyId({self})")
    }
}

/// A symmetric key: 32 bytes, wiped from memory when dropped.
///
/// [`Debug`](fmt::Debug) prints its identity, never the bytes.
#[derive(Clone)]
pub struct Key {
    bytes: Zeroizing<[u8; KEY_LEN]>,
    id: KeyId,
}

impl Key {
    /// A new key from the operating system's random source.
    ///
    /// # Errors
    ///
    /// [`Error::Random`] when there is no randomness to be had.
    pub fn generate() -> Result<Self, Error> {
        Ok(Self::from_bytes(*random::<KEY_LEN>()?))
    }

    /// A key from bytes kept elsewhere - a keyring, an encrypted store, a QR
    /// code shown by another device.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        let bytes = Zeroizing::new(bytes);
        let mut hasher = Sha256::new();
        hasher.update(KEY_ID_DOMAIN);
        hasher.update(bytes.as_slice());
        let digest = hasher.finalize();
        let mut id = [0u8; KEY_ID_LEN];
        id.copy_from_slice(&digest[..KEY_ID_LEN]);
        Self { bytes, id: KeyId(id) }
    }

    /// The key's bytes, for keeping it somewhere safe. They are the key: what
    /// holds them can open everything sealed under it.
    pub fn to_bytes(&self) -> Zeroizing<[u8; KEY_LEN]> {
        self.bytes.clone()
    }

    /// The key's identity.
    pub fn id(&self) -> KeyId {
        self.id
    }

    /// Seals `plaintext` for `context` under a fresh random nonce.
    ///
    /// The result is [`SEALED_OVERHEAD`] bytes longer than the plaintext.
    /// Sealing the same plaintext twice gives two different blobs.
    ///
    /// # Errors
    ///
    /// [`Error::Random`] when there is no randomness for the nonce.
    pub fn seal(&self, context: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.seal_with_nonce(context, plaintext, *random::<NONCE_LEN>()?))
    }

    fn seal_with_nonce(&self, context: &[u8], plaintext: &[u8], nonce: [u8; NONCE_LEN]) -> Vec<u8> {
        let mut blob = Vec::with_capacity(SEALED_OVERHEAD + plaintext.len());
        blob.extend_from_slice(&[SEALED_KIND, VERSION]);
        blob.extend_from_slice(&self.id.0);
        blob.extend_from_slice(&nonce);
        let aad = associated(SEALED_DOMAIN, &blob, context);
        let ciphertext = self
            .cipher()
            .encrypt(&XNonce::from(nonce), Payload { msg: plaintext, aad: &aad })
            .expect("XChaCha20-Poly1305 seals any length a Vec can hold");
        blob.extend_from_slice(&ciphertext);
        blob
    }

    /// Opens a blob sealed for `context` under this key.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] or [`Error::NewerFormat`] when the bytes are not a
    /// sealed blob this release reads; [`Error::WrongKey`] when it was sealed
    /// under another key; [`Error::Inauthentic`] when it was sealed for another
    /// context or changed since.
    pub fn open(&self, context: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
        let found = sealed_key_id(sealed)?;
        if found != self.id {
            return Err(Error::WrongKey { expected: self.id, found });
        }
        let (header, ciphertext) = sealed.split_at(SEALED_HEADER_LEN);
        let nonce: [u8; NONCE_LEN] = header[2 + KEY_ID_LEN..].try_into().expect("the header has a nonce");
        let aad = associated(SEALED_DOMAIN, header, context);
        self.cipher()
            .decrypt(&XNonce::from(nonce), Payload { msg: ciphertext, aad: &aad })
            .map(Zeroizing::new)
            .map_err(|_| Error::Inauthentic)
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&(*self.bytes).into())
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({})", self.id)
    }
}

/// The identity of the key a sealed blob was sealed under, read from its
/// header without opening it.
///
/// # Errors
///
/// [`Error::Malformed`] or [`Error::NewerFormat`] when the bytes are not a
/// sealed blob this release reads.
pub fn sealed_key_id(sealed: &[u8]) -> Result<KeyId, Error> {
    const KIND: &str = "sealed blob";
    check_header(sealed, SEALED_KIND, KIND)?;
    if sealed.len() < SEALED_OVERHEAD {
        return Err(Error::Malformed { expected: KIND, reason: "shorter than a header and a tag" });
    }
    Ok(KeyId(sealed[2..2 + KEY_ID_LEN].try_into().expect("the header has a key identity")))
}

/// How hard Argon2id works to turn a passphrase into a key.
///
/// Stored in every [`LockedKey`], so a lock made with stronger parameters
/// opens with the same code. A lock read from elsewhere is held to the bounds
/// below before any work is spent on it: a planted lock asking for a terabyte
/// of memory is refused, not attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

impl KdfParams {
    /// What [`LockedKey::lock`] uses: 64 MiB, three passes, four lanes - a
    /// fraction of a second on a laptop, the same as sefy's vaults.
    pub const DEFAULT: Self = Self { memory_kib: 64 * 1024, iterations: 3, parallelism: 4 };
    /// The weakest parameters accepted: OWASP's minimum for Argon2id (19 MiB,
    /// two passes, one lane).
    pub const MIN: Self = Self { memory_kib: 19 * 1024, iterations: 2, parallelism: 1 };
    /// The strongest parameters accepted: 1 GiB, 16 passes, 16 lanes.
    pub const MAX: Self = Self { memory_kib: 1024 * 1024, iterations: 16, parallelism: 16 };

    /// Parameters within [`KdfParams::MIN`] and [`KdfParams::MAX`].
    ///
    /// # Errors
    ///
    /// [`Error::Parameters`] when any of the three is out of bounds.
    pub fn new(memory_kib: u32, iterations: u32, parallelism: u32) -> Result<Self, Error> {
        let params = Self { memory_kib, iterations, parallelism };
        params.check()?;
        Ok(params)
    }

    /// Memory, in KiB.
    pub fn memory_kib(&self) -> u32 {
        self.memory_kib
    }

    /// Passes over the memory.
    pub fn iterations(&self) -> u32 {
        self.iterations
    }

    /// Lanes.
    pub fn parallelism(&self) -> u32 {
        self.parallelism
    }

    fn check(&self) -> Result<(), Error> {
        let within = |value: u32, min: u32, max: u32, what: &str| {
            if (min..=max).contains(&value) {
                Ok(())
            } else {
                Err(Error::Parameters(format!("{what} is {value}, and it must be from {min} to {max}")))
            }
        };
        within(self.memory_kib, Self::MIN.memory_kib, Self::MAX.memory_kib, "memory (KiB)")?;
        within(self.iterations, Self::MIN.iterations, Self::MAX.iterations, "iterations")?;
        within(self.parallelism, Self::MIN.parallelism, Self::MAX.parallelism, "parallelism")
    }

    fn derive(&self, passphrase: &[u8], salt: &[u8; SALT_LEN]) -> Result<Key, Error> {
        let params = Params::new(self.memory_kib, self.iterations, self.parallelism, Some(KEY_LEN))
            .map_err(|e| Error::Parameters(e.to_string()))?;
        let mut kek = Zeroizing::new([0u8; KEY_LEN]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase, salt, kek.as_mut())
            .map_err(|e| Error::Parameters(e.to_string()))?;
        Ok(Key::from_bytes(*kek))
    }
}

impl fmt::Display for KdfParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let memory = if self.memory_kib.is_multiple_of(1024) {
            format!("{} MiB", self.memory_kib / 1024)
        } else {
            format!("{} KiB", self.memory_kib)
        };
        write!(f, "argon2id {memory} × {} × {}", self.iterations, self.parallelism)
    }
}

/// A key locked under a passphrase.
///
/// Safe to store in the open - next to the data, on a server - for as long as
/// the passphrase is strong: whoever holds a lock can try passphrases against
/// it offline, as fast as Argon2id lets them. That cost is the whole of its
/// protection.
#[derive(Clone, PartialEq, Eq)]
pub struct LockedKey {
    params: KdfParams,
    locked_at: u64,
    salt: [u8; SALT_LEN],
    key_id: KeyId,
    nonce: [u8; NONCE_LEN],
    wrapped: [u8; KEY_LEN + TAG_LEN],
}

impl LockedKey {
    /// Locks `key` under `passphrase` for `context`, with
    /// [`KdfParams::DEFAULT`].
    ///
    /// # Errors
    ///
    /// [`Error::Random`] when there is no randomness for the salt and nonce.
    pub fn lock(key: &Key, passphrase: &[u8], context: &[u8]) -> Result<Self, Error> {
        Self::lock_with(key, passphrase, context, KdfParams::DEFAULT)
    }

    /// Locks `key` under `passphrase` for `context`, with `params`.
    ///
    /// # Errors
    ///
    /// [`Error::Random`] when there is no randomness for the salt and nonce;
    /// [`Error::Parameters`] when Argon2id refuses `params`.
    pub fn lock_with(key: &Key, passphrase: &[u8], context: &[u8], params: KdfParams) -> Result<Self, Error> {
        let salt = *random::<SALT_LEN>()?;
        let nonce = *random::<NONCE_LEN>()?;
        Self::lock_exactly(key, passphrase, context, params, unix_now(), salt, nonce)
    }

    fn lock_exactly(
        key: &Key,
        passphrase: &[u8],
        context: &[u8],
        params: KdfParams,
        locked_at: u64,
        salt: [u8; SALT_LEN],
        nonce: [u8; NONCE_LEN],
    ) -> Result<Self, Error> {
        params.check()?;
        let mut lock = Self { params, locked_at, salt, key_id: key.id, nonce, wrapped: [0; KEY_LEN + TAG_LEN] };
        let kek = params.derive(passphrase, &salt)?;
        let aad = associated(LOCKED_DOMAIN, &lock.header(), context);
        let wrapped = kek
            .cipher()
            .encrypt(&XNonce::from(nonce), Payload { msg: key.bytes.as_slice(), aad: &aad })
            .expect("XChaCha20-Poly1305 seals 32 bytes");
        lock.wrapped.copy_from_slice(&wrapped);
        Ok(lock)
    }

    /// Unlocks the key with `passphrase`, for `context`.
    ///
    /// Spends what the lock's parameters ask for - by default 64 MiB and a
    /// fraction of a second.
    ///
    /// # Errors
    ///
    /// [`Error::WrongPassphrase`] when the passphrase or the context is not
    /// the one the key was locked with, or the lock was changed since.
    pub fn unlock(&self, passphrase: &[u8], context: &[u8]) -> Result<Key, Error> {
        let kek = self.params.derive(passphrase, &self.salt)?;
        let aad = associated(LOCKED_DOMAIN, &self.header(), context);
        let mut bytes = kek
            .cipher()
            .decrypt(&XNonce::from(self.nonce), Payload { msg: &self.wrapped, aad: &aad })
            .map_err(|_| Error::WrongPassphrase)?;
        let mut array = [0u8; KEY_LEN];
        array.copy_from_slice(&bytes);
        bytes.zeroize();
        let key = Key::from_bytes(array);
        array.zeroize();
        // The identity is authenticated with the rest of the header, so a
        // mismatch here means the locking side computed it wrongly - not
        // tampering. Refused all the same: the lock would name another key.
        if key.id != self.key_id {
            return Err(Error::WrongPassphrase);
        }
        Ok(key)
    }

    /// The identity of the key inside, readable without the passphrase.
    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// The Argon2id parameters the lock was made with.
    pub fn params(&self) -> KdfParams {
        self.params
    }

    /// When the lock was made, by the clock of the device that made it.
    pub fn locked_at(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.locked_at)
    }

    /// The lock as bytes: [`LOCKED_LEN`] of them.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = self.header();
        bytes.extend_from_slice(&self.wrapped);
        bytes
    }

    /// Reads a lock, and holds its parameters to [`KdfParams::MIN`] and
    /// [`KdfParams::MAX`] before anything is spent on them.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] or [`Error::NewerFormat`] when the bytes are not a
    /// lock this release reads; [`Error::Parameters`] when its parameters are
    /// out of bounds.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        const KIND: &str = "locked key";
        check_header(bytes, LOCKED_KIND, KIND)?;
        if bytes.len() != LOCKED_LEN {
            return Err(Error::Malformed { expected: KIND, reason: "not the length of a locked key" });
        }
        let mut fields = Fields { bytes, at: 2 };
        let params = KdfParams {
            memory_kib: u32::from_be_bytes(fields.take()),
            iterations: u32::from_be_bytes(fields.take()),
            parallelism: u32::from_be_bytes(fields.take()),
        };
        let locked_at = u64::from_be_bytes(fields.take());
        let salt = fields.take();
        let key_id = KeyId(fields.take());
        let nonce = fields.take();
        let wrapped = fields.take();
        params.check()?;
        Ok(Self { params, locked_at, salt, key_id, nonce, wrapped })
    }

    fn header(&self) -> Vec<u8> {
        let mut header = Vec::with_capacity(LOCKED_LEN);
        header.extend_from_slice(&[LOCKED_KIND, VERSION]);
        header.extend_from_slice(&self.params.memory_kib.to_be_bytes());
        header.extend_from_slice(&self.params.iterations.to_be_bytes());
        header.extend_from_slice(&self.params.parallelism.to_be_bytes());
        header.extend_from_slice(&self.locked_at.to_be_bytes());
        header.extend_from_slice(&self.salt);
        header.extend_from_slice(&self.key_id.0);
        header.extend_from_slice(&self.nonce);
        header
    }
}

impl fmt::Debug for LockedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LockedKey")
            .field("key_id", &self.key_id)
            .field("params", &self.params)
            .field("locked_at", &self.locked_at)
            .finish_non_exhaustive()
    }
}

/// Whether `bytes` start as a sealed blob does - the first byte of the
/// format, whatever its version. For telling the two kinds apart when they
/// travel side by side; [`Key::open`] still decides whether one opens.
pub fn is_sealed(bytes: &[u8]) -> bool {
    bytes.first() == Some(&SEALED_KIND)
}

/// Whether `bytes` start as a locked key does, whatever its version.
pub fn is_locked_key(bytes: &[u8]) -> bool {
    bytes.first() == Some(&LOCKED_KIND)
}

/// Fixed-width fields read one after another. The length was checked before
/// the first one is taken, so a take never runs past the end.
struct Fields<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Fields<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let field = self.bytes[self.at..self.at + N].try_into().expect("the length was checked");
        self.at += N;
        field
    }
}

fn check_header(bytes: &[u8], kind: u8, name: &'static str) -> Result<(), Error> {
    match bytes {
        [] | [_] => Err(Error::Malformed { expected: name, reason: "shorter than a header" }),
        [first, ..] if *first != kind => Err(Error::Malformed { expected: name, reason: "another kind of blob" }),
        [_, version, ..] if *version == VERSION => Ok(()),
        [_, version, ..] if *version > VERSION => Err(Error::NewerFormat { kind: name, found: *version }),
        _ => Err(Error::Malformed { expected: name, reason: "format version 0 does not exist" }),
    }
}

/// The associated data of a blob: its domain, its header, the caller's
/// context. The header and the domain have one length per format and version,
/// so the context needs no length of its own to stay unambiguous.
fn associated(domain: &[u8], header: &[u8], context: &[u8]) -> Vec<u8> {
    [domain, header, context].concat()
}

fn random<const N: usize>() -> Result<Zeroizing<[u8; N]>, Error> {
    let mut bytes = Zeroizing::new([0u8; N]);
    getrandom::fill(bytes.as_mut()).map_err(|_| Error::Random)?;
    Ok(bytes)
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key::from_bytes([7; KEY_LEN])
    }

    #[test]
    fn a_sealed_blob_opens_under_its_key_and_context_only() {
        let key = key();
        let sealed = key.seal(b"ctx", b"payload").unwrap();
        assert_eq!(key.open(b"ctx", &sealed).unwrap().as_slice(), b"payload");
        assert_eq!(key.open(b"other", &sealed), Err(Error::Inauthentic));
        let other = Key::from_bytes([8; KEY_LEN]);
        assert_eq!(other.open(b"ctx", &sealed), Err(Error::WrongKey { expected: other.id(), found: key.id() }));
    }

    #[test]
    fn every_byte_of_a_sealed_blob_is_covered() {
        // Flipping any bit - header, nonce, ciphertext or tag - is refused.
        let key = key();
        let sealed = key.seal(b"ctx", b"payload").unwrap();
        for index in 0..sealed.len() {
            let mut changed = sealed.clone();
            changed[index] ^= 0x01;
            assert!(key.open(b"ctx", &changed).is_err(), "byte {index} is not authenticated");
        }
    }

    #[test]
    fn sealing_twice_gives_two_blobs_of_a_known_size() {
        let key = key();
        let first = key.seal(b"", b"payload").unwrap();
        let second = key.seal(b"", b"payload").unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), b"payload".len() + SEALED_OVERHEAD);
        assert_eq!(key.seal(b"", b"").unwrap().len(), SEALED_OVERHEAD);
    }

    #[test]
    fn a_blob_of_another_kind_or_a_newer_version_is_named_not_misread() {
        let key = key();
        let mut sealed = key.seal(b"", b"x").unwrap();
        assert!(matches!(key.open(b"", &sealed[..1]), Err(Error::Malformed { .. })));
        assert!(matches!(key.open(b"", &sealed[..SEALED_OVERHEAD - 1]), Err(Error::Malformed { .. })));
        sealed[1] = VERSION + 1;
        assert_eq!(key.open(b"", &sealed), Err(Error::NewerFormat { kind: "sealed blob", found: VERSION + 1 }));
        sealed[0] = LOCKED_KIND;
        assert!(matches!(key.open(b"", &sealed), Err(Error::Malformed { .. })));
    }

    #[test]
    fn a_key_round_trips_through_its_lock() {
        let key = key();
        let lock = LockedKey::lock_with(&key, b"pass phrase", b"ctx", KdfParams::MIN).unwrap();
        assert_eq!(lock.key_id(), key.id());
        let bytes = lock.to_bytes();
        assert_eq!(bytes.len(), LOCKED_LEN);
        let read = LockedKey::from_bytes(&bytes).unwrap();
        assert_eq!(read, lock);
        assert_eq!(*read.unlock(b"pass phrase", b"ctx").unwrap().to_bytes(), *key.to_bytes());
        assert_eq!(read.unlock(b"pass phrasE", b"ctx").unwrap_err(), Error::WrongPassphrase);
        assert_eq!(read.unlock(b"pass phrase", b"other").unwrap_err(), Error::WrongPassphrase);
    }

    #[test]
    fn every_byte_of_a_lock_is_covered() {
        let key = key();
        let lock = LockedKey::lock_with(&key, b"pw", b"ctx", KdfParams::MIN).unwrap().to_bytes();
        // The parameters are covered by refusing to read them, the rest by
        // the tag. Skipping the parameters' bytes would leave them unchecked:
        // a bit flipped there must fail one way or the other.
        for index in 0..lock.len() {
            let mut changed = lock.clone();
            changed[index] ^= 0x01;
            let outcome = LockedKey::from_bytes(&changed).and_then(|l| l.unlock(b"pw", b"ctx"));
            assert!(outcome.is_err(), "byte {index} of a lock is not authenticated");
        }
    }

    #[test]
    fn a_lock_out_of_bounds_is_refused_before_any_work() {
        let key = key();
        let lock = LockedKey::lock_with(&key, b"pw", b"ctx", KdfParams::MIN).unwrap().to_bytes();
        let mut greedy = lock.clone();
        // memory = 4 TiB: refused on reading, not attempted.
        greedy[2..6].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(LockedKey::from_bytes(&greedy), Err(Error::Parameters(_))));
        let mut weak = lock;
        weak[6..10].copy_from_slice(&1u32.to_be_bytes());
        assert!(matches!(LockedKey::from_bytes(&weak), Err(Error::Parameters(_))));
        assert!(KdfParams::new(KdfParams::MAX.memory_kib + 1, 3, 4).is_err());
        assert_eq!(KdfParams::new(65536, 3, 4).unwrap(), KdfParams::DEFAULT);
    }

    #[test]
    fn kinds_are_told_apart_by_their_first_byte() {
        let key = key();
        let sealed = key.seal(b"", b"x").unwrap();
        let lock = LockedKey::lock_with(&key, b"pw", b"", KdfParams::MIN).unwrap().to_bytes();
        assert!(is_sealed(&sealed) && !is_locked_key(&sealed));
        assert!(is_locked_key(&lock) && !is_sealed(&lock));
        assert!(matches!(LockedKey::from_bytes(&sealed), Err(Error::Malformed { .. })));
        assert!(!is_sealed(&[]) && !is_locked_key(&[]));
    }

    #[test]
    fn debug_never_shows_the_key() {
        let key = key();
        let shown = format!("{key:?}");
        assert!(shown.starts_with("Key(") && !shown.contains("07, 07"), "{shown}");
        assert_eq!(shown, format!("Key({})", key.id()));
    }

    // Frozen values, computed outside Rust - Python's `cryptography` (OpenSSL's
    // Argon2id and ChaCha20-Poly1305) with HChaCha20 written out by hand - so
    // they check the formats rather than echo this code. Every blob anyone has
    // sealed depends on these bytes: if this test fails, the change is a new
    // format version, not a fix.
    #[test]
    fn the_formats_are_frozen() {
        let key = Key::from_bytes(*b"0123456789abcdef0123456789abcdef");
        assert_eq!(key.id().to_string(), FROZEN_KEY_ID);

        let sealed = key.seal_with_nonce(b"context", b"hello", *b"nonce-nonce-nonce-nonce!");
        assert_eq!(hex(&sealed), FROZEN_SEALED);

        let lock = LockedKey::lock_exactly(
            &key,
            b"correct horse battery staple",
            b"context",
            KdfParams::MIN,
            1_760_000_000,
            *b"salt-salt-salt-s",
            *b"nonce-nonce-nonce-nonce!",
        )
        .unwrap();
        assert_eq!(hex(&lock.to_bytes()), FROZEN_LOCKED);
    }

    const FROZEN_KEY_ID: &str = "fbcf86ef40348bea";
    const FROZEN_SEALED: &str = concat!(
        "5301fbcf86ef40348bea6e6f6e63652d6e6f6e63652d6e6f6e63652d6e6f6e63652143f707c8a51d90bc9391bd9cb2776f280e",
        "74c75af6"
    );
    const FROZEN_LOCKED: &str = concat!(
        "4c0100004c0000000002000000010000000068e7780073616c742d73616c742d73616c742d73fbcf86ef40348bea6e6f6e",
        "63652d6e6f6e63652d6e6f6e63652d6e6f6e63652148cf6c30e1cc7edbd043709d3dbbd646ce6cb2a0cb26a692270f6818",
        "e32c3e4df95dc80e3323b31aea6cbfc49a917243"
    );

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
