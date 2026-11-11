//! The installers, held to what the line learned about them the hard way.
//!
//! They only ever run on somebody else's machine, after a release, so a
//! mistake in them arrives as a confused operator rather than a failed build.
//! Each test here is a mistake one of the line's tools shipped once.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Root of the repository, two levels above this crate.
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives at crates/efema-server")
        .to_path_buf()
}

fn read(installer: &str) -> String {
    let path = root().join("tools").join(installer);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// An installer without its comment lines: the comments explaining a rule
/// name what the rule forbids.
fn code(installer: &str) -> String {
    read(installer).lines().map(str::trim).filter(|line| !line.starts_with('#')).collect::<Vec<_>>().join("\n")
}

/// The prefix of the installers' environment, as the line's release engine
/// derives it from the program it installs: `EFEMA_SERVER` for
/// `efema-server`. The engine sets `<PREFIX>_VERSION`, `<PREFIX>_INSTALL_DIR`
/// and `<PREFIX>_NO_PATH` when it proves a release; an installer that reads
/// other names installs the latest release into the operator's own place.
fn env_prefix() -> String {
    env!("CARGO_PKG_NAME").to_ascii_uppercase().replace('-', "_")
}

#[test]
fn the_installers_read_the_names_the_release_engine_sets() {
    let prefix = env_prefix();
    for installer in ["install.sh", "install.ps1"] {
        let code = code(installer);
        for name in ["VERSION", "INSTALL_DIR", "NO_PATH"] {
            let variable = format!("{prefix}_{name}");
            assert!(code.contains(&variable), "{installer} never reads {variable}");
        }
    }
}

/// The names the installers read before 0.10.0, refused rather than ignored:
/// `sudo EFEMA_INSTALL_DIR=/usr/local/bin sh` from an old guide would
/// otherwise install into root's home and leave the service on its old
/// binary, with every line of output saying all went well.
///
/// Run rather than read: the refusal has to come before anything is
/// downloaded, which only running it shows.
#[test]
fn the_installers_refuse_the_names_they_used_to_read() {
    let prefix = env_prefix();
    for (old, new) in [("EFEMA_VERSION", "VERSION"), ("EFEMA_INSTALL_DIR", "INSTALL_DIR")] {
        let (succeeded, said) = run_installer(&[(old, "v0.9.0")]);
        assert!(!succeeded, "with {old} set, the installer went on:\n{said}");
        assert!(
            said.contains(&format!("{prefix}_{new}")),
            "with {old} set, the installer does not name {prefix}_{new}:\n{said}"
        );
        assert!(!said.contains("Downloading"), "with {old} set, the installer downloaded before refusing:\n{said}");
    }
}

/// The version the release engine pins is the one installed. An installer
/// that read some other name would install the latest release instead - the
/// same program, a different number, and a proof that passes or fails by
/// luck. A pin that is no tag shows it without a download: it is refused,
/// and by what it says.
#[test]
fn the_installers_install_the_version_they_are_given() {
    let prefix = env_prefix();
    let (succeeded, said) = run_installer(&[(&format!("{prefix}_VERSION"), "latest-please")]);
    assert!(!succeeded, "a version that is no tag was installed:\n{said}");
    assert!(said.contains("'latest-please'"), "the installer does not say which version it refused:\n{said}");
    assert!(!said.contains("Downloading"), "the installer downloaded instead of refusing the version:\n{said}");
}

/// Runs the installer of this platform with `env` added - and, should it get
/// as far as installing, into a scratch directory and off the PATH, so a
/// broken installer under test cannot touch this machine's own. Whether it
/// succeeded, and everything it said.
fn run_installer(env: &[(&str, &str)]) -> (bool, String) {
    let prefix = env_prefix();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let mut command = installer_command();
    for name in ["VERSION", "INSTALL_DIR", "NO_PATH"] {
        command.env_remove(format!("{prefix}_{name}"));
    }
    command.env(format!("{prefix}_INSTALL_DIR"), scratch.path()).env(format!("{prefix}_NO_PATH"), "1");
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().expect("cannot run the installer");
    let said = String::from_utf8_lossy(&output.stderr).into_owned() + &String::from_utf8_lossy(&output.stdout);
    (output.status.success(), said)
}

#[cfg(unix)]
fn installer_command() -> Command {
    let mut command = Command::new("sh");
    command.arg(root().join("tools").join("install.sh"));
    command
}

#[cfg(windows)]
fn installer_command() -> Command {
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(root().join("tools").join("install.ps1"));
    command
}

/// Git Bash, MSYS2 and Cygwin all run a `sh` script on Windows. Without a case
/// of its own for them the script answers "no prebuilt efema-server for
/// MINGW64_NT-...", which reads as "there is no Windows build" - when there is
/// one, installed by the other script.
///
/// Run rather than read: a case that exists but sits below the catch-all
/// would satisfy a search of the text and never match. `uname` is replaced by
/// a shell function, so nothing executable is written for the test to run.
#[cfg(unix)]
#[test]
fn the_unix_installer_sends_windows_shells_to_powershell() {
    let installer = root().join("tools").join("install.sh");
    for system in ["MINGW64_NT-10.0-26200", "MSYS_NT-10.0-26200", "CYGWIN_NT-10.0-26200"] {
        let script = ["uname() { case \"$1\" in -s) echo ", system, " ;; *) echo x86_64 ;; esac; }; . \"$0\""].concat();
        let output = Command::new("sh").args(["-c", &script]).arg(&installer).output().expect("cannot run sh");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "on {system}, install.sh went on as if it could install");
        assert!(said.contains("install.ps1"), "on {system}, install.sh does not send the user to install.ps1:\n{said}");
        assert!(!said.contains("No prebuilt"), "on {system}, install.sh says there is no build:\n{said}");
    }
}

