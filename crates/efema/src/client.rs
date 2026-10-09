//! A device's client of one stream.

use std::ops::ControlFlow;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use efema_proto::wire::{Batch, Entry, ProblemCode, Watch};
use efema_proto::{Cursor, Epoch, StreamId, StreamName, chain};
use lacodda_seal::{Key, KeyId, LockedKey};

use crate::envelope::{self, Compression, ENTRY_OVERHEAD, KEY_CONTEXT, KeyEntry, Opened};
use crate::state::State;
use crate::transport::{Transport, TransportError};
use crate::{DeviceId, Error};

/// What a client opens a stream with. Required: there is no client without
/// one, and so no way to send an entry unsealed.
pub enum Secret<'a> {
    /// The stream's passphrase. Creates the stream's key the first time any
    /// device opens the stream; unlocks it on every device after that.
    Passphrase(&'a [u8]),
    /// The stream's key itself, as [`Client::key`] gave it on a device that
    /// unlocked it - kept in a keyring, say, so the passphrase is asked for
    /// once. A key cannot create a stream: the first device needs the
    /// passphrase, so that the others can unlock the key with it.
    Key(Key),
}

/// One device's view of one stream: it seals what it pushes, opens what it
/// pulls, and remembers how far it has read.
///
/// ```no_run
/// use efema::{Client, Epoch, Relay, Secret, State};
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let state = State::open("sync.sqlite3")?;
/// let relay = Relay::new("https://sync.example.com")?;
/// let notes = Client::open(relay, &state, "notes".parse()?, Epoch(1), Secret::Passphrase(b"a long passphrase")).await?;
///
/// notes.push([b"a change".as_slice(), b"another one"]).await?;
///
/// loop {
///     let pulled = notes.pull().await?;
///     for entry in &pulled.entries {
///         if !entry.own {
///             // apply entry.data to the app's own data
///         }
///     }
///     notes.ack(&pulled).await?;
///     if !pulled.more() {
///         break;
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct Client<T> {
    transport: T,
    state: State,
    stream: StreamName,
    id: StreamId,
    epoch: Epoch,
    key: Key,
    lock: Mutex<Current>,
    compression: Compression,
}

/// The newest lock of the stream's key this device has read.
#[derive(Clone)]
struct Current {
    lock: LockedKey,
    generation: u32,
}

/// What one entry is for the device.
enum Step {
    /// An entry for the app.
    Entry(Received),
    /// A lock of the stream's key, of this generation.
    Relock(LockedKey, u32),
    /// Nothing: a lock of another key.
    Nothing,
}

impl<T: Transport> std::fmt::Debug for Client<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("transport", &self.transport.describe())
            .field("stream", &self.stream)
            .field("stream_id", &self.id)
            .field("epoch", &self.epoch)
            .field("key", &self.key)
            .field("device", &self.state.device())
            .field("compression", &self.compression)
            .finish()
    }
}

/// An entry as the app receives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    /// Its position in the stream.
    pub seq: u64,
    /// The epoch it was written in - at most the client's own.
    pub epoch: Epoch,
    /// The device that wrote it.
    pub device: DeviceId,
    /// Whether this device wrote it: an app usually has its own changes
    /// already, and only needs their place in the order.
    pub own: bool,
    /// The bytes as the app pushed them.
    pub data: Vec<u8>,
}

/// What one [`Client::pull`] read.
#[derive(Clone, Debug)]
pub struct Pulled {
    /// The entries, in position order. Keys and other entries that carry
    /// nothing for the app are left out, but passed over: acknowledging
    /// moves past them too.
    pub entries: Vec<Received>,
    through: Option<Cursor>,
    head: u64,
}

impl Pulled {
    /// Whether the stream has more after this pull - and the app should pull
    /// again once it has acknowledged this one.
    pub fn more(&self) -> bool {
        self.through.is_some_and(|cursor| cursor.seq < self.head)
    }

    /// Where the device will stand once this pull is acknowledged; `None` if
    /// there was nothing new.
    pub fn cursor(&self) -> Option<&Cursor> {
        self.through.as_ref()
    }
}

/// Where a push landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pushed {
    /// The position of each item, in the order they were given.
    pub positions: Vec<u64>,
}

