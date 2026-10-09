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
//!   each stream the cursor and the locked key. One file per device.
//! - [`Relay`] - the relay to talk to; it implements [`Transport`].
//! - [`Client`] - one device's view of one stream: [`push`](Client::push),
//!   [`pull`](Client::pull), [`ack`](Client::ack), [`wait`](Client::wait),
//!   [`change_passphrase`](Client::change_passphrase), and
//!   [`doctor`](Client::doctor) to report on all of it.
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
//! At least once. [`Client::pull`] reads from the device's acknowledged
//! cursor and [`Client::ack`] moves it, once the app has applied what it
//! pulled - so a crash in between hands the same entries over again, and an
//! app applies an entry twice without harm.
//!
//! # Epochs
//!
//! A client writes in its app's epoch and reads up to it. When a newer
//! version of the app moves a stream to a newer epoch, an older one stops in
//! front of the first newer entry ([`Error::NewerEpoch`]) and can no longer
//! write ([`Error::EpochBehind`]) - instead of misreading the newer format or
//! mixing the older one in.

mod client;
mod doctor;
mod envelope;
mod error;
mod relay;
mod state;
mod transport;

pub use client::{Client, Pulled, Pushed, Received, Secret};
pub use doctor::{KeyReport, RelayReport, Report};
pub use efema_proto::{Cursor, Epoch, StreamId, StreamName};
pub use envelope::{Compression, ENTRY_OVERHEAD};
pub use error::Error;
pub use lacodda_seal::{Key, KeyId, LockedKey};
pub use relay::{InvalidUrl, Relay};
pub use state::{DeviceId, State};
pub use transport::{Limits, Transport, TransportError};

/// The wire format and the hash chain, for writing a [`Transport`].
pub use efema_proto::{chain, wire};
