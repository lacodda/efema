//! The format vectors, read back.
//!
//! `docs/public/format-vectors.txt` is written by `tools/format-vectors.py`,
//! an implementation of efema's formats apart from this code. Every case is
//! read here and checked against the code - what it reads, and where the
//! code writes bytes that do not depend on chance, what it writes. A section
//! the file has and this module does not check fails too, so a new part of
//! the format cannot go unread. If a test here fails after a change to the
//! code, the change is a new version of that part of the format, not a fix.

use std::collections::BTreeMap;
use std::str::FromStr;

use efema_proto::wire::{self, Batch, Entry, Head, Heads, Limits, Page, Problem, ProblemCode, Written};
use efema_proto::{AccessKey, Cursor, Epoch, Hash, StreamId, StreamName, chain};
use lacodda_seal::{Key, LockedKey};

use crate::DeviceId;
use crate::bucket::layout::{self, Record, Stored};
use crate::envelope::{self, Compression, KEY_CONTEXT, KeyEntry, Opened};

/// One case: its fields in the order the file gives them.
struct Case(Vec<(String, String)>);

impl Case {
    fn get(&self, name: &str) -> &str {
        let mut found = self.0.iter().filter(|(n, _)| n == name);
        let value = found.next().map(|(_, v)| v.as_str()).unwrap_or_else(|| panic!("no field {name}"));
        assert!(found.next().is_none(), "{name} given twice");
        value
    }

    fn opt(&self, name: &str) -> Option<&str> {
        self.0.iter().any(|(n, _)| n == name).then(|| self.get(name))
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.0.iter().filter(|(n, _)| n == name).map(|(_, v)| v.as_str()).collect()
    }

    fn bytes(&self, name: &str) -> Vec<u8> {
        hex(self.get(name))
    }

    fn array<const N: usize>(&self, name: &str) -> [u8; N] {
        self.bytes(name).try_into().unwrap_or_else(|_| panic!("{name} is not {N} bytes"))
    }

    fn num<T: FromStr>(&self, name: &str) -> T {
        self.get(name).parse().unwrap_or_else(|_| panic!("{name} is not a number"))
    }

    fn epoch(&self) -> Epoch {
        Epoch(self.num("epoch"))
    }

    fn stream(&self) -> StreamId {
        StreamId::from_bytes(self.array("stream"))
    }

    fn entries(&self) -> Vec<Vec<u8>> {
        self.all("entry").into_iter().map(hex).collect()
    }

    fn key(&self) -> Key {
        Key::from_bytes(self.array("key"))
    }
}

fn hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "{text:?} is not hexadecimal");
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hexadecimal")).collect()
}

fn hash(text: &str) -> Hash {
    text.parse().expect("a link")
}

/// The file's sections, each a list of cases.
fn sections() -> BTreeMap<String, Vec<Case>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/public/format-vectors.txt");
    let text = std::fs::read_to_string(path).expect("the format vectors");
    let mut sections: BTreeMap<String, Vec<Case>> = BTreeMap::new();
    let mut section = None;
    let mut case = Vec::new();
    let mut end = |section: &Option<String>, case: &mut Vec<(String, String)>| {
        if !case.is_empty() {
            let name = section.clone().expect("a case before any section");
            sections.entry(name).or_default().push(Case(std::mem::take(case)));
        }
    };
    for line in text.lines() {
        if line.is_empty() {
            end(&section, &mut case);
        } else if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            end(&section, &mut case);
            section = Some(name.to_string());
        } else if !line.starts_with('#') {
            let (name, value) = line.split_once('=').unwrap_or_else(|| panic!("not a field: {line:?}"));
            case.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    end(&section, &mut case);
    sections
}

fn cases(section: &str) -> Vec<Case> {
    let cases = sections().remove(section).unwrap_or_else(|| panic!("no section [{section}]"));
    assert!(!cases.is_empty(), "[{section}] has no cases");
    cases
}

/// Holds `message` to the case's CBOR, both ways.
fn same_cbor<T>(case: &Case, message: &T)
where
    T: minicbor::Encode<()> + for<'b> minicbor::Decode<'b, ()> + PartialEq + std::fmt::Debug,
{
    let bytes = case.bytes("cbor");
    assert_eq!(wire::encode(message), bytes, "{message:?} encodes otherwise");
    assert_eq!(&wire::decode::<T>(&bytes).expect("decodes"), message);
}

const SECTIONS: [&str; 19] = [
    "access",
    "batch-key",
    "bucket-batch",
    "bucket-keys",
    "bucket-record",
    "cbor-batch",
    "cbor-heads",
    "cbor-limits",
    "cbor-page",
    "cbor-problem",
    "cbor-written",
    "cursor",
    "entry",
    "genesis",
    "key-entry",
    "key-id",
    "link",
    "locked",
    "sealed",
];