impl<T: Transport> Client<T> {
    /// Opens `stream` for this device.
    ///
    /// The first time a device opens a stream it needs the relay: it finds
    /// the stream's key there and unlocks it with the passphrase - or, if the
    /// stream does not exist yet, creates the key, locks it under the
    /// passphrase and writes the lock as the stream's first entry. After that
    /// the locked key is in the device's [`State`], and opening needs no
    /// network.
    ///
    /// `epoch` is the format the app writes in, and the newest it reads.
    ///
    /// # Errors
    ///
    /// [`Error::WrongPassphrase`], [`Error::KeyMismatch`] and
    /// [`Error::NoStreamKey`] when the secret does not fit the stream;
    /// [`Error::Transport`] when the relay is needed and cannot be reached.
    pub async fn open(
        transport: T,
        state: &State,
        stream: StreamName,
        epoch: Epoch,
        secret: Secret<'_>,
    ) -> Result<Self, Error> {
        let known = blocking({
            let state = state.clone();
            let stream = stream.clone();
            move || state.known(&stream)
        })
        .await?;
        let (id, current, key) = match known {
            Some(known) => {
                let lock = LockedKey::from_bytes(&known.lock)?;
                let key = take_key(&lock, secret, &stream).await?;
                (known.stream, Current { lock, generation: known.generation }, key)
            }
            None => {
                let (id, current, key) = establish(&transport, &stream, epoch, secret).await?;
                blocking({
                    let state = state.clone();
                    let stream = stream.clone();
                    let lock = current.lock.to_bytes();
                    let generation = current.generation;
                    move || state.join(&stream, id, &lock, generation)
                })
                .await?;
                (id, current, key)
            }
        };
        let lock = Mutex::new(current);
        Ok(Self { transport, state: state.clone(), stream, id, epoch, key, lock, compression: Compression::default() })
    }

    /// The client with items compressed - or not - before they are sealed.
    /// zstd by default; see [`Compression::Off`] for when not to.
    #[must_use]
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Whether items are compressed before they are sealed.
    pub fn compression(&self) -> Compression {
        self.compression
    }

    /// Compresses and seals each item, and appends them to the stream, in
    /// order.
    ///
    /// Items that fit in one request go as one batch - all on consecutive
    /// positions, or none. More than that is cut into several batches, each
    /// whole on its own; if one fails, [`Error::Interrupted`] says how many
    /// items made it.
    ///
    /// # Errors
    ///
    /// [`Error::ItemTooLarge`] before anything is sent, when one item cannot
    /// fit in a request uncompressed - whether it compresses does not change
    /// the limit; [`Error::EpochBehind`] when a newer version of the
    /// app has moved the stream on; [`Error::StreamGone`] and
    /// [`Error::StreamReplaced`] when the relay no longer has the stream this
    /// device knew - nothing is written then.
    pub async fn push<I>(&self, items: I) -> Result<Pushed, Error>
    where
        I: IntoIterator,
        I::Item: AsRef<[u8]>,
    {
        let limits = self.transport.limits().await?;
        let sizer = Sizer::new(self.epoch, limits.max_body);
        let mut batches: Vec<Vec<Vec<u8>>> = Vec::new();
        let mut body = usize::MAX;
        for item in items {
            let item = item.as_ref();
            if item.len() > sizer.max_item() {
                return Err(Error::ItemTooLarge { size: item.len(), limit: sizer.max_item() });
            }
            let entry = envelope::seal(&self.key, self.epoch, self.state.device(), item, self.compression)?;
            match batches.last_mut() {
                Some(batch) if sizer.fits(body, batch.len(), entry.len()) => {
                    body = sizer.grow(body, batch.len(), entry.len());
                    batch.push(entry);
                }
                _ => {
                    body = sizer.grow(sizer.empty(), 0, entry.len());
                    batches.push(vec![entry]);
                }
            }
        }

        let mut positions = Vec::new();
        for entries in batches {
            let count = entries.len() as u64;
            let batch = Batch { epoch: self.epoch, entries, stream: Some(self.id) };
            let landed = match self.transport.append(&self.stream, &batch).await {
                Ok(written) if written.stream != self.id => Err(Error::StreamReplaced { stream: self.stream.clone() }),
                Ok(written) if written.last + 1 - written.first != count => Err(Error::Transport(not_protocol(
                    &self.transport,
                    format!("{count} entries were sent and {}..{} came back", written.first, written.last),
                ))),
                Ok(written) => Ok(written),
                Err(e) => Err(write_error(e, &self.stream, self.epoch)),
            };
            match landed {
                Ok(written) => positions.extend(written.first..=written.last),
                Err(e) if positions.is_empty() => return Err(e),
                Err(e) => return Err(Error::Interrupted { written: positions.len(), source: Box::new(e) }),
            }
        }
        Ok(Pushed { positions })
    }

