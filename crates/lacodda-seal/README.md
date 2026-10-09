# lacodda-seal

> Seal bytes under a key, and lock that key under a passphrase - two small, versioned formats over Argon2id and XChaCha20-Poly1305.

[![crates.io](https://img.shields.io/crates/v/lacodda-seal?style=flat-square)](https://crates.io/crates/lacodda-seal)
[![docs.rs](https://img.shields.io/docsrs/lacodda-seal?style=flat-square&label=docs.rs)](https://docs.rs/lacodda-seal)

The sealing of the [lacodda line](https://github.com/lacodda): the efema sync
client seals every entry with it, and sefy's vaults use the same primitives.
Nothing here is new cryptography - the primitives are RustCrypto's - it is the
two formats around them, kept small and stable:

- **a sealed blob** - bytes sealed under a 32-byte key for a *context* the
  caller names. It opens under that key and that context only, and any change
  to any byte of it is refused.
- **a locked key** - a key sealed under a passphrase, with the Argon2id
  parameters and the salt in its header, so it can be stored in the open and
  opened on any device that knows the passphrase.

Each starts with its kind and its format version, so a blob written today opens
with later releases, and a newer format is refused by name instead of misread.

```rust
use lacodda_seal::{Key, LockedKey};

let key = Key::generate()?;
let sealed = key.seal(b"notes/v1", b"the plan for Tuesday")?;
assert_eq!(key.open(b"notes/v1", &sealed)?.as_slice(), b"the plan for Tuesday");

let lock = LockedKey::lock(&key, b"correct horse battery staple", b"notes/key")?;
let again = lock.unlock(b"correct horse battery staple", b"notes/key")?;
assert_eq!(again.id(), key.id());
```

The byte layouts, and what each part protects:
[Sealing](https://lacodda.github.io/efema/concepts/sealing/) on the efema
documentation site. The formats are frozen by a test whose values were computed
outside Rust; changing them is a new format version, never a fix.

## License

MIT (c) [Kirill Lakhtachev](https://lacodda.com)
