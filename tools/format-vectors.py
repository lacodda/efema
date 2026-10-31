"""Write efema's format vectors, or with --check say whether the file on disk
is what they are.

This is an implementation of efema's formats apart from the Rust code, written
from the documentation: the access key's digest, the hash chain, the batch
key, the cursor, the CBOR messages, lacodda-seal's sealed blob and lock, an
entry and a key entry, and the layout of a stream in a bucket. Hashes come
from Python's hashlib; Argon2id and ChaCha20-Poly1305 from the `cryptography`
package (OpenSSL); HChaCha20, CBOR and the zstd frames are written out below.
The tests of the efema crate read every case back, and check that the Rust
code writes the same bytes. Two implementations that agree on every byte are
what makes the pages a contract.

    python tools/format-vectors.py           write docs/public/format-vectors.txt
    python tools/format-vectors.py --check   exit 1 if the file is not what this writes

Needs cryptography 44 or later, for Argon2id.
"""

import hashlib
import struct
import sys
from pathlib import Path

from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.kdf.argon2 import Argon2id

OUT = Path(__file__).resolve().parent.parent / "docs" / "public" / "format-vectors.txt"


def sha256(*parts):
    return hashlib.sha256(b"".join(parts)).digest()


def u32(n):
    return struct.pack(">I", n)


def u64(n):
    return struct.pack(">Q", n)


# --- CBOR (RFC 8949): definite lengths, the shortest form of every integer,
# map keys in ascending order - what efema writes.


def cbor_head(major, n):
    if n < 24:
        return bytes([major << 5 | n])
    for extra, size in ((24, 1), (25, 2), (26, 4), (27, 8)):
        if n < 1 << (8 * size):
            return bytes([major << 5 | extra]) + n.to_bytes(size, "big")
    raise ValueError(n)


def cbor(value):
    if isinstance(value, bool):
        return b"\xf5" if value else b"\xf4"
    if isinstance(value, int):
        return cbor_head(0, value)
    if isinstance(value, bytes):
        return cbor_head(2, len(value)) + value
    if isinstance(value, str):
        data = value.encode()
        return cbor_head(3, len(data)) + data
    if isinstance(value, list):
        return cbor_head(4, len(value)) + b"".join(cbor(v) for v in value)
    if isinstance(value, dict):
        present = sorted((k, v) for k, v in value.items() if v is not None)
        return cbor_head(5, len(present)) + b"".join(cbor(k) + cbor(v) for k, v in present)
    raise TypeError(value)


# --- XChaCha20-Poly1305: HChaCha20 (draft-irtf-cfrg-xchacha) turns the key and
# the nonce's first 16 bytes into a subkey for ChaCha20-Poly1305 (RFC 8439),
# whose nonce is four zero bytes and the last 8.

MASK = 0xFFFFFFFF


def hchacha20(key, nonce16):
    def rotl(v, c):
        return ((v << c) & MASK) | (v >> (32 - c))

    def quarter(s, a, b, c, d):
        s[a] = (s[a] + s[b]) & MASK
        s[d] = rotl(s[d] ^ s[a], 16)
        s[c] = (s[c] + s[d]) & MASK
        s[b] = rotl(s[b] ^ s[c], 12)
        s[a] = (s[a] + s[b]) & MASK
        s[d] = rotl(s[d] ^ s[a], 8)
        s[c] = (s[c] + s[d]) & MASK
        s[b] = rotl(s[b] ^ s[c], 7)

    s = [0x61707865, 0x3320646E, 0x79622D32, 0x6B206574]
    s += list(struct.unpack("<8I", key)) + list(struct.unpack("<4I", nonce16))
    for _ in range(10):
        quarter(s, 0, 4, 8, 12)
        quarter(s, 1, 5, 9, 13)
        quarter(s, 2, 6, 10, 14)
        quarter(s, 3, 7, 11, 15)
        quarter(s, 0, 5, 10, 15)
        quarter(s, 1, 6, 11, 12)
        quarter(s, 2, 7, 8, 13)
        quarter(s, 3, 4, 9, 14)
    return struct.pack("<8I", *(s[0:4] + s[12:16]))