    /// Reads what came after this device's acknowledged cursor: one page,
    /// checked link by link and opened.
    ///
    /// Pulling does not move the cursor - [`Client::ack`] does, once the app
    /// has applied what it pulled. Until then the same entries come again, on
    /// this run or the next: delivery is at least once, and an app applies an
    /// entry it has seen before without harm.
    ///
    /// A pull returns everything it can. The first entry it cannot - from a
    /// newer epoch, sealed under another key, not following from the one
    /// before - ends the page, and becomes the error of the next pull, with
    /// the cursor still in front of it.
    ///
    /// # Errors
    ///
    /// [`Error::NewerEpoch`] and [`Error::NewerEntryFormat`] when the next
    /// entry needs a newer app; [`Error::StreamGone`],
    /// [`Error::StreamReplaced`], [`Error::CursorAhead`] and
    /// [`Error::CursorDiverged`] when the relay's history is no longer the
    /// one this device read; [`Error::BrokenChain`] and
    /// [`Error::Inauthentic`] when the relay sent entries that do not hold
    /// together or were changed.
    pub async fn pull(&self) -> Result<Pulled, Error> {
        let limits = self.transport.limits().await?;
        let cursor = self.cursor().await?;
        let page = self
            .transport
            .read(&self.stream, Some(&cursor), limits.page_entries)
            .await
            .map_err(|e| classify_read(e, &self.stream, &cursor))?;
        if page.stream != self.id {
            return Err(Error::StreamReplaced { stream: self.stream.clone() });
        }

        let mut entries = Vec::new();
        let mut previous = cursor;
        let mut through: Option<Cursor> = None;
        let mut newest = self.current();
        let mut relocked = false;
        for entry in &page.entries {
            let seq = entry.seq;
            let stop = match self.check(&previous, entry) {
                Ok(Step::Entry(received)) => {
                    entries.push(received);
                    None
                }
                Ok(Step::Relock(lock, generation)) => {
                    if generation > newest.generation {
                        newest = Current { lock, generation };
                        relocked = true;
                    }
                    None
                }
                Ok(Step::Nothing) => None,
                Err(stop) => Some(stop),
            };
            if let Some(stop) = stop {
                // Everything before it goes to the app; the entry itself
                // waits, and is the next pull's error.
                if through.is_none() {
                    return Err(stop);
                }
                break;
            }
            previous = Cursor { stream: page.stream, seq, hash: entry.hash };
            through = Some(previous);
        }
        if relocked {
            self.adopt(newest).await?;
        }
        Ok(Pulled { entries, through, head: page.head })
    }

    /// What one entry is for the device: an entry for the app, a lock of
    /// the key, nothing, or the reason to stop in front of it.
    fn check(&self, previous: &Cursor, entry: &Entry) -> Result<Step, Error> {
        let stream = || self.stream.clone();
        follows(previous, entry, &self.stream)?;
        if entry.epoch > self.epoch {
            return Err(Error::NewerEpoch { stream: stream(), seq: entry.seq, ours: self.epoch, entry: entry.epoch });
        }
        if lacodda_seal::is_locked_key(&entry.data) {
            return self.check_key_entry(entry);
        }
        if !lacodda_seal::is_sealed(&entry.data) {
            return Err(Error::ForeignEntry { stream: stream(), seq: entry.seq });
        }
        let newer = |version| Error::NewerEntryFormat { stream: stream(), seq: entry.seq, version };
        match lacodda_seal::sealed_key_id(&entry.data) {
            Ok(key) if key != self.key.id() => {
                return Err(Error::UnknownKey { stream: stream(), seq: entry.seq, key });
            }
            Ok(_) => {}
            Err(lacodda_seal::Error::NewerFormat { found, .. }) => return Err(newer(found)),
            Err(e) => return Err(e.into()),
        }
        match envelope::open(&self.key, entry.epoch, &entry.data) {
            Ok(Opened::Entry { device, data }) => Ok(Step::Entry(Received {
                seq: entry.seq,
                epoch: entry.epoch,
                device,
                own: device == self.state.device(),
                data,
            })),
            Ok(Opened::NewerFormat(version)) => Err(newer(version)),
            Ok(Opened::NewerCodec(codec)) => Err(Error::NewerCodec { stream: stream(), seq: entry.seq, codec }),
            Err(lacodda_seal::Error::Inauthentic) => Err(Error::Inauthentic { stream: stream(), seq: entry.seq }),
            Err(e) => Err(e.into()),
        }
    }