#[test]
fn every_section_is_read_here() {
    let found: Vec<String> = sections().into_keys().collect();
    assert_eq!(found, SECTIONS, "a section of the vectors is not checked by this module, or one is gone");
}

#[test]
fn access() {
    for case in cases("access") {
        let key: AccessKey = case.get("key").parse().expect("an access key");
        assert_eq!(key.digest().to_vec(), case.bytes("digest"));
        assert_eq!(key.id(), case.get("id"));
        assert_eq!(key.expose(), case.get("key"));
    }
}

#[test]
fn chain() {
    for case in cases("genesis") {
        assert_eq!(chain::genesis(&case.stream()), hash(case.get("link")));
    }
    for case in cases("link") {
        let link = chain::link(&hash(case.get("previous")), case.num("seq"), case.epoch(), &case.bytes("data"));
        assert_eq!(link, hash(case.get("link")));
    }
}

#[test]
fn batch_key() {
    for case in cases("batch-key") {
        let batch = Batch { epoch: case.epoch(), entries: case.entries(), stream: None };
        assert_eq!(batch.key(), hash(case.get("key")));
        // The stream a batch names is not part of its key.
        assert_eq!(Batch { stream: Some(StreamId::from_bytes([1; 16])), ..batch.clone() }.key(), batch.key());
    }
}

#[test]
fn cursor() {
    for case in cases("cursor") {
        let cursor = Cursor { stream: case.stream(), seq: case.num("seq"), hash: hash(case.get("hash")) };
        assert_eq!(cursor.to_string(), case.get("text"));
        assert_eq!(case.get("text").parse::<Cursor>().expect("a cursor"), cursor);
    }
}

#[test]
fn cbor_messages() {
    for case in cases("cbor-batch") {
        let stream = case.opt("stream").map(|id| StreamId::from_bytes(hex(id).try_into().expect("an identity")));
        same_cbor(&case, &Batch { epoch: case.epoch(), entries: case.entries(), stream });
    }
    for case in cases("cbor-written") {
        let written = Written {
            stream: case.stream(),
            epoch: case.epoch(),
            first: case.num("first"),
            last: case.num("last"),
            repeated: case.opt("repeated").is_some_and(|r| r == "true"),
        };
        same_cbor(&case, &written);
    }
    for case in cases("cbor-page") {
        let entries = case
            .all("entry")
            .into_iter()
            .map(|entry| {
                let [seq, epoch, link, data] = entry.split(' ').collect::<Vec<_>>().try_into().expect("four parts");
                let data = if data == "-" { Vec::new() } else { hex(data) };
                Entry { seq: seq.parse().unwrap(), epoch: Epoch(epoch.parse().unwrap()), hash: hash(link), data }
            })
            .collect();
        same_cbor(&case, &Page { stream: case.stream(), epoch: case.epoch(), head: case.num("head"), entries });
    }
    for case in cases("cbor-heads") {
        let heads = case
            .all("head")
            .into_iter()
            .map(|head| {
                let [name, id, at] = head.split(' ').collect::<Vec<_>>().try_into().expect("three parts");
                let id = (id != "-").then(|| StreamId::from_bytes(hex(id).try_into().expect("an identity")));
                Head { stream: name.parse::<StreamName>().unwrap(), id, head: at.parse().unwrap() }
            })
            .collect();
        same_cbor(&case, &Heads { heads });
    }
    for case in cases("cbor-limits") {
        let limits = Limits {
            write: case.get("write") == "true",
            body: case.num("body"),
            entry: case.num("entry"),
            quota: case.num("quota"),
            used: case.num("used"),
            rate: case.num("rate"),
            page_entries: case.num("page_entries"),
            page_entries_max: case.num("page_entries_max"),
            page_bytes: case.num("page_bytes"),
            wait_max: case.num("wait_max"),
            watches: case.num("watches"),
        };
        same_cbor(&case, &limits);
    }
    for case in cases("cbor-problem") {
        let num = |name| case.opt(name).map(|n: &str| n.parse::<u64>().unwrap());
        let problem = Problem {
            code: ProblemCode::from_wire(case.get("code")),
            message: case.get("message").to_string(),
            head: num("head"),
            epoch: num("epoch").map(|e| Epoch(e as u32)),
            limit: num("limit"),
            retry: num("retry"),
        };
        assert!(!matches!(problem.code, ProblemCode::Other(_)), "{} is not a code of the protocol", case.get("code"));
        same_cbor(&case, &problem);
    }
}

