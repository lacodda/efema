//! Holds the pages another implementation is written from - Sealing, Format,
//! and the threat model's list of proofs - to the code.
//!
//! The pages give every domain string, length, default and version the client
//! and its sealing crate use. Nothing fails when the two drift - until a
//! client in another language cannot open what this one sealed.

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
    assert_eq!(efema::Limits::DEFAULT.body as usize, 16 * 1024 * 1024, "the page gives the expansion bound as 16 MiB");
    let default = lacodda_seal::KdfParams::DEFAULT;
    let stated = format!(
        "{} MiB, {} passes, {} lanes",
        default.memory_kib() / 1024,
        default.iterations(),
        default.parallelism()
    );
    assert!(page.contains(&stated), "the sealing page does not give the default Argon2id parameters as {stated}");
}

#[test]
fn the_format_page_gives_each_part_at_the_version_the_code_reads() {
    let page = read("docs/src/content/docs/reference/format.md");
    for (part, source, code, version) in [
        ("PROTOCOL", "crates/efema-server/src/http.rs", ".route(\"/v1/streams/{name}/entries\"", 1),
        ("SEAL", "crates/lacodda-seal/src/lib.rs", "const VERSION: u8 = 1;", 1),
        ("ENTRY", "crates/efema/src/envelope.rs", "const FORMAT: u8 = 2;", 2),
        ("KEY_ENTRY", "crates/efema/src/envelope.rs", "b\"efema/key-entry/v1\"", 1),
        ("LAYOUT", "crates/efema/src/bucket/layout.rs", "pub(crate) const FORMAT: u32 = 1;", 1),
    ] {
        assert!(read(source).contains(code), "`{code}` moved in {source}");
        assert!(
            page.contains(&format!("<!-- {part} -->{version} |")),
            "the format page does not give {part} as {version}"
        );
    }
    // The vectors say which versions they are of, and the page links them.
    let vectors = read("docs/public/format-vectors.txt");
    assert!(
        vectors.starts_with(
            "# efema's format vectors: protocol 1, lacodda-seal format 1, entry format 2,\n# bucket layout 1."
        ),
        "the vectors name other versions than the page"
    );
    assert!(page.contains("](/efema/format-vectors.txt)"), "the format page does not link the vectors");
}

#[test]
fn every_test_the_threat_model_names_is_there() {
    // The page says which test shows each promise. Renamed or deleted, the
    // test would leave the promise pointing at nothing.
    let page = read("docs/src/content/docs/concepts/threat-model.md");
    let section = page.split("## What is shown, not claimed").nth(1).expect("the threat model has its proof section");
    let mut named = 0;
    for row in section.lines().filter(|line| line.starts_with("| ") && line.contains("](https://")) {
        let shown_by = row.rsplit(" | ").next().unwrap();
        for reference in shown_by.split("), ") {
            let name = reference.split('`').nth(1).expect("a test name in backticks");
            let path = reference.split("([").nth(1).and_then(|rest| rest.split(']').next()).expect("a path");
            let source = read(path);
            assert!(
                source.contains(&format!("fn {name}(")),
                "the threat model names {name} in {path}, and it is not there"
            );
            named += 1;
        }
    }
    assert!(named >= 9, "only {named} tests named: the table was not read");
}