/// The fallback an installer suggests has to work: `cargo install` with a
/// name that is not the crate's fails, and a suggestion that fails is worse
/// than none. kasl suggested `kasl` while publishing `kasl-cli`.
#[test]
fn the_installers_suggest_the_crate_that_is_published() {
    let name = env!("CARGO_PKG_NAME");
    for installer in ["install.sh", "install.ps1"] {
        for (number, line) in read(installer).lines().enumerate() {
            let Some(at) = line.find("cargo install ") else { continue };
            let named = line[at + "cargo install ".len()..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
            assert_eq!(
                named,
                name,
                "{installer} line {} suggests `cargo install {named}`, but the crate is `{name}`",
                number + 1
            );
        }
    }
}

/// The Windows installer adds itself to the user PATH, which is almost always
/// an expandable string: entries like `%JAVA_HOME%\bin` are stored unexpanded.
/// `[Environment]::SetEnvironmentVariable` writes it back as a plain string,
/// and every such entry becomes literal text - one installer of the line did.
/// Comments are left out of the search: the one explaining this names the call.
#[test]
fn the_windows_installer_keeps_the_path_expandable() {
    let code = code("install.ps1");
    assert!(
        !code.contains("SetEnvironmentVariable"),
        "install.ps1 calls SetEnvironmentVariable, which turns the PATH into a plain string"
    );
    assert!(
        code.contains("DoNotExpandEnvironmentNames") && code.contains("ExpandString"),
        "install.ps1 has to read the PATH unexpanded and write it back as an expandable string"
    );
}

/// `<PREFIX>_NO_PATH` leaves the PATH alone. The release check that proves an
/// installer works installs into a scratch directory, and an installer that
/// edits the PATH anyway leaves that directory on the user's PATH for good.
/// The flag has to be read before the first edit, not after it.
#[test]
fn the_installers_leave_the_path_alone_when_asked() {
    let flag = format!("{}_NO_PATH", env_prefix());

    let ps1 = code("install.ps1");
    let read_at = ps1.find(&format!("$env:{flag}")).unwrap_or_else(|| panic!("install.ps1 never reads {flag}"));
    let edit_at = ps1.find("OpenSubKey(\"Environment\"").expect("install.ps1 edits the user PATH");
    assert!(read_at < edit_at, "install.ps1 reads {flag} only after it has edited the PATH");

    let sh = code("install.sh");
    let read_at = sh.find(&flag).unwrap_or_else(|| panic!("install.sh never reads {flag}"));
    let note_at = sh.find("to your PATH").expect("install.sh says where the program went");
    assert!(read_at < note_at, "install.sh reads {flag} only after it has talked about the PATH");
}

/// Windows will not overwrite a program that is running, but it will rename
/// it. An installer that copies over the old exe fails exactly when it is
/// needed most: upgrading a relay while it serves.
#[test]
fn the_windows_installer_moves_a_running_program_aside() {
    let ps1 = code("install.ps1");
    let aside = ps1.find("Move-Item $target").expect("install.ps1 never moves the installed program aside");
    let copy = ps1.find("Copy-Item").expect("install.ps1 copies the program into place");
    assert!(aside < copy, "install.ps1 moves the old program aside only after copying over it");
    assert!(ps1.contains(".exe.old-*"), "install.ps1 never cleans up the programs it moved aside");
}