    /// What a key entry is for this device: a lock of its key under some
    /// passphrase, or a lock of another key - one a second device wrote while
    /// racing to create the stream, which lost and is nothing for anyone.
    fn check_key_entry(&self, entry: &Entry) -> Result<Step, Error> {
        let read = KeyEntry::from_bytes(&entry.data)?;
        if read.lock.key_id() != self.key.id() {
            return Ok(Step::Nothing);
        }
        // A lock of this key that the key did not seal was not written by a
        // device holding it: taken in, it would stop every device from
        // opening with the passphrase.
        match read.generation(&self.key) {
            Some(generation) => Ok(Step::Relock(read.lock, generation)),
            None => Err(Error::Inauthentic { stream: self.stream.clone(), seq: entry.seq }),
        }
    }

    /// Keeps a newer lock of the key - the passphrase that opens the stream
    /// on this device from now on. The state only ever moves to a newer
    /// generation, and the client takes whatever the state then holds: two
    /// reads racing on one client cannot take it back.
    async fn adopt(&self, newer: Current) -> Result<(), Error> {
        let known = blocking({
            let state = self.state.clone();
            let stream = self.stream.clone();
            let (id, lock, generation) = (self.id, newer.lock.to_bytes(), newer.generation);
            move || {
                state.relock(&stream, id, &lock, generation)?;
                state.known(&stream)
            }
        })
        .await?;
        if let Some(known) = known.filter(|known| known.stream == self.id) {
            *self.lock_current() = Current { lock: LockedKey::from_bytes(&known.lock)?, generation: known.generation };
        }
        Ok(())
    }

    /// Changes the passphrase that unlocks the stream's key, everywhere.
    ///
    /// The key stays the same - nothing in the stream is sealed again. The
    /// key is locked under the new passphrase, with today's Argon2id
    /// parameters, and written to the stream as a key entry; every device
    /// takes it in as it reads past it, this one included, and asks for the
    /// new passphrase from its next start. A device that joins later opens
    /// with the new passphrase.
    ///
    /// The old passphrase is not revoked: its lock stays in the stream, which
    /// only grows. Whoever knows it and holds a copy of the stream can still
    /// unlock the key. A passphrase that leaked needs a new stream; this is
    /// for one that was weak, or locked with weaker parameters than today's.
    ///
    /// Reads the stream from this device's cursor to its head first, for
    /// the newest lock: a change has to follow it to hold.
    ///
    /// # Errors
    ///
    /// [`Error::PassphraseRace`] when another device changed the passphrase
    /// at the same moment and its change came first - the passphrase is the
    /// one it set; [`Error::EpochBehind`], [`Error::StreamGone`] and
    /// [`Error::StreamReplaced`] as for [`Client::push`].
    pub async fn change_passphrase(&self, passphrase: &[u8]) -> Result<(), Error> {
        let read = self.follow_keys(self.cursor().await?, None).await?;
        // Saturates: a change at the last generation holds for good, and one
        // after it loses to it like any change written behind another.
        let generation = self.current().generation.saturating_add(1);
        let lock = blocking({
            let key = self.key.clone();
            let passphrase = passphrase.to_vec();
            move || {
                let lock = LockedKey::lock(&key, &passphrase, KEY_CONTEXT);
                wipe(passphrase);
                lock
            }
        })
        .await?;
        let entry = KeyEntry::new(&self.key, lock.clone(), generation)?;
        let batch = Batch { epoch: self.epoch, entries: vec![entry.to_bytes()], stream: Some(self.id) };
        let written = match self.transport.append(&self.stream, &batch).await {
            Ok(written) if written.stream != self.id => {
                return Err(Error::StreamReplaced { stream: self.stream.clone() });
            }
            Ok(written) => written,
            Err(e) => return Err(write_error(e, &self.stream, self.epoch)),
        };
        // Another device may have changed the passphrase at the same moment.
        // The first change in the stream holds, everywhere: read up to ours
        // to know which one that is.
        self.follow_keys(read, Some(written.last)).await?;
        if self.current().lock == lock { Ok(()) } else { Err(Error::PassphraseRace { stream: self.stream.clone() }) }
    }

