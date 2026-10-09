//! Holds the sealing page of the documentation site to the code.
//!
//! The page is the specification another implementation is written from: it
//! gives every domain string, length and default the client and its sealing
//! crate use. Nothing fails when the two drift - until a client in another
//! language cannot open what this one sealed.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives at crates/efema")
        .to_path_buf()
}

fn read(path: &str) -> String {
    let path = repo_root().join(path);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn the_sealing_page_gives_the_formats_the_code_writes() {
    // Another implementation reads and writes these bytes from the page alone.
    let page = read("docs/src/content/docs/concepts/sealing.md");
    let seal = read("crates/lacodda-seal/src/lib.rs");
    let envelope = read("crates/efema/src/envelope.rs");
    for (domain, source) in [
        ("lacodda-seal/v1/key-id", &seal),
        ("lacodda-seal/v1/sealed", &seal),
        ("lacodda-seal/v1/locked", &seal),
        ("efema/entry/v1", &envelope),
        ("efema/stream-key/v1", &envelope),
        ("efema/key-entry/v1", &envelope),
    ] {
        assert!(source.contains(&format!("b\"{domain}\"")), "the domain {domain} moved in the code");
        assert!(page.contains(&format!("\"{domain}\"")), "the sealing page does not give the domain {domain}");
    }
    for (name, value) in [
        ("SEALED_OVERHEAD", lacodda_seal::SEALED_OVERHEAD),
        ("LOCKED_LEN", lacodda_seal::LOCKED_LEN),
        ("ENTRY_OVERHEAD", efema::ENTRY_OVERHEAD),
        // A lock, then the key's seal over four bytes of generation.
        ("KEY_ENTRY_LEN", lacodda_seal::LOCKED_LEN + lacodda_seal::SEALED_OVERHEAD + 4),
    ] {
        assert!(
            page.contains(&format!("<!-- {name} -->{value} bytes")),
            "the sealing page does not state {name} as {value} bytes"
        );
    }
    for (code, page_says) in [
        ("const FORMAT: u8 = 2;", "format: u8 (2)"),
        ("const CODEC_NONE: u8 = 0;", "| `0` | the item"),
        ("const CODEC_ZSTD: u8 = 1;", "| `1` | one [zstd]"),
        ("const MAX_EXPANDED: usize = limits::MAX_BODY_BYTES;", "at most 16 MiB"),
    ] {
        assert!(envelope.contains(code), "`{code}` moved in the code");
        assert!(page.contains(page_says), "the sealing page does not say `{page_says}`");
    }
    assert_eq!(efema::Limits::V1.max_body, 16 * 1024 * 1024, "the page gives the expansion bound as 16 MiB");
    let default = lacodda_seal::KdfParams::DEFAULT;
    let stated = format!(
        "{} MiB, {} passes, {} lanes",
        default.memory_kib() / 1024,
        default.iterations(),
        default.parallelism()
    );
    assert!(page.contains(&stated), "the sealing page does not give the default Argon2id parameters as {stated}");
}