# The draft's own test vector, section 2.2.1: a wrong HChaCha20 stops here
# rather than writing vectors nothing else agrees with.
assert hchacha20(bytes(range(32)), bytes.fromhex("000000090000004a0000000031415927")).hex() == (
    "82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc"
)


def xchacha_seal(key, nonce, plaintext, aad):
    return ChaCha20Poly1305(hchacha20(key, nonce[:16])).encrypt(b"\0" * 4 + nonce[16:], plaintext, aad)


# --- lacodda-seal: the Sealing page.


def key_id(key):
    return sha256(b"lacodda-seal/v1/key-id", key)[:8]


def seal(key, context, plaintext, nonce):
    header = b"S\x01" + key_id(key) + nonce
    return header + xchacha_seal(key, nonce, plaintext, b"lacodda-seal/v1/sealed" + header + context)


def lock(key, passphrase, context, memory, passes, lanes, locked_at, salt, nonce):
    header = b"L\x01" + u32(memory) + u32(passes) + u32(lanes) + u64(locked_at) + salt + key_id(key) + nonce
    kek = Argon2id(salt=salt, length=32, iterations=passes, lanes=lanes, memory_cost=memory).derive(passphrase)
    return header + xchacha_seal(kek, nonce, key, b"lacodda-seal/v1/locked" + header + context)


# --- efema: the Protocol, Sealing and A stream in a bucket pages.

ENTRY_FORMAT = 2


def access_digest(key):
    return sha256(b"efema/access/v1", key)


def genesis(stream):
    return sha256(b"efema/chain/v1/genesis", stream)


def link(previous, seq, epoch, data):
    return sha256(previous, u64(seq), u32(epoch), u64(len(data)), data)


def batch_key(epoch, entries):
    return sha256(b"efema/batch/v1", u32(epoch), u64(len(entries)), *(u64(len(e)) + e for e in entries))


def zstd_frame(size, block):
    """A zstd frame (RFC 8878) of one block, single segment, its content size
    in one byte (descriptor 0x20)."""
    assert size < 256
    return b"\x28\xb5\x2f\xfd\x20" + bytes([size]) + block


def block_header(kind, size):
    return (1 | kind << 1 | size << 3).to_bytes(3, "little")


def raw_frame(item):
    return zstd_frame(len(item), block_header(0, len(item)) + item)


def rle_frame(byte, count):
    return zstd_frame(count, block_header(1, count) + bytes([byte]))


def entry(key, epoch, device, codec, body, nonce):
    plaintext = bytes([ENTRY_FORMAT]) + device + bytes([codec]) + body
    return seal(key, b"efema/entry/v1" + u32(epoch), plaintext, nonce)


def from_nines(position):
    return "".join(str(9 - int(d)) for d in f"{position:020d}")


# --- The inputs. Fixed bytes stand where the formats take random ones: a
# key, a nonce, a salt, a stream's identity.

KEY = b"0123456789abcdef0123456789abcdef"
OTHER_KEY = bytes(range(0x40, 0x60))
NONCE = b"nonce-nonce-nonce-nonce!"
SALT = b"salt-salt-salt-s"
STREAM = b"0123456789abcdef"
DEVICE = b"device-device-de"
PASSPHRASE = b"correct horse battery staple"
LOCKED_AT = 1_760_000_000


class Vectors:
    def __init__(self):
        self.lines = []

    def section(self, name, *comment):
        self.lines += ["", f"[{name}]"] + [f"# {line}".rstrip() for line in comment]

    def case(self, *fields):
        self.lines.append("")
        for name, value in fields:
            if isinstance(value, bytes):
                value = value.hex()
            self.lines.append(f"{name} = {value}".rstrip())

    def text(self):
        return "\n".join(HEADER + self.lines) + "\n"


