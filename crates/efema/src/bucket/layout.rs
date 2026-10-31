//! Where a stream lives in a bucket, object by object, and what each object
//! holds. The keys are the bucket's own, or under a prefix the bucket was
//! given - `apps/notes/` before each.
//!
//! ```text
//! .efema-probe                          efema's check that the bucket refuses to create an object twice
//! <stream>/stream                       the stream's incarnation: its identity, created once
//! <stream>/<id>/b/<first, from nines>   a batch, under the position of its first entry
//! <stream>/<id>/o/<batch key>/<first>   an offer: the batch was put at that position
//! ```
//!
//! A batch is created under its first position only if no object is there
//! yet (`If-None-Match: *`), so two batches can never take one place, and the
//! next place is always the one after the last batch's last entry: a stream
//! in a bucket has no gaps and no batch in half, with no transaction and no
//! one in between. The position's digits are written from nine - each digit
//! `d` as `9 - d` - so a listing, which goes in the order of the keys, starts
//! from the newest batch: the newest one, or the one that holds any
//! position, is the first key of one listing.
//!
//! An offer is written before every attempt to put a batch in a place. A
//! batch can only be where one of its offers says, so a batch sent again -
//! its answer lost on the way back - is found by its key, and not written
//! twice.

use efema_proto::wire::{Batch, Entry};
use efema_proto::{Epoch, Hash, StreamId, StreamName, chain};
use minicbor::{Decode, Encode};

/// The object efema creates twice to see the second creation refused.
pub(crate) const PROBE: &str = ".efema-probe";

/// What the probe holds, for whoever comes across it in the bucket.
pub(crate) const PROBE_TEXT: &str = "efema creates this object again from time to time, to check that the bucket refuses to: \
     the order of every stream here stands on that.\n";

/// The layout this release writes and reads. A stream written in another is
/// not read as this one.
pub(crate) const FORMAT: u32 = 1;

/// The digits of a position in a key: as many as the largest one has.
const DIGITS: usize = 20;

/// The stream's record.
pub(crate) fn record(stream: &StreamName) -> String {
    format!("{stream}/stream")
}

/// Where the batches of an incarnation are.
pub(crate) fn batches(stream: &StreamName, id: StreamId) -> String {
    format!("{stream}/{id}/b/")
}

/// The batch whose first entry is at `first`.
pub(crate) fn batch(stream: &StreamName, id: StreamId, first: u64) -> String {
    format!("{}{}", batches(stream, id), from_nines(first))
}

/// The position a batch's key puts its first entry at.
pub(crate) fn first_of(key: &str, batches: &str) -> Option<u64> {
    let digits = key.strip_prefix(batches)?;
    if digits.len() != DIGITS || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Twenty nines would be a position past the largest: it does not parse.
    let position: String = digits.bytes().map(|b| (b'9' - b + b'0') as char).collect();
    position.parse().ok()
}

/// Where the offers of the batch with `key` are.
pub(crate) fn offers(stream: &StreamName, id: StreamId, key: &Hash) -> String {
    format!("{stream}/{id}/o/{key}/")
}

/// An offer of the batch with `key` at `first`.
pub(crate) fn offer(stream: &StreamName, id: StreamId, key: &Hash, first: u64) -> String {
    format!("{}{first:0DIGITS$}", offers(stream, id, key))
}

/// The position an offer names.
pub(crate) fn offered_at(key: &str, offers: &str) -> Option<u64> {
    let digits = key.strip_prefix(offers)?;
    if digits.len() != DIGITS || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn from_nines(position: u64) -> String {
    format!("{position:0DIGITS$}").bytes().map(|b| (b'9' - b + b'0') as char).collect()
}

/// A stream's record: which incarnation the name is, in which layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub(crate) struct Record {
    #[n(0)]
    pub(crate) id: StreamId,
    #[n(1)]
    pub(crate) format: u32,
}

/// A batch as a bucket keeps it: where it starts, the link before it, and
/// the batch as its writer sent it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub(crate) struct Stored {
    #[n(0)]
    pub(crate) first: u64,
    #[n(1)]
    pub(crate) previous: Hash,
    #[n(2)]
    pub(crate) batch: Batch,
}