    /// Reads the stream from `from` up to position `until`, or to its head,
    /// checking each link, and takes in the newest lock of the key on the
    /// way. Where it stopped.
    async fn follow_keys(&self, from: Cursor, until: Option<u64>) -> Result<Cursor, Error> {
        let mut previous = from;
        let mut newest = self.current();
        let mut relocked = false;
        let limit = self.transport.limits().await?.page_entries;
        scan(&self.transport, &self.stream, Some(from), limit, |id, entry| {
            if until.is_some_and(|until| entry.seq > until) {
                return Ok(ControlFlow::Break(()));
            }
            if id != self.id {
                return Err(Error::StreamReplaced { stream: self.stream.clone() });
            }
            follows(&previous, entry, &self.stream)?;
            if lacodda_seal::is_locked_key(&entry.data)
                && let Step::Relock(lock, generation) = self.check_key_entry(entry)?
                && generation > newest.generation
            {
                newest = Current { lock, generation };
                relocked = true;
            }
            previous = entry.cursor(id);
            Ok(ControlFlow::Continue(()))
        })
        .await?;
        if relocked {
            self.adopt(newest).await?;
        }
        Ok(previous)
    }

    fn current(&self) -> Current {
        self.lock_current().clone()
    }

    fn lock_current(&self) -> MutexGuard<'_, Current> {
        // Nothing panics while holding it; a poisoned lock still holds a
        // whole value.
        self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records that the app has applied everything `pulled` returned: the
    /// device's cursor moves past it, and survives a restart there.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when the state file cannot be written.
    pub async fn ack(&self, pulled: &Pulled) -> Result<(), Error> {
        let Some(cursor) = pulled.through else {
            return Ok(());
        };
        let state = self.state.clone();
        let stream = self.stream.clone();
        blocking(move || state.advance(&stream, &cursor)).await
    }

    /// Waits up to `timeout` - at most the relay's longest wait - for the
    /// stream to move past this device's acknowledged cursor. `true` when it
    /// has: there is something to pull.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the relay cannot be reached.
    pub async fn wait(&self, timeout: Duration) -> Result<bool, Error> {
        let limits = self.transport.limits().await?;
        let cursor = self.cursor().await?;
        let watch = Watch { stream: self.stream.clone(), cursor: Some(cursor) };
        let heads = self.transport.wait(&[watch], timeout.min(limits.wait_max)).await?;
        Ok(!heads.heads.is_empty())
    }

    /// The largest item one push can carry, in bytes.
    pub async fn max_item_len(&self) -> Result<usize, Error> {
        let limits = self.transport.limits().await?;
        Ok(Sizer::new(self.epoch, limits.max_body).max_item())
    }

    /// The stream's key. Whoever holds these bytes can read the stream: keep
    /// them where the app keeps its most private data, or not at all - the
    /// passphrase unlocks the key again.
    pub fn key(&self) -> &Key {
        &self.key
    }

    /// The stream's key as locked under the passphrase - the newest lock
    /// this device has read.
    pub fn locked_key(&self) -> LockedKey {
        self.current().lock
    }

    /// How many times the stream's passphrase has been changed, as far as
    /// this device has read: the generation of [`Client::locked_key`].
    pub fn passphrase_changes(&self) -> u32 {
        self.current().generation
    }

    /// The stream's name.
    pub fn stream(&self) -> &StreamName {
        &self.stream
    }

    /// The incarnation of the stream this device syncs with.
    pub fn stream_id(&self) -> StreamId {
        self.id
    }

    /// The epoch the app writes in, and the newest it reads.
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// This device's identity.
    pub fn device(&self) -> DeviceId {
        self.state.device()
    }

    /// The transport the client talks through.
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// The device's acknowledged cursor.
    pub async fn cursor(&self) -> Result<Cursor, Error> {
        let state = self.state.clone();
        let stream = self.stream.clone();
        let known = blocking(move || state.known(&stream)).await?;
        // The record was written when the client opened; it is gone only if
        // the app forgot the stream since, and then the client is stale.
        known.map(|known| known.cursor).ok_or_else(|| Error::StreamGone { stream: self.stream.clone() })
    }
}

