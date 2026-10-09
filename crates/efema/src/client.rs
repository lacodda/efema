//! A device's client of one stream.

use std::time::Duration;

use efema_proto::wire::{Batch, ProblemCode, Watch};
use efema_proto::{Cursor, Epoch, StreamId, StreamName, chain};
use lacodda_seal::{Key, LockedKey};

use crate::envelope::{self, ENTRY_OVERHEAD, KEY_CONTEXT, Opened};
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
    lock: LockedKey,
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
        let (id, lock, key) = match known {
            Some(known) => {
                let lock = LockedKey::from_bytes(&known.lock)?;
                let key = take_key(&lock, secret, &stream).await?;
                (known.stream, lock, key)
            }
            None => {
                let (id, lock, key) = establish(&transport, &stream, epoch, secret).await?;
                blocking({
                    let state = state.clone();
                    let stream = stream.clone();
                    let lock = lock.to_bytes();
                    move || state.join(&stream, id, &lock)
                })
                .await?;
                (id, lock, key)
            }
        };
        Ok(Self { transport, state: state.clone(), stream, id, epoch, key, lock })
    }

    /// Seals each item and appends them to the stream, in order.
    ///
    /// Items that fit in one request go as one batch - all on consecutive
    /// positions, or none. More than that is cut into several batches, each
    /// whole on its own; if one fails, [`Error::Interrupted`] says how many
    /// items made it.
    ///
    /// # Errors
    ///
    /// [`Error::ItemTooLarge`] before anything is sent, when one item cannot
    /// fit in a request; [`Error::EpochBehind`] when a newer version of the
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
            let entry = envelope::seal(&self.key, self.epoch, self.state.device(), item)?;
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
        for entry in &page.entries {
            let seq = entry.seq;
            let stop = match self.check(&previous, entry) {
                Ok(Some(received)) => {
                    entries.push(received);
                    None
                }
                Ok(None) => None,
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
        Ok(Pulled { entries, through, head: page.head })
    }

    /// What one entry is for the app: an entry, nothing (a key), or the
    /// reason to stop in front of it.
    fn check(&self, previous: &Cursor, entry: &efema_proto::wire::Entry) -> Result<Option<Received>, Error> {
        let stream = || self.stream.clone();
        if entry.seq != previous.seq + 1
            || chain::link(&previous.hash, entry.seq, entry.epoch, &entry.data) != entry.hash
        {
            return Err(Error::BrokenChain { stream: stream(), seq: entry.seq });
        }
        if entry.epoch > self.epoch {
            return Err(Error::NewerEpoch { stream: stream(), seq: entry.seq, ours: self.epoch, entry: entry.epoch });
        }
        if lacodda_seal::is_locked_key(&entry.data) {
            // The stream's key, or a lock a second device wrote while racing
            // to create it: nothing for the app either way.
            return Ok(None);
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
            Ok(Opened::Entry { device, data }) => Ok(Some(Received {
                seq: entry.seq,
                epoch: entry.epoch,
                device,
                own: device == self.state.device(),
                data,
            })),
            Ok(Opened::Newer(version)) => Err(newer(version)),
            Err(lacodda_seal::Error::Inauthentic) => Err(Error::Inauthentic { stream: stream(), seq: entry.seq }),
            Err(e) => Err(e.into()),
        }
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

    /// The stream's key as locked under the passphrase.
    pub fn locked_key(&self) -> &LockedKey {
        &self.lock
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
) -> Result<(StreamId, LockedKey, Key), Error> {
    if let Some((id, lock)) = first_lock(transport, stream).await? {
        let key = take_key(&lock, secret, stream).await?;
        return Ok((id, lock, key));
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
    let batch = Batch { epoch, entries: vec![lock.to_bytes()], stream: None };
    transport.append(stream, &batch).await.map_err(|e| write_error(e, stream, epoch))?;

    // Another device may have created a key for the same stream at the same
    // moment. The first lock in the stream is the stream's key, for everyone:
    // read it back, and give up ours if it lost.
    let (id, first) = first_lock(transport, stream)
        .await?
        .ok_or_else(|| not_protocol(transport, "a key was written to the stream and is not there".into()))?;
    if first.key_id() == key.id() {
        return Ok((id, first, key));
    }
    let theirs = unlock(&first, passphrase, stream).await?;
    Ok((id, first, theirs))
}

/// The first lock in the stream, with the stream's incarnation; `None` if
/// there is no stream or no lock in it.
async fn first_lock<T: Transport>(transport: &T, stream: &StreamName) -> Result<Option<(StreamId, LockedKey)>, Error> {
    let limits = transport.limits().await?;
    let mut after: Option<Cursor> = None;
    // The lock is the first entry of every stream a client created: one entry
    // answers, where a full page would carry megabytes of the stream along.
    let mut limit = 1;
    loop {
        let page = match transport.read(stream, after.as_ref(), limit).await {
            Ok(page) => page,
            Err(e) if e.code() == Some(&ProblemCode::StreamNotFound) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        for entry in &page.entries {
            if lacodda_seal::is_locked_key(&entry.data) {
                return Ok(Some((page.stream, LockedKey::from_bytes(&entry.data)?)));
            }
            if !lacodda_seal::is_sealed(&entry.data) {
                return Err(Error::ForeignEntry { stream: stream.clone(), seq: entry.seq });
            }
        }
        match page.cursor() {
            Some(cursor) if cursor.seq < page.head => after = Some(cursor),
            _ => return Ok(None),
        }
        limit = limits.page_entries;
    }
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
