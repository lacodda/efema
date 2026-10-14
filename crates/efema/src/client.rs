//! A device's client of one stream.

use std::ops::ControlFlow;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use efema_proto::wire::{Batch, Entry, Limits, ProblemCode, Watch};
use efema_proto::{Cursor, Epoch, StreamId, StreamName, chain};
use lacodda_seal::{Key, KeyId, LockedKey};

use crate::envelope::{self, Compression, ENTRY_OVERHEAD, KEY_CONTEXT, KeyEntry, Opened};
use crate::state::{Queue, State, Waiting};
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

/// One device's view of one stream: it seals what it pushes, keeps it until
/// the relay has it, opens what it pulls, and remembers how far it has read.
///
/// ```no_run
/// use efema::{AccessKey, Client, Epoch, Relay, Secret, State};
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let state = State::open("sync.sqlite3")?;
/// // Handed out by the relay's operator: `efema-server keys add`.
/// let key: AccessKey = std::env::var("EFEMA_KEY")?.parse()?;
/// let relay = Relay::new("https://sync.example.com", key)?;
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

/// How old the limits kept in the device's state may get before a send that
/// reached the relay asks for them again: an operator who raised a limit is
/// heard within a day, one who lowered it at the next refusal.
const LIMITS_STALE: Duration = Duration::from_secs(24 * 60 * 60);

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

