//! The [efema](https://github.com/lacodda/efema) client: sync a local-first
//! app's changes between devices through a relay that cannot read them.
//!
//! An app pushes its changes into a *stream* as opaque items; every other
//! device pulls what came after its cursor, in the same order for everyone.
//! What an item means and how two changes are merged stay with the app: this
//! crate moves bytes, sealed.
//!
//! **Every entry is sealed on the device.** A [`Client`] cannot be made
//! without a [`Secret`] - the stream's passphrase or its key - and has no
//! method that sends an item as it is. The relay stores ciphertext, and so
//! does every backup of it.
//!
//! # The pieces
//!
//! - [`State`] - what a device remembers between runs: its identity, and for
//!   each stream the cursor, the locked key and what waits to be sent. One
//!   file per device.
//! - [`Relay`] - the relay to talk to, with the [`AccessKey`] its operator
//!   handed out; it implements [`Transport`].
//! - [`Client`] - one device's view of one stream: [`push`](Client::push),
//!   [`send`](Client::send), [`pull`](Client::pull), [`ack`](Client::ack),
//!   [`wait`](Client::wait),
//!   [`change_passphrase`](Client::change_passphrase), and
//!   [`doctor`](Client::doctor) to report on all of it.
//! - [`copy`] - a stream moved to another relay or bucket, every batch at the
//!   position it had, so each device goes on there from where it stood.
//!
//! # Access
//!
//! A relay answers only devices with an access key, which opens one *space*
//! of the relay: its streams, under names of their own. A key reads the
//! space, or reads and writes it - [`Error::ReadOnly`] when a device with a
//! key that reads tries to write. The relay's operator hands keys out and
//! revokes them ([`Error::Unauthorized`]). An access key decides who reaches
//! the sealed entries; the stream's key below decides who can open them.
//!
//! What the relay holds a key to - the largest request, the largest entry,
//! how much its space may hold - it announces; the client keeps the answer in
//! the device's state, cuts its batches to it without the network, and asks
//! again when the relay refuses something as too large.
//!
//! # Keys
//!
//! The first device to open a stream creates its key - 32 random bytes - and
//! writes it to the stream locked under the passphrase (Argon2id, then
//! XChaCha20-Poly1305: [`lacodda_seal`]). Every other device finds that lock,
//! unlocks the key with the same passphrase and keeps the lock in its state,
//! so it opens without the network after that. The key itself is never
//! written anywhere unless the app asks for it with [`Client::key`].
//!
//! Changing the passphrase locks the same key again and writes the new lock
//! to the stream; nothing is sealed again, and every device moves to the new
//! lock as it reads it.
//!
//! # Compression
//!
//! Items are compressed with zstd before they are sealed, each one only if it
//! gets shorter - text usually to a third. [`Compression::Off`] turns it off
//! for an app that mixes secrets with text someone else chose in one item.
//!
//! # Delivery
//!
//! Reading is at least once. [`Client::pull`] reads from the device's
//! acknowledged cursor and [`Client::ack`] moves it, once the app has applied
//! what it pulled - so a crash in between hands the same entries over again,
//! and an app applies an entry twice without harm.
//!
//! Writing is exactly once. [`Client::push`] keeps what it seals in the
//! device's state, cut into the batches it will be sent as, before it sends
//! anything; a batch leaves the device when the relay has it. A torn
//! connection, a relay that is down, the app closing: the batches wait, and
//! go with the next push or [`Client::send`] - [`Error::Unsent`] says how
//! many wait. A batch whose answer was lost goes again byte for byte, and the
//! relay, which knows a batch by what it holds, does not write it twice.
//!
//! A connection that goes silent - a network dropped without a word, a proxy
//! whose upstream hung - is given up on once nothing has moved on it for
//! [`STALL`], and what was being sent waits like any other. A request that
//! keeps moving is never cut, however slow the network.
//!
//! # Epochs
//!
//! A client writes in its app's epoch and reads up to it. When a newer
//! version of the app moves a stream to a newer epoch, an older one stops in
//! front of the first newer entry ([`Error::NewerEpoch`]) and can no longer
//! write ([`Error::EpochBehind`]) - instead of misreading the newer format or
//! mixing the older one in.

mod bucket;
mod client;
mod copy;
mod doctor;
mod envelope;
mod error;
mod relay;
mod stall;
mod state;
mod transport;
#[cfg(test)]
mod vectors;

pub use bucket::{Bucket, Credentials};
pub use client::{Client, Pulled, Received, Secret, Sent};
pub use copy::{Copied, CopyError, copy};
pub use doctor::{KeyReport, QUEUE_STALE, RelayReport, Report};
pub use efema_proto::wire::Limits;
pub use efema_proto::{ACCESS_KEY_PREFIX, AccessKey, Cursor, Epoch, ParseAccessKeyError, StreamId, StreamName};
pub use envelope::{Compression, ENTRY_OVERHEAD};
pub use error::{Error, Part};
pub use lacodda_seal::{Key, KeyId, LockedKey};
pub use relay::{InvalidUrl, Relay};
pub use stall::STALL;
pub use state::{DeviceId, Queue, State};
pub use transport::{Transport, TransportError};

/// The wire format and the hash chain, for writing a [`Transport`].
pub use efema_proto::{chain, wire};