/// Finds the stream's key on the relay, or creates it.
async fn establish<T: Transport>(
    transport: &T,
    stream: &StreamName,
    epoch: Epoch,
    secret: Secret<'_>,
) -> Result<(StreamId, Current, Key), Error> {
    if let Some((first, entry)) = first_key_entry(transport, stream).await? {
        return join(transport, stream, first, entry, secret).await;
    }
    let Secret::Passphrase(passphrase) = secret else {
        return Err(Error::NoStreamKey { stream: stream.clone() });
    };
    let key = Key::generate()?;
    let lock = blocking({
        let key = key.clone();
        let passphrase = passphrase.to_vec();
        move || {
            let lock = LockedKey::lock(&key, &passphrase, KEY_CONTEXT);
            wipe(passphrase);
            lock
        }
    })
    .await?;
    let entry = KeyEntry::new(&key, lock, 0)?;
    let batch = Batch { epoch, entries: vec![entry.to_bytes()], stream: None };
    transport.append(stream, &batch).await.map_err(|e| write_error(e, stream, epoch))?;

    // Another device may have created a key for the same stream at the same
    // moment. The first key entry in the stream is the stream's key, for
    // everyone: read it back, and give up ours if it lost.
    let (first, entry) = first_key_entry(transport, stream)
        .await?
        .ok_or_else(|| not_protocol(transport, "a key was written to the stream and is not there".into()))?;
    if entry.lock.key_id() == key.id() {
        return Ok((first.stream, Current { lock: entry.lock, generation: 0 }, key));
    }
    join(transport, stream, first, entry, Secret::Passphrase(passphrase)).await
}

/// Takes a stream's key with a secret, on a device that has not synced the
/// stream before: from the first key entry, or - when the passphrase was
/// changed since - from a later one.
async fn join<T: Transport>(
    transport: &T,
    stream: &StreamName,
    first: Cursor,
    entry: KeyEntry,
    secret: Secret<'_>,
) -> Result<(StreamId, Current, Key), Error> {
    let generation_of = |entry: &KeyEntry, key: &Key, seq: u64| {
        entry.generation(key).ok_or_else(|| Error::Inauthentic { stream: stream.clone(), seq })
    };
    let passphrase = match secret {
        Secret::Key(key) => {
            let key = take_key(&entry.lock, Secret::Key(key), stream).await?;
            let generation = generation_of(&entry, &key, first.seq)?;
            return Ok((first.stream, Current { lock: entry.lock, generation }, key));
        }
        Secret::Passphrase(passphrase) => passphrase,
    };
    match unlock(&entry.lock, passphrase, stream).await {
        Ok(key) => {
            let generation = generation_of(&entry, &key, first.seq)?;
            return Ok((first.stream, Current { lock: entry.lock, generation }, key));
        }
        Err(Error::WrongPassphrase { .. }) => {}
        Err(e) => return Err(e),
    }

    // The passphrase may have been changed since the stream was created: try
    // the later locks of the same key, the newest first.
    let later = key_entries_after(transport, stream, first, entry.lock.key_id()).await?;
    for (seq, candidate) in later.iter().rev() {
        let key = match unlock(&candidate.lock, passphrase, stream).await {
            Ok(key) => key,
            Err(Error::WrongPassphrase { .. }) => continue,
            Err(e) => return Err(e),
        };
        generation_of(candidate, &key, *seq)?;
        // The lock that opened is not always the one that holds - an older
        // passphrase, or a change that lost a race. The device keeps the one
        // every other device keeps: the first of each newer generation.
        let mut current = Current { lock: entry.lock.clone(), generation: generation_of(&entry, &key, first.seq)? };
        for (seq, later) in &later {
            let generation = generation_of(later, &key, *seq)?;
            if generation > current.generation {
                current = Current { lock: later.lock.clone(), generation };
            }
        }
        return Ok((first.stream, current, key));
    }
    Err(Error::WrongPassphrase { stream: stream.clone() })
}

/// The stream's first key entry and where it is; `None` if there is no
/// stream or no key in it.
async fn first_key_entry<T: Transport>(
    transport: &T,
    stream: &StreamName,
) -> Result<Option<(Cursor, KeyEntry)>, Error> {
    let mut found = None;
    // The key is the first entry of every stream a client created: one entry
    // answers, where a full page would carry megabytes of the stream along.
    let read = scan(transport, stream, None, 1, |id, entry| {
        if lacodda_seal::is_locked_key(&entry.data) {
            found = Some((entry.cursor(id), KeyEntry::from_bytes(&entry.data)?));
            return Ok(ControlFlow::Break(()));
        }
        if !lacodda_seal::is_sealed(&entry.data) {
            return Err(Error::ForeignEntry { stream: stream.clone(), seq: entry.seq });
        }
        Ok(ControlFlow::Continue(()))
    })
    .await?;
    Ok(read.and(found))
}

