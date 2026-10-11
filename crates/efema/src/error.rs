//! What can go wrong, by what the app should do about it.

use std::path::PathBuf;

use efema_proto::{Epoch, StreamName};
use lacodda_seal::KeyId;

use crate::transport::TransportError;

/// Why a client operation failed.
///
/// The variants are sorted by what the app can do: try again later
/// ([`Error::Transport`], [`Error::Unsent`]), update itself ([`Error::EpochBehind`],
/// [`Error::NewerEpoch`], [`Error::NewerEntryFormat`], [`Error::NewerCodec`]),
/// ask the person ([`Error::WrongPassphrase`], [`Error::KeyMismatch`],
/// [`Error::PassphraseRace`]), or recover a stream
/// the relay no longer has as this device knew it ([`Error::StreamGone`],
/// [`Error::StreamReplaced`], [`Error::CursorAhead`],
/// [`Error::CursorDiverged`]).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The relay could not be reached, refused for a reason not listed below,
    /// or answered something that is not the protocol. Usually worth trying
    /// again later.
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// The stream this device synced with is not on the relay any more - its
    /// data was lost or wiped. Nothing was written: a client never creates a
    /// stream it already knew under a name.
    #[error("stream {stream} is gone from the relay")]
    StreamGone { stream: StreamName },
    /// The name now belongs to another incarnation of the stream - the relay
    /// was wiped and the stream written again. Nothing was written.
    #[error("stream {stream} on the relay is not the one this device synced with")]
    StreamReplaced { stream: StreamName },
    /// The relay has fewer entries than this device has read: it lost recent
    /// entries, typically to a restore from an older backup.
    #[error("stream {stream} ends at {head} on the relay, and this device has read up to {cursor}")]
    CursorAhead { stream: StreamName, cursor: u64, head: u64 },
    /// The relay's history at this device's cursor is not the one the device
    /// read - it went another way since.
    #[error("stream {stream} on the relay has another history at position {seq} than the one this device read")]
    CursorDiverged { stream: StreamName, seq: u64 },
    /// An entry's link does not follow from the one before: the relay sent a
    /// history that does not hold together.
    #[error("stream {stream}: the entry at position {seq} does not follow from the one before it")]
    BrokenChain { stream: StreamName, seq: u64 },

    /// A newer version of the app has moved the stream to a newer epoch; this
    /// one can no longer write to it. The app needs updating. `ours` is the
    /// epoch of what was being sent - for items an older version of the app
    /// left waiting, older than the app's own.
    #[error(
        "stream {stream} is at epoch {current}, and this app writes epoch {ours}: a newer version of the app has written to it"
    )]
    EpochBehind { stream: StreamName, ours: Epoch, current: Epoch },
    /// The next entry was written in a newer epoch than this app reads. The
    /// cursor stays before it; the app needs updating to read on.
    #[error("stream {stream}: the entry at position {seq} is in epoch {entry}, and this app reads up to epoch {ours}")]
    NewerEpoch { stream: StreamName, seq: u64, ours: Epoch, entry: Epoch },
    /// The next entry was sealed by a newer release of this library, in a
    /// format this one does not read. The cursor stays before it.
    #[error("stream {stream}: the entry at position {seq} is in entry format {version}, newer than this library reads")]
    NewerEntryFormat { stream: StreamName, seq: u64, version: u8 },
    /// The next entry was compressed by a newer release of this library, in
    /// a way this one does not read. The cursor stays before it.
    #[error(
        "stream {stream}: the entry at position {seq} is compressed with codec {codec}, newer than this library reads"
    )]
    NewerCodec { stream: StreamName, seq: u64, codec: u8 },

    /// The passphrase does not unlock the stream's key. When the passphrase
    /// was changed on another device, the old one stops opening the stream
    /// here once this device has read the change.
    #[error("the passphrase does not unlock the key of stream {stream}")]
    WrongPassphrase { stream: StreamName },
    /// Another device changed the passphrase at the same moment, and its
    /// change came first in the stream: the passphrase is now the one it set.
    #[error(
        "the passphrase of stream {stream} was changed on another device at the same moment, and that change holds"
    )]
    PassphraseRace { stream: StreamName },
    /// The key given is not the stream's key.
    #[error("stream {stream} is sealed under key {stream_key}, not under the key given ({ours})")]
    KeyMismatch { stream: StreamName, ours: KeyId, stream_key: KeyId },
    /// A key was given for a stream that has no key yet. A stream's key is
    /// created with a passphrase, so that every device can unlock it.
    #[error("stream {stream} has no key yet: open it with a passphrase the first time")]
    NoStreamKey { stream: StreamName },
    /// An entry is sealed under a key this device does not have.
    #[error("stream {stream}: the entry at position {seq} is sealed under key {key}, which this device does not have")]
    UnknownKey { stream: StreamName, seq: u64, key: KeyId },
    /// An entry does not open under the stream's key: it was changed after
    /// sealing. A relay can withhold entries but not alter them, and this is
    /// that refusal.
    #[error("stream {stream}: the entry at position {seq} does not open - it was changed after it was sealed")]
    Inauthentic { stream: StreamName, seq: u64 },
    /// An entry that no efema client wrote: neither a sealed entry nor a key.
    #[error("stream {stream}: the entry at position {seq} was not written by an efema client")]
    ForeignEntry { stream: StreamName, seq: u64 },
    /// Sealing or a key failed for a reason of its own.
    #[error(transparent)]
    Seal(#[from] lacodda_seal::Error),

    /// One item is larger than one request may carry.
    #[error("an item of {size} bytes is larger than the {limit} bytes one request may carry")]
    ItemTooLarge { size: usize, limit: usize },
    /// Sending stopped with items still waiting on this device: they are
    /// kept, sealed, and go with the next [`Client::push`](crate::Client::push)
    /// or [`Client::send`](crate::Client::send). The source says why it
    /// stopped - a [`Error::Transport`] is worth trying again later; an
    /// [`Error::EpochBehind`], [`Error::StreamGone`] or
    /// [`Error::StreamReplaced`] will not pass by waiting.
    #[error("{unsent} items wait on this device to be sent")]
    Unsent {
        unsent: usize,
        #[source]
        source: Box<Error>,
    },

    /// The state file could not be read or written.
    #[error("the sync state in {path} failed")]
    State {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    /// The state file belongs to a newer release of this library.
    #[error("the sync state in {path} has schema version {found}, newer than this library reads")]
    StateNewer { path: PathBuf, found: i64 },
    /// Another process has the state file open.
    #[error("the sync state in {0} is in use by another process")]
    StateBusy(PathBuf),
    /// The state file's lock could not be taken.
    #[error("cannot lock the sync state {path}")]
    StateLock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The operating system's random source failed.
    #[error("the operating system's random source failed")]
    Random,
}
