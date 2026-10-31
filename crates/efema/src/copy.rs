//! Moving a stream: every batch of it, from one transport to another, at the
//! positions it had.
//!
//! A copy keeps what a device holds on to. The stream's identity, each
//! entry's position and epoch - so every link of the chain, and with it every
//! cursor - and each batch as it was written, so a batch still waiting on a
//! device, whose answer was lost on the way back from the old place, is
//! known at the new one and not written twice. Nothing is opened: the
//! entries go sealed, byte for byte, and a copy needs no key of the stream.

use efema_proto::wire::{Batch, Entry, Problem, ProblemCode};
use efema_proto::{Cursor, Epoch, Hash, StreamId, StreamName, chain, limits};

use crate::transport::{Transport, TransportError};

/// What a copy did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Copied {
    /// Where the copy and the stream it copies stand together: the last
    /// entry of the stream as the copy read it, which the target now has
    /// with the same link - or the cursor before the first entry of a
    /// stream with none.
    pub cursor: Cursor,
    /// Batches this copy wrote. Those the target had already - from an
    /// earlier copy that stopped half way - are not counted.
    pub batches: u64,
    /// The entries in them.
    pub entries: u64,
}

/// Why a copy stopped. What it wrote stays: the next copy of the same stream
/// goes on from there.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CopyError {
    /// The stream could not be read where it is.
    #[error("reading stream {stream}")]
    Source {
        /// The stream.
        stream: StreamName,
        /// What the transport said.
        #[source]
        source: TransportError,
    },
    /// The copy could not be written where it goes - a relay that refused
    /// (with the reason: an access key that only reads, a batch larger than
    /// it takes, a full space, too many requests), or one that could not be
    /// reached.
    #[error("writing stream {stream}")]
    Target {
        /// The stream.
        stream: StreamName,
        /// What the transport said.
        #[source]
        source: TransportError,
    },
    /// Where the copy goes, the name holds another stream.
    #[error("the target has another stream named {stream}: {theirs}, and the stream copied is {ours}")]
    Taken {
        /// The stream's name.
        stream: StreamName,
        /// The identity of the stream copied.
        ours: StreamId,
        /// The identity of the one at the target.
        theirs: StreamId,
    },
    /// The target has the stream, and has another history from `seq` on:
    /// something other than this copy was written there.
    #[error("stream {stream} at the target has another history than the one copied, from position {seq}")]
    Parted {
        /// The stream.
        stream: StreamName,
        /// The first position the two do not share.
        seq: u64,
    },
    /// The stream's pages do not say where its batches end - a relay older
    /// than this release - or say it of entries of two epochs at once.
    #[error("the source does not say where the batches of stream {stream} end: update it to efema 0.9 or later")]
    Unmarked {
        /// The stream.
        stream: StreamName,
    },
    /// The target does not make a stream for a copy: a relay older than
    /// this release, which takes no batch for a stream it does not have.
    #[error("the target does not take a copy of stream {stream}: update it to efema 0.9 or later")]
    OldTarget {
        /// The stream.
        stream: StreamName,
    },
    /// The stream as read does not hold together: an entry's link does not
    /// follow from the one before it, or a position is missing.
    #[error("stream {stream}: the entry at position {seq} does not follow from the one before it")]
    BrokenChain {
        /// The stream.
        stream: StreamName,
        /// The entry's position.
        seq: u64,
    },
}