/// The key entries of `key` after `after`, with their positions, in stream
/// order.
async fn key_entries_after<T: Transport>(
    transport: &T,
    stream: &StreamName,
    after: Cursor,
    key: KeyId,
) -> Result<Vec<(u64, KeyEntry)>, Error> {
    let mut found = Vec::new();
    let limit = transport.limits().await?.page_entries;
    scan(transport, stream, Some(after), limit, |_, entry| {
        if lacodda_seal::is_locked_key(&entry.data) {
            let read = KeyEntry::from_bytes(&entry.data)?;
            if read.lock.key_id() == key {
                found.push((entry.seq, read));
            }
        }
        Ok(ControlFlow::Continue(()))
    })
    .await?;
    Ok(found)
}

/// Reads `stream` page by page after `after`, the first page `limit`
/// entries long, handing each entry and the stream's incarnation to `visit`
/// until it breaks or the stream ends. `None` when there is no such stream.
async fn scan<T: Transport>(
    transport: &T,
    stream: &StreamName,
    mut after: Option<Cursor>,
    mut limit: usize,
    mut visit: impl FnMut(StreamId, &Entry) -> Result<ControlFlow<()>, Error>,
) -> Result<Option<()>, Error> {
    let page_entries = transport.limits().await?.page_entries;
    loop {
        let page = match (transport.read(stream, after.as_ref(), limit).await, &after) {
            (Ok(page), _) => page,
            (Err(e), None) if e.code() == Some(&ProblemCode::StreamNotFound) => return Ok(None),
            (Err(e), None) => return Err(e.into()),
            (Err(e), Some(cursor)) => return Err(classify_read(e, stream, cursor)),
        };
        for entry in &page.entries {
            if visit(page.stream, entry)?.is_break() {
                return Ok(Some(()));
            }
        }
        match page.cursor() {
            Some(cursor) if cursor.seq < page.head => after = Some(cursor),
            _ => return Ok(Some(())),
        }
        limit = page_entries;
    }
}

/// Whether `entry` is the link that follows `previous`.
fn follows(previous: &Cursor, entry: &Entry, stream: &StreamName) -> Result<(), Error> {
    if entry.seq != previous.seq + 1 || chain::link(&previous.hash, entry.seq, entry.epoch, &entry.data) != entry.hash {
        return Err(Error::BrokenChain { stream: stream.clone(), seq: entry.seq });
    }
    Ok(())
}

/// The key a secret gives for a stream locked with `lock`.
async fn take_key(lock: &LockedKey, secret: Secret<'_>, stream: &StreamName) -> Result<Key, Error> {
    match secret {
        Secret::Passphrase(passphrase) => unlock(lock, passphrase, stream).await,
        Secret::Key(key) if key.id() == lock.key_id() => Ok(key),
        Secret::Key(key) => {
            Err(Error::KeyMismatch { stream: stream.clone(), ours: key.id(), stream_key: lock.key_id() })
        }
    }
}

/// Unlocks off the async workers: Argon2id takes a fraction of a second of
/// one core and 64 MiB, on purpose.
async fn unlock(lock: &LockedKey, passphrase: &[u8], stream: &StreamName) -> Result<Key, Error> {
    let lock = lock.clone();
    let passphrase = passphrase.to_vec();
    let unlocked = blocking(move || {
        let key = lock.unlock(&passphrase, KEY_CONTEXT);
        wipe(passphrase);
        key
    })
    .await;
    unlocked.map_err(|e| match e {
        lacodda_seal::Error::WrongPassphrase => Error::WrongPassphrase { stream: stream.clone() },
        other => other.into(),
    })
}

fn wipe(mut bytes: Vec<u8>) {
    bytes.fill(0);
    std::hint::black_box(&bytes);
}

/// A refused read, said as what it means for this device.
pub(crate) fn classify_read(e: TransportError, stream: &StreamName, cursor: &Cursor) -> Error {
    let stream = stream.clone();
    match e.code() {
        Some(ProblemCode::StreamNotFound) => Error::StreamGone { stream },
        Some(ProblemCode::StreamReplaced) => Error::StreamReplaced { stream },
        Some(ProblemCode::CursorAhead) => {
            let head = match &e {
                TransportError::Refused { problem, .. } => problem.head.unwrap_or_default(),
                _ => 0,
            };
            Error::CursorAhead { stream, cursor: cursor.seq, head }
        }
        Some(ProblemCode::CursorDiverged) => Error::CursorDiverged { stream, seq: cursor.seq },
        _ => Error::Transport(e),
    }
}