impl Stored {
    /// The batch's entries from position `from` on, with their positions
    /// and links. The links before `from` are computed, the entries there
    /// not copied.
    pub(crate) fn entries_from(&self, from: u64) -> impl Iterator<Item = Entry> + '_ {
        let epoch = self.batch.epoch;
        let last = self.first + self.batch.entries.len() as u64 - 1;
        self.links().filter(move |(seq, ..)| *seq >= from).map(move |(seq, hash, data)| Entry {
            seq,
            epoch,
            hash,
            data: data.to_vec(),
            ends_batch: seq == last,
        })
    }

    /// Each entry's position and link, with its data.
    pub(crate) fn links(&self) -> impl Iterator<Item = (u64, Hash, &[u8])> + '_ {
        let epoch = self.batch.epoch;
        let mut link = self.previous;
        self.batch.entries.iter().zip(self.first..).map(move |(data, seq)| {
            link = chain::link(&link, seq, epoch, data);
            (seq, link, data.as_slice())
        })
    }

    /// What the object's metadata says of it.
    pub(crate) fn summary(&self) -> Summary {
        let last = self.first + self.batch.entries.len() as u64 - 1;
        let link = self.links().last().map_or(self.previous, |(_, link, _)| link);
        Summary { first: self.first, last, epoch: self.batch.epoch, link, key: self.batch.key() }
    }
}

/// A batch as its object's metadata tells it, without its entries: enough
/// to find the next place, check a cursor at its end, or know it again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) first: u64,
    pub(crate) last: u64,
    pub(crate) epoch: Epoch,
    /// The chain link at `last`.
    pub(crate) link: Hash,
    /// The batch's key.
    pub(crate) key: Hash,
}

/// The metadata headers, as S3 takes them on a write and returns them on a
/// read.
const LAST: &str = "x-amz-meta-efema-last";
const EPOCH: &str = "x-amz-meta-efema-epoch";
const LINK: &str = "x-amz-meta-efema-link";
const KEY: &str = "x-amz-meta-efema-key";

impl Summary {
    pub(crate) fn headers(&self) -> Vec<(String, String)> {
        vec![
            (LAST.to_string(), self.last.to_string()),
            (EPOCH.to_string(), self.epoch.to_string()),
            (LINK.to_string(), self.link.to_string()),
            (KEY.to_string(), self.key.to_string()),
        ]
    }