HEADER = [
    "# efema's format vectors: protocol 1, lacodda-seal format 1, entry format 2,",
    "# bucket layout 1. The pages these cases follow are at",
    "# https://lacodda.github.io/efema/reference/format/",
    "#",
    "# A section is one part of the format; its comment names the fields. A blank",
    "# line ends a case, and a field given more than once in a case is a list, in",
    "# order. Bytes are lowercase hexadecimal - nothing after `=` is no bytes -",
    "# numbers are decimal, and a field the comment calls text is the text itself.",
    "#",
    "# Written by tools/format-vectors.py, an implementation of the formats apart",
    "# from the Rust code; read back by the tests of the efema crate, which also",
    "# check that the code writes the same bytes. Do not edit by hand.",
]


def build():
    v = Vectors()

    v.section(
        "access",
        "key: an access key, text. digest: what a relay keeps of it.",
        "id: the name a relay gives it, text - the digest's first eight hex digits.",
    )
    for key in (bytes([0x5A] * 32), bytes(range(32))):
        digest = access_digest(key)
        v.case(("key", "efema_" + key.hex()), ("digest", digest), ("id", digest.hex()[:8]))

    v.section("genesis", "stream: a stream's identity. link: the chain's first link, before any entry.")
    for stream in (STREAM, bytes.fromhex("11c3cd7e971ccb544d78c74ae8d814ba")):
        v.case(("stream", stream), ("link", genesis(stream)))

    v.section("link", "previous: the link before. seq, epoch: numbers. data: the entry. link: its link.")
    chain = [(1, 1, b"hello"), (2, 1, b""), (3, 2, bytes(range(200)))]
    previous = genesis(STREAM)
    links = []
    for seq, epoch, data in chain:
        current = link(previous, seq, epoch, data)
        v.case(("previous", previous), ("seq", seq), ("epoch", epoch), ("data", data), ("link", current))
        links.append(current)
        previous = current

    v.section("batch-key", "epoch: a number. entry: the batch's entries, in order. key: the batch key.")
    for epoch, entries in ((1, [b"hello", b""]), (1, [b"hel", b"lo"]), (2, [b"hello", b""]), (3, [b"x"])):
        v.case(("epoch", epoch), *(("entry", e) for e in entries), ("key", batch_key(epoch, entries)))

    v.section("cursor", "seq: a number. stream: an identity. hash: a link. text: the cursor as a query names it.")
    for seq, hash_ in ((0, genesis(STREAM)), (2, links[1])):
        v.case(("seq", seq), ("stream", STREAM), ("hash", hash_), ("text", f"{seq}.{STREAM.hex()}.{hash_.hex()}"))

    v.section(
        "cbor-batch",
        "epoch: a number. entry: the entries. stream: the identity, when the batch names one.",
        "first: the position the batch must start at, when it says. cbor: the batch.",
    )
    for epoch, entries, stream, first in (
        (1, [b"hello", b""], None, None),
        (7, [b"x"], STREAM, None),
        (2, [b"copied", b"in place"], STREAM, 300),
    ):
        fields = [("epoch", epoch), *(("entry", e) for e in entries)]
        if stream:
            fields.append(("stream", stream))
        if first:
            fields.append(("first", first))
        v.case(*fields, ("cbor", cbor({0: epoch, 1: entries, 2: stream, 3: first})))

    v.section(
        "cbor-written",
        "stream: an identity. epoch, first, last: numbers. repeated: present, as true, when the",
        "batch was there already. cbor: the written record.",
    )
    for repeated in (False, True):
        fields = [("stream", STREAM), ("epoch", 1), ("first", 1), ("last", 2)]
        if repeated:
            fields.append(("repeated", "true"))
        v.case(*fields, ("cbor", cbor({0: STREAM, 1: 1, 2: 1, 3: 2, 4: True if repeated else None})))

    v.section(
        "cbor-page",
        "stream: an identity. epoch, head: numbers. entry: each entry as `seq epoch hash data`,",
        "the data `-` for none, then `end` when the entry is the last of its batch. cbor: the page.",
    )
    # The first two entries of the chain, written as one batch.
    entries = [(seq, epoch, links[i], data, seq == 2) for i, (seq, epoch, data) in enumerate(chain[:2])]
    v.case(
        ("stream", STREAM),
        ("epoch", 1),
        ("head", 2),
        *(("entry", f"{s} {e} {h.hex()} {d.hex() or '-'}" + (" end" if end else "")) for s, e, h, d, end in entries),
        (
            "cbor",
            cbor({0: STREAM, 1: 1, 2: 2, 3: [{0: s, 1: e, 2: h, 3: d, 4: end or None} for s, e, h, d, end in entries]}),
        ),
    )

    v.section(
        "cbor-heads",
        "head: each stream as `name id head`, the id `-` when no stream has the name. cbor: the",
        "heads record.",
    )
    heads = [("notes", STREAM, 2), ("todo", None, 0)]
    v.case(
        *(("head", f"{n} {i.hex() if i else '-'} {h}") for n, i, h in heads),
        ("cbor", cbor({0: [{0: n, 1: i, 2: h} for n, i, h in heads]})),
    )

    v.section(
        "cbor-limits",
        "write: true or false. body, entry, quota, used, rate, page_entries, page_entries_max,",
        "page_bytes, wait_max, watches: numbers. cbor: the limits record.",
    )
    names = ["body", "entry", "quota", "used", "rate", "page_entries", "page_entries_max", "page_bytes"]
    names += ["wait_max", "watches"]
    relay = [16777216, 8388608, 8589934592, 12345, 600, 1000, 10000, 8388608, 60, 64]
    bucket = [16777216, 8388608, 2**64 - 1, 0, 2**64 - 1, 1000, 10000, 8388608, 60, 64]
    for write, values in ((True, relay), (False, bucket)):
        v.case(
            ("write", "true" if write else "false"),
            *zip(names, values),
            ("cbor", cbor({0: write, **{i + 1: value for i, value in enumerate(values)}})),
        )

    v.section(
        "cbor-problem",
        "code, message: text. head, epoch, limit, retry: numbers, each only with the refusal it",
        "belongs to. cbor: the problem.",
    )
    problems = [
        ("cursor_ahead", "the cursor is past the stream's head", 2, 7),
        ("epoch_behind", "the stream is at a newer epoch", 3, 3),
        ("too_large", "the batch is larger than the relay takes", 4, 16777216),
        ("rate_limited", "too many requests with this key", 5, 12),
        ("quota_exceeded", "the space is full", 4, 8589934592),
        ("out_of_place", "the batch is to start elsewhere", 2, 299),
        ("not_found", "no such path", None, None),
    ]
    extra = {2: "head", 3: "epoch", 4: "limit", 5: "retry"}
    for code, message, key, value in problems:
        fields = [("code", code), ("message", message)]
        if key:
            fields.append((extra[key], value))
        v.case(*fields, ("cbor", cbor({0: code, 1: message, **({key: value} if key else {})})))

    v.section("key-id", "key: a key. id: its identity.")
    for key in (KEY, OTHER_KEY):
        v.case(("key", key), ("id", key_id(key)))

    v.section(
        "sealed",
        "key, context, nonce, plaintext: what is sealed, how. sealed: the sealed blob.",
    )
    for key, context, plaintext in ((KEY, b"context", b"hello"), (KEY, b"", b""), (OTHER_KEY, b"efema", bytes(300))):
        v.case(
            ("key", key),
            ("context", context),
            ("nonce", NONCE),
            ("plaintext", plaintext),
            ("sealed", seal(key, context, plaintext, NONCE)),
        )

    v.section(
        "locked",
        "key: the key locked. passphrase, context: what it is locked under. memory (KiB), passes,",
        "lanes, locked_at (seconds since 1970): numbers. salt, nonce: as drawn. locked: the lock.",
    )
    for key, lanes in ((KEY, 1), (OTHER_KEY, 4)):
        v.case(
            ("key", key),
            ("passphrase", PASSPHRASE),
            ("context", b"context"),
            ("memory", 19456),
            ("passes", 2),
            ("lanes", lanes),
            ("locked_at", LOCKED_AT),
            ("salt", SALT),
            ("nonce", NONCE),
            ("locked", lock(key, PASSPHRASE, b"context", 19456, 2, lanes, LOCKED_AT, SALT, NONCE)),
        )

    v.section(
        "entry",
        "key: the stream's key. epoch: a number. device: the writing device. codec: a number.",
        "body: the item as the codec holds it - a zstd frame for codec 1. item: what the app pushed.",
        "nonce: as drawn. entry: the sealed entry, as the stream holds it.",
    )
    for epoch, codec, item in (
        (1, 0, b"a note"),
        (2, 0, b""),
        (1, 1, b"hello"),
        (1, 1, b"z" * 64),
    ):
        if codec == 0:
            body = item
        elif item == b"z" * 64:
            body = rle_frame(ord("z"), 64)
        else:
            body = raw_frame(item)
        v.case(
            ("key", KEY),
            ("epoch", epoch),
            ("device", DEVICE),
            ("codec", codec),
            ("body", body),
            ("item", item),
            ("nonce", NONCE),
            ("entry", entry(KEY, epoch, DEVICE, codec, body, NONCE)),
        )

    v.section(
        "key-entry",
        "key: the stream's key. passphrase: what locks it. memory, passes, lanes, locked_at: numbers.",
        "salt, lock_nonce: the lock's. generation: a number. seal_nonce: the key's seal's. entry:",
        "the key entry, as the stream holds it.",
    )
    for generation in (0, 3):
        locked = lock(KEY, PASSPHRASE, b"efema/stream-key/v1", 19456, 2, 1, LOCKED_AT, SALT, NONCE)
        sealed = seal(KEY, b"efema/key-entry/v1" + locked, u32(generation), NONCE[::-1])
        v.case(
            ("key", KEY),
            ("passphrase", PASSPHRASE),
            ("memory", 19456),
            ("passes", 2),
            ("lanes", 1),
            ("locked_at", LOCKED_AT),
            ("salt", SALT),
            ("lock_nonce", NONCE),
            ("generation", generation),
            ("seal_nonce", NONCE[::-1]),
            ("entry", locked + sealed),
        )

    v.section(
        "bucket-keys",
        "name: a stream's name, text. id: its identity. batch_key: a batch key. first: a position.",
        "probe, record, batch, offer: the objects' keys, text.",
    )
    for name, first in (("notes", 1), ("notes", 10), ("a.b-c_d", 18446744073709551615)):
        key = batch_key(1, [b"x"])
        v.case(
            ("name", name),
            ("id", STREAM),
            ("batch_key", key),
            ("first", first),
            ("probe", ".efema-probe"),
            ("record", f"{name}/stream"),
            ("batch", f"{name}/{STREAM.hex()}/b/{from_nines(first)}"),
            ("offer", f"{name}/{STREAM.hex()}/o/{key.hex()}/{first:020d}"),
        )

    v.section("bucket-record", "id: the stream's identity. format: the layout, a number. cbor: the record.")
    v.case(("id", STREAM), ("format", 1), ("cbor", cbor({0: STREAM, 1: 1})))

    v.section(
        "bucket-batch",
        "first: a number. previous: the link before the batch. epoch: a number. entry: its",
        "entries. cbor: the stored batch. last, link, key: its metadata - x-amz-meta-efema-last,",
        "-link and -key, as text; x-amz-meta-efema-epoch is the epoch.",
    )
    for first, previous, epoch, data in ((1, genesis(STREAM), 1, [b"hello", b""]), (3, links[1], 2, [bytes(range(200))])):
        last_link = previous
        for i, d in enumerate(data):
            last_link = link(last_link, first + i, epoch, d)
        v.case(
            ("first", first),
            ("previous", previous),
            ("epoch", epoch),
            *(("entry", d) for d in data),
            ("cbor", cbor({0: first, 1: previous, 2: {0: epoch, 1: data}})),
            ("last", first + len(data) - 1),
            ("link", last_link.hex()),
            ("key", batch_key(epoch, data).hex()),
        )

    return v.text()


def main():
    text = build()
    if sys.argv[1:] == ["--check"]:
        if not OUT.exists() or OUT.read_bytes() != text.encode():
            print(f"{OUT} is not what tools/format-vectors.py writes: run it, and read the difference", file=sys.stderr)
            sys.exit(1)
        print(f"{OUT.name}: every case is what this implementation writes")
        return
    if sys.argv[1:]:
        sys.exit(__doc__)
    OUT.write_bytes(text.encode())
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