/// What a [`Client::push`] or a [`Client::send`] took to the relay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sent {
    /// Items now on the relay that were waiting on this device - those of
    /// earlier pushes too, sent first.
    pub items: usize,
    /// The batches they went in.
    pub batches: usize,
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
                // The relay is needed now anyway, and its limits say whether
                // the key writes: a key that only reads cannot create a
                // stream.
                let limits = transport.limits().await?;
                let (id, current, key) = establish(&transport, &stream, epoch, secret, &limits).await?;
                blocking({
                    let state = state.clone();
                    let stream = stream.clone();
                    let lock = current.lock.to_bytes();
                    let generation = current.generation;
                    move || {
                        state.join(&stream, id, &lock, generation)?;
                        state.keep_limits(&stream, id, &limits)
                    }
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

    /// Compresses and seals each item, keeps them on this device, and sends
    /// everything the device has waiting for the stream - these items last.
    ///
    /// The items are kept before anything is sent, in the device's
    /// [`State`]: sealed, and cut into batches that each fit the limits the
    /// relay announced - kept in the state too, so a push needs no network -
    /// each to land whole: all on consecutive positions, or none. Once kept,
    /// they outlive a torn connection, a relay that is down and the app
    /// closing: they wait, and go with the next push or [`Client::send`]. A
    /// batch whose answer was lost goes again as it was, and the relay, which
    /// knows it by what it holds, does not write it twice.
    ///
    /// # Errors
    ///
    /// [`Error::Unsent`] when the items are kept but not all sent: its source
    /// says why - [`Error::Transport`] for a relay that cannot be reached,
    /// worth a [`Client::send`] later; [`Error::EpochBehind`],
    /// [`Error::StreamGone`], [`Error::StreamReplaced`] or
    /// [`Error::QuotaExceeded`] for a stream that will not take them. Before
    /// anything is kept: [`Error::ReadOnly`] when the access key only reads;
    /// [`Error::ItemTooLarge`] when one item cannot fit what the relay takes
    /// uncompressed - whether it compresses does not change the limit - and
    /// [`Error::StreamGone`] when the app forgot the stream since this client
    /// opened it.
    pub async fn push<I>(&self, items: I) -> Result<Sent, Error>
    where
        I: IntoIterator,
        I::Item: AsRef<[u8]>,
    {
        let sizer = Sizer::new(self.epoch, &self.limits_to_write().await?);
        let mut entries: Vec<Vec<u8>> = Vec::new();
        for item in items {
            let item = item.as_ref();
            if item.len() > sizer.max_item() {
                return Err(Error::ItemTooLarge { size: item.len(), limit: sizer.max_item() });
            }
            entries.push(envelope::seal(&self.key, self.epoch, self.state.device(), item, self.compression)?);
        }
        if !entries.is_empty() {
            let counts = sizer.cut(&entries);
            let mut entries = entries.into_iter();
            let batches: Vec<Vec<Vec<u8>>> =
                counts.into_iter().map(|count| entries.by_ref().take(count).collect()).collect();
            blocking({
                let state = self.state.clone();
                let (stream, id, epoch) = (self.stream.clone(), self.id, self.epoch);
                move || state.queue(&stream, id, epoch, &batches)
            })
            .await?;
        }
        self.send().await
    }

    /// Sends what waits on this device for the stream, oldest first, until
    /// nothing does - after a push that could not send, when the network is
    /// back, or when the app starts.
    ///
    /// Each batch leaves the device once the relay has it; a batch whose
    /// answer was lost stays, and goes again as it was. When the relay
    /// refuses a batch as too large - its operator lowered a limit since the
    /// batch was cut - the client asks for the limits again and cuts
    /// everything waiting to them, in order, then goes on: the relay does not
    /// have a batch it refused, and none behind it was sent yet.
    ///
    /// # Errors
    ///
    /// [`Error::Unsent`] when something still waits, with the reason as its
    /// source, as for [`Client::push`] - [`Error::ItemTooLarge`] for an item
    /// waiting that is larger than what the relay takes now; only forgetting
    /// the stream ([`State::forget`]) clears that.
    pub async fn send(&self) -> Result<Sent, Error> {
        let turn = self.state.sending(&self.stream);
        let _turn = turn.lock().await;
        let mut sent = Sent::default();
        // Each batch at most once per send, in the order queued: a batch
        // that did not leave the queue waits for the next send rather than
        // holding this one in a loop.
        let mut after = 0;
        // And the queue cut again at most once: a relay that refuses what
        // was cut to its own limits is not argued with.
        let mut cut_again = false;
        let stopped = loop {
            let waiting = blocking({
                let state = self.state.clone();
                let (stream, id) = (self.stream.clone(), self.id);
                move || state.next_unsent(&stream, id, after)
            })
            .await;
            let result = match waiting {
                Ok(Some(waiting)) => {
                    after = waiting.id;
                    self.send_batch(waiting).await
                }
                Ok(None) => {
                    if sent.batches > 0 {
                        self.refresh_stale_limits().await;
                    }
                    return Ok(sent);
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(items) => {
                    sent.items += items;
                    sent.batches += 1;
                }
                Err(e) if refused_as_too_large(&e) && !cut_again => {
                    cut_again = true;
                    match self.cut_again().await {
                        // Cut again, the queue starts over with new batches.
                        Ok(()) => after = 0,
                        Err(e) => break e,
                    }
                }
                Err(e) => break e,
            }
        };
        match self.queue().await {
            Ok(queue) if !queue.is_empty() => Err(Error::Unsent { unsent: queue.items, source: Box::new(stopped) }),
            _ => Err(stopped),
        }
    }

    /// Sends one queued batch, and takes it out of the queue once the relay
    /// has it. How many items it carried.
    async fn send_batch(&self, waiting: Waiting) -> Result<usize, Error> {
        let count = waiting.entries.len();
        let batch = Batch { epoch: waiting.epoch, entries: waiting.entries, stream: Some(self.id) };
        match self.transport.append(&self.stream, &batch).await {
            Ok(written) if written.stream != self.id => {
                return Err(Error::StreamReplaced { stream: self.stream.clone() });
            }
            Ok(written) if written.last.checked_sub(written.first) != Some(count as u64 - 1) => {
                return Err(Error::Transport(not_protocol(
                    &self.transport,
                    format!("{count} entries were sent and {}..{} came back", written.first, written.last),
                )));
            }
            Ok(_) => {}
            Err(e) => return Err(write_error(e, &self.stream, waiting.epoch)),
        }
        let state = self.state.clone();
        blocking(move || state.sent(waiting.id)).await?;
        Ok(count)
    }

    /// Cuts everything waiting for the stream again, to the limits the relay
    /// announces now.
    async fn cut_again(&self) -> Result<(), Error> {
        let limits = self.refresh_limits().await?;
        let state = self.state.clone();
        let (stream, id) = (self.stream.clone(), self.id);
        blocking(move || {
            state.recut(&stream, id, |epoch, entries| {
                let sizer = Sizer::new(epoch, &limits);
                // A sealed entry is cut no further: one larger than the
                // relay takes now waits, and says so.
                if let Some(entry) = entries.iter().find(|entry| entry.len() > sizer.entry_room()) {
                    return Err(Error::ItemTooLarge { size: entry.len(), limit: sizer.entry_room() });
                }
                Ok(sizer.cut(entries))
            })
        })
        .await
    }

    /// The limits the relay last announced for this device's key, as the
    /// device's state keeps them; the protocol's defaults if the relay was
    /// never asked. No network.
    async fn limits(&self) -> Result<Limits, Error> {
        let state = self.state.clone();
        let stream = self.stream.clone();
        let kept = blocking(move || state.limits(&stream)).await?;
        Ok(kept.map_or(Limits::DEFAULT, |(limits, _)| limits))
    }

    /// Asks the relay for its limits, and keeps the answer.
    pub(crate) async fn refresh_limits(&self) -> Result<Limits, Error> {
        let limits = self.transport.limits().await?;
        let state = self.state.clone();
        let (stream, id) = (self.stream.clone(), self.id);
        blocking(move || state.keep_limits(&stream, id, &limits)).await?;
        Ok(limits)
    }

    /// Asks again for limits older than [`LIMITS_STALE`], or never asked
    /// for. Only after a send that reached the relay, so a device without
    /// the network does not pay for it; a failure keeps what was kept.
    async fn refresh_stale_limits(&self) {
        let state = self.state.clone();
        let stream = self.stream.clone();
        let stale = match blocking(move || state.limits(&stream)).await {
            Ok(Some((_, at))) => SystemTime::now().duration_since(at).is_ok_and(|age| age > LIMITS_STALE),
            Ok(None) => true,
            Err(_) => false,
        };
        if stale {
            let _ = self.refresh_limits().await;
        }
    }

    /// The limits to write with - or [`Error::ReadOnly`] when the key only
    /// reads. A kept answer that said so is checked with the relay first:
    /// the operator may have given the key more since.
    async fn limits_to_write(&self) -> Result<Limits, Error> {
        let limits = self.limits().await?;
        if limits.write {
            return Ok(limits);
        }
        match self.refresh_limits().await {
            Ok(fresh) if fresh.write => Ok(fresh),
            Err(e @ Error::Unauthorized { .. }) => Err(e),
            _ => Err(Error::ReadOnly { stream: self.stream.clone() }),
        }
    }

    /// What waits on this device to be sent to the stream.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when the state file cannot be read.
    pub async fn queue(&self) -> Result<Queue, Error> {
        let state = self.state.clone();
        let stream = self.stream.clone();
        blocking(move || state.queued(&stream)).await
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
        let limits = self.limits().await?;
        let cursor = self.cursor().await?;
        let page = self
            .transport
            .read(&self.stream, Some(&cursor), page_entries(&limits))
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
        let limit = page_entries(&self.limits().await?);
        scan(&self.transport, &self.stream, Some(from), limit, limit, |id, entry| {
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
        let limits = self.limits().await?;
        let cursor = self.cursor().await?;
        let watch = Watch { stream: self.stream.clone(), cursor: Some(cursor) };
        let heads = self.transport.wait(&[watch], timeout.min(Duration::from_secs(limits.wait_max))).await?;
        Ok(!heads.heads.is_empty())
    }

    /// The largest item one push can carry, in bytes, by the limits the
    /// relay last announced. No network.
    pub async fn max_item_len(&self) -> Result<usize, Error> {
        Ok(Sizer::new(self.epoch, &self.limits().await?).max_item())
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
    limits: &Limits,
) -> Result<(StreamId, Current, Key), Error> {
    let page = page_entries(limits);
    if let Some((first, entry)) = first_key_entry(transport, stream, page).await? {
        return join(transport, stream, first, entry, secret, page).await;
    }
    if !limits.write {
        return Err(Error::ReadOnly { stream: stream.clone() });
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
    let (first, entry) = first_key_entry(transport, stream, page)
        .await?
        .ok_or_else(|| not_protocol(transport, "a key was written to the stream and is not there".into()))?;
    if entry.lock.key_id() == key.id() {
        return Ok((first.stream, Current { lock: entry.lock, generation: 0 }, key));
    }
    join(transport, stream, first, entry, Secret::Passphrase(passphrase), page).await
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
    page: usize,
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
    let later = key_entries_after(transport, stream, first, entry.lock.key_id(), page).await?;
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
    page: usize,
) -> Result<Option<(Cursor, KeyEntry)>, Error> {
    let mut found = None;
    // The key is the first entry of every stream a client created: one entry
    // answers, where a full page would carry megabytes of the stream along.
    let read = scan(transport, stream, None, 1, page, |id, entry| {
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
    page: usize,
) -> Result<Vec<(u64, KeyEntry)>, Error> {
    let mut found = Vec::new();
    scan(transport, stream, Some(after), page, page, |_, entry| {
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
/// entries long and the rest `page_entries`, handing each entry and the
/// stream's incarnation to `visit` until it breaks or the stream ends. `None`
/// when there is no such stream.
async fn scan<T: Transport>(
    transport: &T,
    stream: &StreamName,
    mut after: Option<Cursor>,
    mut limit: usize,
    page_entries: usize,
    mut visit: impl FnMut(StreamId, &Entry) -> Result<ControlFlow<()>, Error>,
) -> Result<Option<()>, Error> {
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
        _ => Error::from(e),
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
            ProblemCode::Forbidden => Error::ReadOnly { stream },
            ProblemCode::QuotaExceeded => Error::QuotaExceeded { stream, quota: problem.limit.unwrap_or_default() },
            _ => Error::from(e),
        },
        _ => Error::from(e),
    }
}

/// Whether the relay refused a batch for its size - which means it does not
/// have the batch.
fn refused_as_too_large(e: &Error) -> bool {
    matches!(e, Error::Transport(e) if e.code() == Some(&ProblemCode::TooLarge))
}

/// Entries in a page, as the relay announced it.
fn page_entries(limits: &Limits) -> usize {
    usize::try_from(limits.page_entries).unwrap_or(usize::MAX).max(1)
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
/// batches that each fit the relay's limits, exactly rather than roughly.
///
/// A batch is `{0: epoch, 1: [entries...], 2: stream}`: a map head, three
/// keys, the epoch, an array head and a byte string head per entry, and the
/// sixteen bytes of the stream's identity.
struct Sizer {
    max_body: usize,
    max_entry: usize,
    fixed: usize,
}

impl Sizer {
    fn new(epoch: Epoch, limits: &Limits) -> Self {
        let fixed = 1 + 1 + head_len(u64::from(epoch.0)) + 1 + 1 + head_len(16) + 16;
        let bytes = |limit: u64| usize::try_from(limit).unwrap_or(usize::MAX);
        Self { max_body: bytes(limits.body), max_entry: bytes(limits.entry), fixed }
    }

    /// Batch sizes, in entries, for `entries` in order: each batch as full as
    /// the body limit lets it be. Every entry fits on its own.
    fn cut(&self, entries: &[Vec<u8>]) -> Vec<usize> {
        let mut counts: Vec<usize> = Vec::new();
        let mut body = 0;
        for entry in entries {
            match counts.last_mut() {
                Some(count) if self.fits(body, *count, entry.len()) => {
                    body = self.grow(body, *count, entry.len());
                    *count += 1;
                }
                _ => {
                    body = self.grow(self.empty(), 0, entry.len());
                    counts.push(1);
                }
            }
        }
        counts
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

    /// The largest sealed entry the relay takes: no larger than its entry
    /// limit, and in a batch of its own no larger than its body limit.
    fn entry_room(&self) -> usize {
        let room = self.max_body.saturating_sub(self.fixed + head_len(1));
        // The entry and its byte string head share the room; the head's
        // length depends on the entry's, so try each head length.
        let in_a_body = [9, 5, 3, 2, 1]
            .into_iter()
            .filter_map(|head| room.checked_sub(head).filter(|entry| head_len(*entry as u64) == head))
            .max()
            .unwrap_or(0);
        in_a_body.min(self.max_entry)
    }

    /// The largest item that, sealed, the relay takes.
    fn max_item(&self) -> usize {
        self.entry_room().saturating_sub(ENTRY_OVERHEAD)
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

    fn body(max_body: u64) -> Limits {
        Limits { body: max_body, entry: u64::MAX, ..Limits::DEFAULT }
    }

    #[test]
    fn the_sizer_measures_a_batch_as_the_encoder_writes_it() {
        let stream = Some(StreamId::from_bytes([1; 16]));
        for epoch in [Epoch(0), Epoch(24), Epoch(70_000)] {
            let sizer = Sizer::new(epoch, &body(u64::MAX));
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
            let sizer = Sizer::new(Epoch(1), &body(max_body as u64));
            let item = sizer.max_item();
            let fits = |len: usize| sizer.grow(sizer.empty(), 0, len + ENTRY_OVERHEAD) <= max_body;
            assert!(fits(item), "the largest item does not fit {max_body}");
            assert!(!fits(item + 1), "one byte more still fits {max_body}");
        }
        assert_eq!(Sizer::new(Epoch(1), &body(10)).max_item(), 0);
        // The entry limit, when it is the smaller one.
        let limits = Limits { body: 1 << 20, entry: 1000, ..Limits::DEFAULT };
        assert_eq!(Sizer::new(Epoch(1), &limits).max_item(), 1000 - ENTRY_OVERHEAD);
    }

    #[test]
    fn a_cut_fills_each_batch_to_the_body_and_keeps_the_order() {
        let sizer = Sizer::new(Epoch(1), &body(300));
        let entries: Vec<Vec<u8>> = [100, 100, 100, 250, 1, 1].iter().map(|len| vec![7u8; *len]).collect();
        let counts = sizer.cut(&entries);
        assert_eq!(counts.iter().sum::<usize>(), entries.len());
        assert_eq!(counts, [2, 1, 3]);
        let mut at = 0;
        for count in counts {
            let batch = &entries[at..at + count];
            let encoded = wire::encode(&Batch {
                epoch: Epoch(1),
                entries: batch.to_vec(),
                stream: Some(StreamId::from_bytes([1; 16])),
            });
            assert!(encoded.len() <= 300, "a batch of {} bytes", encoded.len());
            at += count;
        }
        assert!(sizer.cut(&[]).is_empty());
    }
}