    /// The summary of the batch at `first` from its object's headers.
    pub(crate) fn from_headers<'h>(first: u64, header: impl Fn(&str) -> Option<&'h str>) -> Option<Self> {
        let last: u64 = header(LAST)?.parse().ok()?;
        if last < first {
            return None;
        }
        Some(Self {
            first,
            last,
            epoch: Epoch(header(EPOCH)?.parse().ok()?),
            link: header(LINK)?.parse().ok()?,
            key: header(KEY)?.parse().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> StreamName {
        "notes".parse().unwrap()
    }

    fn id() -> StreamId {
        StreamId::from_bytes(*b"0123456789abcdef")
    }

    #[test]
    fn a_listing_starts_from_the_newest_batch() {
        let keys: Vec<String> =
            [1, 2, 9, 10, 11, 99, 100, u64::MAX].iter().map(|&p| batch(&stream(), id(), p)).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.reverse();
        assert_eq!(sorted, keys, "keys order the other way from positions");
        assert!(keys[0].ends_with("/b/99999999999999999998"));
        let prefix = batches(&stream(), id());
        for (key, position) in keys.iter().zip([1, 2, 9, 10, 11, 99, 100, u64::MAX]) {
            assert_eq!(first_of(key, &prefix), Some(position));
        }
    }

    #[test]
    fn a_key_that_is_not_a_batch_is_not_read_as_one() {
        let prefix = batches(&stream(), id());
        assert_eq!(first_of(&format!("{prefix}9999"), &prefix), None);
        assert_eq!(first_of(&format!("{prefix}9999999999999999999x"), &prefix), None);
        assert_eq!(first_of("other/b/99999999999999999998", &prefix), None);
        // Nines for more than the largest position.
        assert_eq!(first_of(&format!("{prefix}00000000000000000000"), &prefix), None);
        let key = Hash::from_bytes([7; 32]);
        let offers = offers(&stream(), id(), &key);
        assert_eq!(offered_at(&offer(&stream(), id(), &key, 42), &offers), Some(42));
        assert_eq!(offered_at(&format!("{offers}42"), &offers), None);
    }

    #[test]
    fn a_stored_batch_links_its_entries_from_the_link_before_it() {
        let previous = chain::genesis(&id());
        let stored = Stored {
            first: 1,
            previous,
            batch: Batch { epoch: Epoch(2), entries: vec![b"a".to_vec(), b"bc".to_vec()], stream: None, first: None },
        };
        let entries: Vec<Entry> = stored.entries_from(1).collect();
        assert_eq!(stored.entries_from(2).map(|entry| entry.seq).collect::<Vec<_>>(), [2]);
        let one = chain::link(&previous, 1, Epoch(2), b"a");
        assert_eq!(entries[0], Entry { seq: 1, epoch: Epoch(2), hash: one, data: b"a".to_vec(), ends_batch: false });
        assert_eq!(entries[1].hash, chain::link(&one, 2, Epoch(2), b"bc"));
        assert!(entries[1].ends_batch, "the last entry of a batch does not say it ends it");
        let summary = stored.summary();
        assert_eq!((summary.first, summary.last, summary.epoch, summary.link), (1, 2, Epoch(2), entries[1].hash));
        assert_eq!(summary.key, stored.batch.key());
        let headers = summary.headers();
        let read = Summary::from_headers(1, |name| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str()));
        assert_eq!(read, Some(summary));
        assert_eq!(
            Summary::from_headers(3, |name| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())),
            None
        );
    }

    /// The reference page is the contract for clients in other languages:
    /// what it says the keys, the bodies and the metadata are must be what
    /// this code writes.
    #[test]
    fn the_reference_page_describes_this_layout() {
        let page = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/src/content/docs/reference/bucket.md"
        ))
        .expect("the reference page of the layout");
        let says = |text: &str| assert!(page.contains(text), "the page does not say {text:?}");
        says(&format!("`{PROBE}`"));
        says(&format!("| 1 | `format` | uint | the layout: `{FORMAT}` |"));
        says(&format!("Position 1 is\n`{}`", from_nines(1)));
        says(&format!("position 10 is `{}`", from_nines(10)));
        says("| `<stream>/stream` |");
        says("| `<stream>/<id>/b/<first, from nines>` |");
        says("| `<stream>/<id>/o/<batch key>/<first>` |");
        for header in [LAST, EPOCH, LINK, KEY] {
            says(&format!("| `{header}` |"));
        }
        assert_eq!(record(&stream()), "notes/stream");
        assert_eq!(batch(&stream(), id(), 1), format!("notes/{}/b/99999999999999999998", id()));
        let key = Hash::from_bytes([7; 32]);
        assert_eq!(offer(&stream(), id(), &key, 5), format!("notes/{}/o/{key}/00000000000000000005", id()));
        // The bodies' tables, key by key.
        for row in [
            "| 0 | `id` | identity |",
            "| 0 | `first` | uint |",
            "| 1 | `previous` | link |",
            "| 2 | `batch` | [batch]",
        ] {
            says(row);
        }
    }

    // The layout is a contract between devices, like the wire format: an
    // object written by one release is read by every other. If this test
    // fails, the change is a new FORMAT, not a fix.
    #[test]
    fn what_a_bucket_holds_is_frozen() {
        let record = Record { id: id(), format: FORMAT };
        let mut expected = vec![0xa2, 0x00, 0x50];
        expected.extend_from_slice(b"0123456789abcdef");
        expected.extend_from_slice(&[0x01, 0x01]);
        assert_eq!(efema_proto::wire::encode(&record), expected);

        let stored = Stored {
            first: 3,
            previous: Hash::from_bytes([0xcd; 32]),
            batch: Batch { epoch: Epoch(1), entries: vec![b"x".to_vec()], stream: None, first: None },
        };
        let mut expected = vec![0xa3, 0x00, 0x03, 0x01, 0x58, 0x20];
        expected.extend_from_slice(&[0xcd; 32]);
        expected.extend_from_slice(&[0x02, 0xa2, 0x00, 0x01, 0x01, 0x81, 0x41, b'x']);
        assert_eq!(efema_proto::wire::encode(&stored), expected);
        assert_eq!(efema_proto::wire::decode::<Stored>(&expected).unwrap(), stored);
    }
}
