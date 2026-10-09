# ADR 0009: One random key per stream, locked under the passphrase, kept in the stream

- Status: accepted
- Date: 2026-10-09

## Context

Every device that syncs a stream needs the same key, and the person types a
passphrase - nothing longer - on each device. Deriving the key from the
passphrase directly needs a salt every device agrees on: a fixed one lets an
attacker precompute guesses for everyone at once, and a random one has to
travel. A key derived from the passphrase also changes with it, so changing
the passphrase would mean sealing the whole stream again - and the plan says,
from 0.3, that it must not.

## Decision

- A stream's key is **32 random bytes**, made by the first device that opens
  the stream.
- That device **locks** the key under the passphrase (`lacodda-seal`:
  Argon2id with a random salt, the parameters and the salt in the lock's
  header, the key sealed with XChaCha20-Poly1305) and writes the lock as the
  stream's **first entry**.
- Every other device reads the first lock in the stream and unlocks it with
  the passphrase. If two devices create a stream at once, both locks land;
  **the first one in the stream is the key**. The device whose lock came
  second reads the stream back, adopts the first key, and has sealed nothing
  with its own.
- A device keeps the **lock**, not the key, in its state - so it opens the
  stream without the network, and its state file opens nothing on its own.
  The key leaves the client only when the app asks for it.
- An entry is a `lacodda-seal` sealed blob. Its context is
  `"efema/entry/v1" || epoch`, so a relay that relabels an entry's epoch makes
  it fail to open rather than be read as another format. Inside: an entry
  format byte, the writing device's identity and the app's bytes - the device
  is sealed, and the relay never learns who wrote what.
- The stream's name and identity are **not** in the context: a stream copied to
  another relay, under the same name or another, stays readable.

## Consequences

- Changing the passphrase is writing a new lock of the same key; nothing else
  is sealed again (0.3).
- The lock is on the relay, and so is the chance to guess passphrases against
  it offline, at Argon2id's cost per guess. The threat model says so: the
  passphrase's length is the whole of that protection.
- A stream holds two kinds of entries, told apart by their first byte. A
  reader passes over locks; an entry that is neither kind is refused as
  foreign.
- An entry costs 67 bytes over the app's own: 50 for sealing, 16 for the
  device, one for the entry format.