/// Copies `stream` from `from` to `to`, batch by batch, each at the position
/// it has in `from`: the stream's identity, entries, epochs and batches stay
/// as they are, so every device's cursor and every batch waiting on a device
/// hold at the copy. The stream is read up to its last entry when the copy
/// gets there; what is written to it after that, a second copy brings over.
///
/// A copy that stopped - a torn connection, a relay that wants fewer
/// requests a minute - is run again: it reads the stream from the start,
/// writes only what the target does not have, and checks that what the
/// target has is this stream's history.
///
/// ```no_run
/// # async fn move_it(relay: efema::Relay, bucket: efema::Bucket) -> Result<(), efema::CopyError> {
/// let stream: efema::StreamName = "notes".parse().expect("a stream's name");
/// let copied = efema::copy(&relay, &bucket, &stream).await?;
/// println!("{} batches over, the two agree up to {}", copied.batches, copied.cursor.seq);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// [`CopyError`]: the source or the target failed or refused, the target
/// holds another stream under the name or another history of this one, or
/// the source does not say where its batches end.
pub async fn copy<F: Transport, T: Transport>(from: &F, to: &T, stream: &StreamName) -> Result<Copied, CopyError> {
    let source = |source| CopyError::Source { stream: stream.clone(), source };
    let target = |source| CopyError::Target { stream: stream.clone(), source };
    let parted = |seq| CopyError::Parted { stream: stream.clone(), seq };

    let limit = page_entries(from.limits().await.map_err(source)?.page_entries);
    if !to.limits().await.map_err(target)?.write {
        return Err(target(TransportError::Refused {
            at: to.describe(),
            problem: Box::new(Problem {
                code: ProblemCode::Forbidden,
                message: "the target's credentials read and do not write".into(),
                head: None,
                epoch: None,
                limit: None,
                retry: None,
            }),
        }));
    }

    let mut page = from.read(stream, None, limit).await.map_err(source)?;
    let id = page.stream;
    // How far the target has the stream already, if at all.
    let there = match to.read(stream, None, 1).await {
        Ok(theirs) if theirs.stream != id => {
            return Err(CopyError::Taken { stream: stream.clone(), ours: id, theirs: theirs.stream });
        }
        Ok(theirs) => theirs.head,
        Err(e) if e.code() == Some(&ProblemCode::StreamNotFound) => 0,
        Err(e) => return Err(target(e)),
    };

    let mut copied = Copied { cursor: Cursor::start(id), batches: 0, entries: 0 };
    // The batch being read, and the bytes of its entries: more than any
    // relay takes in one request, and its end was not said.
    let mut batch: Vec<Entry> = Vec::new();
    let mut held = 0;
    loop {
        for entry in std::mem::take(&mut page.entries) {
            if entry.seq != copied.cursor.seq + batch.len() as u64 + 1 {
                return Err(CopyError::BrokenChain { stream: stream.clone(), seq: copied.cursor.seq + 1 });
            }
            let ends = entry.ends_batch;
            held += entry.data.len();
            batch.push(entry);
            if !ends {
                if held > limits::MAX_BODY_BYTES {
                    return Err(CopyError::Unmarked { stream: stream.clone() });
                }
                continue;
            }
            held = 0;
            let entries = std::mem::take(&mut batch);
            let end = links(stream, copied.cursor.hash, &entries)?;
            let (first, last) = (entries[0].seq, entries[entries.len() - 1].seq);
            if last <= there {
                // The target has it: checked once, where the target ends.
                if last == there {
                    aligned(to, stream, Cursor { stream: id, seq: last, hash: end }).await?;
                }
            } else if first <= there {
                // The target ends inside a batch of the stream: it was
                // written in batches of its own.
                return Err(parted(first));
            } else {
                let count = entries.len() as u64;
                let batch = Batch {
                    epoch: entries[0].epoch,
                    entries: entries.into_iter().map(|entry| entry.data).collect(),
                    stream: Some(id),
                    first: Some(first),
                };
                match to.append(stream, &batch).await {
                    Ok(written) if written.stream == id && (written.first, written.last) == (first, last) => {
                        if !written.repeated {
                            copied.batches += 1;
                            copied.entries += count;
                        }
                    }
                    Ok(_) => return Err(parted(first)),
                    // Someone else is writing to the target.
                    Err(e) if e.code() == Some(&ProblemCode::OutOfPlace) => return Err(parted(first)),
                    // A copy's first batch, refused for naming a stream the
                    // target does not have: it does not know the place.
                    Err(e) if first == 1 && e.code() == Some(&ProblemCode::StreamNotFound) => {
                        return Err(CopyError::OldTarget { stream: stream.clone() });
                    }
                    Err(e) => return Err(target(e)),
                }
            }
            copied.cursor = Cursor { stream: id, seq: last, hash: end };
        }
        if !batch.is_empty() && batch[batch.len() - 1].seq >= page.head {
            // The stream's last entry, and no batch ends at it.
            return Err(CopyError::Unmarked { stream: stream.clone() });
        }
        if copied.cursor.seq + batch.len() as u64 >= page.head {
            break;
        }
        let after = match batch.last() {
            Some(entry) => {
                let hash = links(stream, copied.cursor.hash, &batch[..])?;
                Cursor { stream: id, seq: entry.seq, hash }
            }
            None => copied.cursor,
        };
        page = from.read(stream, Some(&after), limit).await.map_err(source)?;
        if page.stream != id {
            return Err(source(TransportError::NotProtocol {
                at: from.describe(),
                reason: format!("stream {stream} turned into stream {} while it was copied", page.stream),
            }));
        }
    }

    // The target has more than the stream had: it must be the stream's
    // history that it goes on from.
    if there > copied.cursor.seq {
        aligned(to, stream, copied.cursor).await?;
    }
    // And what was written is the stream's history, link for link.
    if copied.batches > 0 {
        aligned(to, stream, copied.cursor).await?;
    }
    Ok(copied)
}

/// Checks the links of a batch's entries from `previous`, and returns the
/// last.
fn links(stream: &StreamName, previous: Hash, entries: &[Entry]) -> Result<Hash, CopyError> {
    let epoch: Epoch = entries[0].epoch;
    let mut link = previous;
    for entry in entries {
        if entry.epoch != epoch {
            return Err(CopyError::Unmarked { stream: stream.clone() });
        }
        link = chain::link(&link, entry.seq, entry.epoch, &entry.data);
        if link != entry.hash {
            return Err(CopyError::BrokenChain { stream: stream.clone(), seq: entry.seq });
        }
    }
    Ok(link)
}

/// Whether the target's history has the link `cursor` holds at its
/// position: a read after the cursor is refused where it does not.
async fn aligned<T: Transport>(to: &T, stream: &StreamName, cursor: Cursor) -> Result<(), CopyError> {
    match to.read(stream, Some(&cursor), 1).await {
        Ok(_) => Ok(()),
        Err(e) if e.code() == Some(&ProblemCode::CursorDiverged) => {
            Err(CopyError::Parted { stream: stream.clone(), seq: cursor.seq })
        }
        Err(source) => Err(CopyError::Target { stream: stream.clone(), source }),
    }
}

fn page_entries(announced: u64) -> usize {
    usize::try_from(announced).unwrap_or(usize::MAX).max(1)
}