fn write_error(e: TransportError, stream: &StreamName, ours: Epoch) -> Error {
    let stream = stream.clone();
    match &e {
        TransportError::Refused { problem, .. } => match problem.code {
            ProblemCode::EpochBehind => {
                Error::EpochBehind { stream, ours, current: problem.epoch.unwrap_or(Epoch(ours.0.saturating_add(1))) }
            }
            ProblemCode::StreamNotFound => Error::StreamGone { stream },
            ProblemCode::StreamReplaced => Error::StreamReplaced { stream },
            _ => Error::Transport(e),
        },
        _ => Error::Transport(e),
    }
}

fn not_protocol<T: Transport>(transport: &T, reason: String) -> TransportError {
    TransportError::NotProtocol { at: transport.describe(), reason }
}

/// Runs `work` on the blocking pool: SQLite waits on the disk, Argon2id on
/// the processor, and neither should hold an async worker.
async fn blocking<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> R {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// The size of a batch's CBOR body, entry by entry - so a push is cut into
/// batches that each fit the relay's limit, exactly rather than roughly.
///
/// A batch is `{0: epoch, 1: [entries...], 2: stream}`: a map head, three
/// keys, the epoch, an array head and a byte string head per entry, and the
/// sixteen bytes of the stream's identity.
struct Sizer {
    max_body: usize,
    fixed: usize,
}

impl Sizer {
    fn new(epoch: Epoch, max_body: usize) -> Self {
        let fixed = 1 + 1 + head_len(u64::from(epoch.0)) + 1 + 1 + head_len(16) + 16;
        Self { max_body, fixed }
    }

    /// The body of a batch with no entries.
    fn empty(&self) -> usize {
        self.fixed + head_len(0)
    }

    /// The body after adding an entry of `len` bytes to a batch of `count`
    /// entries whose body is `body`.
    fn grow(&self, body: usize, count: usize, len: usize) -> usize {
        body - head_len(count as u64) + head_len(count as u64 + 1) + head_len(len as u64) + len
    }

    fn fits(&self, body: usize, count: usize, len: usize) -> bool {
        self.grow(body, count, len) <= self.max_body
    }

    /// The largest item that fits in a batch of its own.
    fn max_item(&self) -> usize {
        let room = self.max_body.saturating_sub(self.fixed + head_len(1));
        // The entry and its byte string head share the room; the head's
        // length depends on the entry's, so try each head length.
        [9, 5, 3, 2, 1]
            .into_iter()
            .filter_map(|head| room.checked_sub(head).filter(|entry| head_len(*entry as u64) == head))
            .max()
            .map_or(0, |entry| entry.saturating_sub(ENTRY_OVERHEAD))
    }
}

/// The length of a CBOR head carrying `value`.
fn head_len(value: u64) -> usize {
    match value {
        0..=23 => 1,
        24..=0xff => 2,
        0x100..=0xffff => 3,
        0x1_0000..=0xffff_ffff => 5,
        _ => 9,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use efema_proto::wire;

    #[test]
    fn the_sizer_measures_a_batch_as_the_encoder_writes_it() {
        let stream = Some(StreamId::from_bytes([1; 16]));
        for epoch in [Epoch(0), Epoch(24), Epoch(70_000)] {
            let sizer = Sizer::new(epoch, usize::MAX);
            let mut body = sizer.empty();
            let mut entries: Vec<Vec<u8>> = Vec::new();
            for len in [0, 1, 23, 24, 255, 256, 65_535, 65_536, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3] {
                body = sizer.grow(body, entries.len(), len);
                entries.push(vec![0; len]);
                let encoded = wire::encode(&Batch { epoch, entries: entries.clone(), stream });
                assert_eq!(body, encoded.len(), "{} entries in epoch {epoch}", entries.len());
            }
        }
    }

    #[test]
    fn the_largest_item_fits_and_one_byte_more_does_not() {
        for max_body in [200, 1000, 70_000, 16 * 1024 * 1024] {
            let sizer = Sizer::new(Epoch(1), max_body);
            let item = sizer.max_item();
            let fits = |len: usize| sizer.grow(sizer.empty(), 0, len + ENTRY_OVERHEAD) <= max_body;
            assert!(fits(item), "the largest item does not fit {max_body}");
            assert!(!fits(item + 1), "one byte more still fits {max_body}");
        }
        assert_eq!(Sizer::new(Epoch(1), 10).max_item(), 0);
    }
}