#[test]
fn sealing() {
    for case in cases("key-id") {
        assert_eq!(case.key().id().as_bytes().to_vec(), case.bytes("id"));
    }
    for case in cases("sealed") {
        let (key, sealed) = (case.key(), case.bytes("sealed"));
        assert_eq!(lacodda_seal::sealed_key_id(&sealed).unwrap(), key.id());
        assert_eq!(sealed[10..34], case.bytes("nonce"), "the nonce is where the page says");
        assert_eq!(*key.open(&case.bytes("context"), &sealed).expect("opens"), case.bytes("plaintext"));
    }
    for case in cases("locked") {
        let lock = LockedKey::from_bytes(&case.bytes("locked")).expect("a lock");
        assert_eq!(lock.key_id(), case.key().id());
        let params = lock.params();
        let read = (params.memory_kib(), params.iterations(), params.parallelism());
        assert_eq!(read, (case.num("memory"), case.num("passes"), case.num("lanes")));
        let locked_at = lock.locked_at().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(locked_at, case.num::<u64>("locked_at"));
        let key = lock.unlock(&case.bytes("passphrase"), &case.bytes("context")).expect("unlocks");
        assert_eq!(key.to_bytes().to_vec(), case.bytes("key"));
        assert_eq!(lock.to_bytes(), case.bytes("locked"));
    }
}

#[test]
fn entries() {
    for case in cases("entry") {
        let (key, epoch, entry) = (case.key(), case.epoch(), case.bytes("entry"));
        let device = DeviceId::from_bytes(case.array("device"));
        match envelope::open(&key, epoch, &entry).expect("opens") {
            Opened::Entry { device: by, data } => assert_eq!((by, data), (device, case.bytes("item"))),
            _ => panic!("the entry does not open as one"),
        }
        // What the code writes inside the seal is the same: format, device,
        // codec, body.
        let context = [b"efema/entry/v1".as_slice(), &epoch.0.to_be_bytes()].concat();
        let theirs = key.open(&context, &entry).unwrap();
        assert_eq!(theirs[..17], [&[2][..], device.as_bytes()].concat());
        assert_eq!(theirs[17], case.num::<u8>("codec"));
        assert_eq!(theirs[18..], case.bytes("body"));
        if case.get("codec") == "0" {
            let ours = envelope::seal(&key, epoch, device, &case.bytes("item"), Compression::Off).unwrap();
            assert_eq!(*key.open(&context, &ours).unwrap(), *theirs);
        }
    }
}

#[test]
fn key_entries() {
    for case in cases("key-entry") {
        let (key, bytes) = (case.key(), case.bytes("entry"));
        let entry = KeyEntry::from_bytes(&bytes).expect("a key entry");
        let generation: u32 = case.num("generation");
        assert_eq!(entry.generation(&key), Some(generation));
        let unlocked = entry.lock.unlock(&case.bytes("passphrase"), KEY_CONTEXT).expect("unlocks");
        assert_eq!(unlocked.to_bytes().to_vec(), case.bytes("key"));
        assert_eq!(entry.to_bytes(), bytes);
        // Written by the code over the same lock: the same lock, then a seal
        // of the same generation for the same context.
        let ours = KeyEntry::new(&key, entry.lock.clone(), generation).unwrap().to_bytes();
        assert_eq!(ours.len(), bytes.len());
        assert_eq!(ours[..lacodda_seal::LOCKED_LEN], bytes[..lacodda_seal::LOCKED_LEN]);
        assert_eq!(KeyEntry::from_bytes(&ours).unwrap().generation(&key), Some(generation));
    }
}

#[test]
fn a_bucket() {
    for case in cases("bucket-keys") {
        let name: StreamName = case.get("name").parse().expect("a name");
        let id = StreamId::from_bytes(case.array("id"));
        let (batch_key, first) = (hash(case.get("batch_key")), case.num("first"));
        assert_eq!(layout::PROBE, case.get("probe"));
        assert_eq!(layout::record(&name), case.get("record"));
        assert_eq!(layout::batch(&name, id, first), case.get("batch"));
        assert_eq!(layout::offer(&name, id, &batch_key, first), case.get("offer"));
        assert_eq!(layout::first_of(case.get("batch"), &layout::batches(&name, id)), Some(first));
        assert_eq!(layout::offered_at(case.get("offer"), &layout::offers(&name, id, &batch_key)), Some(first));
    }
    for case in cases("bucket-record") {
        assert_eq!(case.num::<u32>("format"), layout::FORMAT);
        same_cbor(&case, &Record { id: StreamId::from_bytes(case.array("id")), format: case.num("format") });
    }
    for case in cases("bucket-batch") {
        let stored = Stored {
            first: case.num("first"),
            previous: hash(case.get("previous")),
            batch: Batch { epoch: case.epoch(), entries: case.entries(), stream: None },
        };
        same_cbor(&case, &stored);
        let headers = stored.summary().headers();
        let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str()).unwrap();
        assert_eq!(header("x-amz-meta-efema-last"), case.get("last"));
        assert_eq!(header("x-amz-meta-efema-epoch"), case.get("epoch"));
        assert_eq!(header("x-amz-meta-efema-link"), case.get("link"));
        assert_eq!(header("x-amz-meta-efema-key"), case.get("key"));
    }
}
