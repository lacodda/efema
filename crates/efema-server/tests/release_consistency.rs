//! Guards the facts that must agree before a version is published.
//!
//! efema ships to GitHub, crates.io and a documentation site, and each renders
//! its own copy of what the product is. They drift silently: nothing fails when
//! a crate's manifest still says 0.1.0 after the others moved on, when the
//! protocol page lists a limit the relay no longer has, or when a command is
//! renamed and its page is not. The drift shows only after publishing, when it
//! cannot be taken back. These checks run with every `cargo test`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use efema_proto::limits;
use efema_proto::wire::ProblemCode;

/// Root of the repository, two levels above this crate.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives at crates/efema-server")
        .to_path_buf()
}

fn read(path: impl AsRef<Path>) -> String {
    let path = repo_root().join(path);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// A `key = "value"` from one `[section]` of a manifest. Naive on purpose: one
/// block is all these checks need, and a TOML parser is a dependency to carry.
fn manifest_field(file: &str, section: &str, key: &str) -> String {
    let mut inside = false;
    for line in read(file).lines() {
        let line = line.trim();
        if line.starts_with('[') {
            if inside {
                break;
            }
            inside = line == section;
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        // Exact match, so `rust-version` cannot answer a lookup for `version`.
        if inside && name.trim() == key {
            return value.trim().trim_matches('"').to_string();
        }
    }
    panic!("`{key}` not found in the {section} block of {file}");
}

/// The members of the workspace, as paths from the root.
fn members() -> Vec<String> {
    let manifest = read("Cargo.toml");
    let block = manifest.split("members = [").nth(1).expect("the workspace lists its members");
    let block = &block[..block.find(']').expect("the members list ends")];
    let members: Vec<String> =
        block.split(',').map(|m| m.trim().trim_matches('"')).filter(|m| !m.is_empty()).map(str::to_string).collect();
    assert!(members.len() >= 4, "could not read the workspace members: {members:?}");
    members
}

/// The crates published to crates.io, by name: every member under `crates/`.
/// The rest - the examples - must say `publish = false`.
fn published() -> Vec<String> {
    let crates: Vec<String> = members().iter().filter_map(|m| m.strip_prefix("crates/")).map(str::to_string).collect();
    for member in members().iter().filter(|m| !m.starts_with("crates/")) {
        assert_eq!(
            manifest_field(&format!("{member}/Cargo.toml"), "[package]", "publish"),
            "false",
            "{member} is outside crates/ and would be published"
        );
    }
    crates
}

/// The crates released with efema's version. The line's crates built here -
/// `lacodda-seal`, `lacodda-tear` - carry their own
/// (docs/adr/0008-lacodda-seal-in-this-workspace.md,
/// docs/adr/0015-lacodda-tear-in-this-workspace.md).
fn efema_crates() -> Vec<String> {
    published().into_iter().filter(|c| c.starts_with("efema")).collect()
}

/// The line's crates that live in this workspace, each with a version and a
/// README of its own.
fn line_crates() -> Vec<String> {
    let line: Vec<String> = published().into_iter().filter(|c| c.starts_with("lacodda-")).collect();
    assert_eq!(
        line.len() + efema_crates().len(),
        published().len(),
        "a published crate is neither efema's nor the line's"
    );
    line
}

fn help(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_efema-server")).args(args).output().unwrap();
    String::from_utf8(output.stdout).unwrap()
}

/// The subcommands `efema-server --help` lists, without clap's own `help`.
fn commands() -> Vec<String> {
    let commands: Vec<String> = help(&["--help"])
        .lines()
        .skip_while(|line| !line.starts_with("Commands:"))
        .skip(1)
        .take_while(|line| line.starts_with("  ") && !line.trim().is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_string)
        .collect();
    assert!(commands.len() >= 2, "could not read the commands out of --help: {commands:?}");
    commands
}

fn docs_pages() -> Vec<(String, String)> {
    fn walk(dir: &Path, found: &mut Vec<(String, String)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.extension().is_some_and(|e| e == "md" || e == "mdx") {
                found.push((path.display().to_string(), fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let mut found = Vec::new();
    walk(&repo_root().join("docs/src/content/docs"), &mut found);
    assert!(found.len() > 5, "the documentation pages were not found");
    found
}

#[test]
fn every_crate_carries_the_one_version() {
    let version = manifest_field("Cargo.toml", "[workspace.package]", "version");
    for member in efema_crates() {
        let manifest = format!("crates/{member}/Cargo.toml");
        assert_eq!(
            manifest_field(&manifest, "[package]", "version.workspace"),
            "true",
            "{manifest} sets a version of its own; the efema crates are released together"
        );
    }
    // The workspace depends on its own crates at the versions they carry: a
    // stale one publishes a crate that asks for its sibling's previous release.
    let workspace = read("Cargo.toml");
    for member in published() {
        let expected = if line_crates().contains(&member) {
            manifest_field(&format!("crates/{member}/Cargo.toml"), "[package]", "version")
        } else {
            version.clone()
        };
        let dependency = workspace
            .lines()
            .find(|line| line.starts_with(&format!("{member} = ")))
            .unwrap_or_else(|| panic!("the workspace does not declare {member}"));
        assert!(
            dependency.contains(&format!("version = \"{expected}\"")),
            "the workspace depends on {member} at another version than {expected}: {dependency}"
        );
    }
}

#[test]
fn the_docs_site_describes_the_product_as_the_manifest_does() {
    let description = manifest_field("Cargo.toml", "[workspace.package]", "description");
    assert!(
        read("docs/astro.config.mjs").contains(&format!("const description = \"{description}\";")),
        "docs/astro.config.mjs describes the product differently from Cargo.toml"
    );
    assert!(
        read("docs/src/content/docs/index.md").contains(&format!("description: \"{description}\"")),
        "the front page of the docs describes the product differently from Cargo.toml"
    );
}

#[test]
fn readme_is_shared_rather_than_duplicated() {
    for member in efema_crates() {
        let manifest = format!("crates/{member}/Cargo.toml");
        assert_eq!(
            manifest_field(&manifest, "[package]", "readme"),
            "../../README.md",
            "{manifest} does not publish the root README"
        );
        assert!(
            !repo_root().join(format!("crates/{member}/README.md")).exists(),
            "crates/{member} has a README of its own"
        );
    }
    // The line's crates are other products to their readers, each with a
    // README of its own.
    for member in line_crates() {
        assert_eq!(manifest_field(&format!("crates/{member}/Cargo.toml"), "[package]", "readme"), "README.md");
    }
}

#[test]
fn readme_links_resolve_off_github() {
    // crates.io renders the same file with no repository to resolve a relative
    // path against: the banner becomes a broken image, the links 404.
    links_are_absolute("README.md");
    for member in line_crates() {
        links_are_absolute(&format!("crates/{member}/README.md"));
    }
}

fn links_are_absolute(readme: &str) {
    for (number, line) in read(readme).lines().enumerate() {
        for marker in ["src=\"", "href=\"", "]("] {
            for target in line.split(marker).skip(1) {
                let end = target.find(['"', ')']).unwrap_or(target.len());
                let target = &target[..end];
                assert!(
                    target.starts_with("https://") || target.starts_with('#'),
                    "{readme} line {}: `{target}` does not resolve on crates.io; use an absolute URL",
                    number + 1
                );
            }
        }
    }
}

#[test]
fn every_command_shown_exists() {
    // A transcript of a command that is gone sends a reader to an error. Only
    // lines typed at a prompt inside a fenced block count.
    let commands = commands();
    let mut sources = vec![("README.md".to_string(), read("README.md"))];
    sources.extend(docs_pages());
    for (source, text) in sources {
        let mut in_block = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                in_block = !in_block;
                continue;
            }
            let Some(rest) = line.trim().strip_prefix("$ efema-server ").filter(|_| in_block) else {
                continue;
            };
            let word = rest.split_whitespace().next().unwrap_or_default();
            if word.starts_with('-') || word.starts_with('<') {
                continue;
            }
            assert!(commands.iter().any(|c| c == word), "{source} shows `efema-server {word}`, which is not a command");
        }
    }
}

#[test]
fn every_command_has_a_reference_page_and_every_page_a_command() {
    let commands = commands();
    let reference = repo_root().join("docs/src/content/docs/reference");
    for command in &commands {
        assert!(
            reference.join(format!("{command}.md")).is_file(),
            "`efema-server {command}` has no page at docs/src/content/docs/reference/{command}.md"
        );
    }
    // The pages that are not about one command.
    let overviews = ["commands", "protocol"];
    for entry in fs::read_dir(&reference).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_str().unwrap().to_string();
        assert!(
            overviews.contains(&name.as_str()) || commands.contains(&name),
            "reference/{name}.md documents `efema-server {name}`, which is not a command"
        );
    }
}

#[test]
fn the_default_address_is_documented_as_the_binary_has_it() {
    let default = "127.0.0.1:2451";
    assert!(help(&["serve", "--help"]).contains(&format!("[default: {default}]")), "serve's default address moved");
    for page in ["docs/src/content/docs/reference/serve.md", "docs/src/content/docs/getting-started.md"] {
        assert!(read(page).contains(default), "{page} does not state the default address {default}");
    }
}

/// The rows of the markdown table that follows `heading` on the protocol page.
fn protocol_rows(heading: &str) -> Vec<Vec<String>> {
    let page = read("docs/src/content/docs/reference/protocol.md");
    let section = page.split(heading).nth(1).unwrap_or_else(|| panic!("the protocol page has no `{heading}`"));
    section
        .lines()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .skip(2)
        .map(|line| line.trim_matches('|').split('|').map(|cell| cell.trim().trim_matches('`').to_string()).collect())
        .collect()
}

#[test]
fn the_protocol_page_names_every_refusal_and_no_other() {
    let documented: Vec<String> = protocol_rows("## Refusals").into_iter().map(|row| row[1].clone()).collect();
    for code in ProblemCode::KNOWN {
        assert!(documented.iter().any(|d| d == code.as_str()), "the protocol page does not document `{code}`");
    }
    for code in &documented {
        assert!(
            ProblemCode::KNOWN.iter().any(|known| known.as_str() == code),
            "the protocol page documents `{code}`, which the relay never sends"
        );
    }
}

#[test]
fn the_protocol_page_states_the_limits_the_relay_enforces() {
    let rows = protocol_rows("## Ceilings and defaults");
    let value = |name: &str| -> String {
        rows.iter().find(|row| row[0] == name).unwrap_or_else(|| panic!("no `{name}` row in the limits table"))[1]
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    let expected = [
        ("request body, at most and by default", limits::MAX_BODY_BYTES.to_string()),
        ("entry, by default", limits::ENTRY_BYTES_DEFAULT.to_string()),
        ("space quota, by default", limits::SPACE_QUOTA_DEFAULT.to_string()),
        ("requests per key in a minute, by default", limits::RATE_DEFAULT.to_string()),
        ("entries in a page, by default", limits::PAGE_ENTRIES_DEFAULT.to_string()),
        ("entries in a page, at most", limits::PAGE_ENTRIES_MAX.to_string()),
        ("entry data in a page", limits::PAGE_BYTES.to_string()),
        ("wait, by default", limits::WAIT_DEFAULT.as_secs().to_string()),
        ("wait, at most", limits::WAIT_MAX.as_secs().to_string()),
        ("streams in one wait", limits::WATCHES_MAX.to_string()),
        ("stream name", efema_proto::MAX_NAME_LEN.to_string()),
    ];
    for (name, number) in expected {
        assert_eq!(value(name), number, "the protocol page says {name} is {}, the relay has {number}", value(name));
    }
    assert_eq!(rows.len(), 11, "the limits table has a row this test does not check");
}

#[test]
fn the_protocol_page_gives_every_field_of_the_announced_limits() {
    // Every key the relay sends, in order, and no other: an encoded record
    // is a map of as many keys as the table has rows.
    let rows = protocol_rows("### limits");
    let keys: Vec<String> = rows.iter().map(|row| row[0].clone()).collect();
    let expected: Vec<String> = (0..rows.len()).map(|key| key.to_string()).collect();
    assert_eq!(keys, expected, "the limits table skips or repeats a key");
    let encoded = efema_proto::wire::encode(&efema_proto::wire::Limits::DEFAULT);
    assert_eq!(usize::from(encoded[0] & 0x1f), rows.len(), "the record has another number of fields than the page");
}

/// The long options `efema-server <args> --help` lists, without clap's own.
fn options(args: &[&str]) -> Vec<String> {
    let mut args = args.to_vec();
    args.push("--help");
    let options: Vec<String> = help(&args)
        .lines()
        .filter_map(|line| line.split_whitespace().find(|word| word.starts_with("--")))
        .map(|word| word.trim_end_matches(',').to_string())
        .filter(|option| option != "--help" && option != "--version")
        .collect();
    assert!(!options.is_empty(), "could not read the options of {args:?}");
    options
}

#[test]
fn every_option_is_on_its_commands_page() {
    for (args, page) in [
        (vec!["serve"], "serve"),
        (vec!["streams"], "streams"),
        (vec!["keys"], "keys"),
        (vec!["keys", "add"], "keys"),
        (vec!["keys", "revoke"], "keys"),
    ] {
        let text = read(format!("docs/src/content/docs/reference/{page}.md"));
        for option in options(&args) {
            assert!(text.contains(&format!("`{option}")), "reference/{page}.md does not document {option} of {args:?}");
        }
    }
}

#[test]
fn the_documented_chain_is_the_implemented_one() {
    // The chain's construction is in every client's cursor; the page that tells
    // other implementations how to compute it must not drift from the code.
    let domain = "efema/chain/v1/genesis";
    assert!(read("crates/efema-proto/src/chain.rs").contains(&format!("b\"{domain}\"")), "the chain's domain moved");
    let page = read("docs/src/content/docs/reference/protocol.md");
    assert!(page.contains(&format!("\"{domain}\"")), "the protocol page does not give the chain's domain");
    assert!(
        page.contains("SHA-256(link(n-1) || seq: u64 || epoch: u32 || length(data): u64 || data)"),
        "the protocol page's link formula is not the one in efema-proto"
    );
}

#[test]
fn the_documented_batch_key_is_the_implemented_one() {
    // Every relay keeps the keys of the batches it wrote, and a transport
    // without a relay (S3) computes them itself: the formula is a contract.
    let domain = "efema/batch/v1";
    assert!(read("crates/efema-proto/src/wire.rs").contains(&format!("b\"{domain}\"")), "the batch key's domain moved");
    let page = read("docs/src/content/docs/reference/protocol.md");
    assert!(
        page.contains(&format!(
            "SHA-256(\"{domain}\" || epoch: u32 || count: u64 || (length(data): u64 || data) for each entry)"
        )),
        "the protocol page's batch key is not the one in efema-proto"
    );
}

#[test]
fn every_crate_is_published_in_dependency_order() {
    let publish = read(".github/workflows/publish.yml");
    let line = publish.lines().find(|l| l.trim().starts_with("for crate in ")).expect("publish.yml loops over crates");
    let listed: Vec<&str> =
        line.trim().trim_start_matches("for crate in ").trim_end_matches("; do").split_whitespace().collect();
    // A crate comes after every crate it depends on, dev-dependencies
    // included: crates.io resolves them when it accepts a package.
    let order = ["efema-proto", "lacodda-seal", "lacodda-tear", "efema-server", "efema"];
    assert_eq!(listed, order, "publish.yml must publish every crate, each after what it depends on");
    let mut crates = published();
    crates.sort();
    let mut expected: Vec<String> = order.iter().map(|c| c.to_string()).collect();
    expected.sort();
    assert_eq!(crates, expected, "a crate is published that the order above does not place");
    for (index, member) in order.iter().enumerate() {
        let manifest = read(format!("crates/{member}/Cargo.toml"));
        for later in &order[index + 1..] {
            assert!(
                !manifest.lines().any(|l| l.starts_with(&format!("{later}.workspace"))),
                "{member} depends on {later}, which publish.yml publishes after it"
            );
        }
    }
}

#[test]
fn the_release_packages_the_binary_the_workspace_builds() {
    let release = read(".github/workflows/release.yml");
    assert!(release.contains("--package efema-server"), "release.yml does not build efema-server");
    assert!(release.contains("release/efema-server.exe") && release.contains("release/efema-server\""));
    assert!(release.contains("aarch64-unknown-linux-gnu"), "the release lost the ARM Linux build a home server needs");
}

#[test]
fn the_changelog_has_the_version_being_released() {
    let version = manifest_field("Cargo.toml", "[workspace.package]", "version");
    assert!(
        read("CHANGELOG.md").contains(&format!("## [{version}]")),
        "CHANGELOG.md has no section for {version}: generate it with `git cliff --tag v{version} --output CHANGELOG.md`"
    );
}

/// The text between `start` and the next line that is `end`, both excluded.
fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text.find(start).unwrap_or_else(|| panic!("no {start:?}")) + start.len();
    let rest = &text[from..];
    let to = rest.find(&format!("\n{end}\n")).unwrap_or_else(|| panic!("no {end:?} after {start:?}"));
    rest[..to].trim_start_matches('\n')
}

#[test]
fn the_service_page_shows_the_unit_the_script_writes() {
    // An operator reads the unit on the page and runs the script that writes
    // it: two copies of one file, held to one here.
    let script = read("tools/service.sh");
    let unit = between(&script, "<<'UNIT'", "UNIT");
    assert!(unit.starts_with("[Unit]"), "the unit in tools/service.sh was not found: {unit:?}");
    let page = read("docs/src/content/docs/guides/on-a-server.md");
    let shown = between(&page, "```ini", "```");
    assert_eq!(shown, unit, "the unit on guides/on-a-server.md is not the one tools/service.sh writes");
}

#[test]
fn every_installer_asks_for_an_archive_the_release_makes() {
    let release = read(".github/workflows/release.yml");
    let built: Vec<&str> = release.lines().filter_map(|l| l.trim().strip_prefix("target: ")).map(str::trim).collect();
    assert!(built.len() >= 4, "could not read the release targets: {built:?}");

    let unix = read("tools/install.sh");
    let asked: Vec<&str> = unix.lines().filter_map(|l| l.split("TARGET=\"").nth(1)?.split('"').next()).collect();
    assert!(asked.len() >= 3, "could not read the targets install.sh asks for: {asked:?}");
    for target in &asked {
        assert!(built.contains(target), "install.sh asks for {target}, which release.yml does not build");
    }
    assert!(unix.contains("$NAME.tar.gz") && release.contains("tar czf \"$name.tar.gz\""));

    let windows = read("tools/install.ps1");
    let target = windows
        .lines()
        .find_map(|l| l.trim().strip_prefix("$name = \"efema-server-$tag-")?.strip_suffix('"'))
        .expect("install.ps1 names its archive");
    assert!(built.contains(&target), "install.ps1 asks for {target}, which release.yml does not build");
    assert!(windows.contains("$name.zip") && release.contains("\"$name.zip\""));
}
