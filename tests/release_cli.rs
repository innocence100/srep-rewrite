//! Release CLI surface: software version vs archive format, and help completeness.
//!
//! These checks must not create files or otherwise touch the filesystem beyond
//! the process working directory snapshot.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_srep"))
}

fn temp_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "srep-release-cli-{}-{}",
        std::process::id(),
        unique()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn snapshot(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let metadata = entry.metadata().unwrap();
        entries.push((entry.path(), metadata.len()));
    }
    entries.sort();
    entries
}

fn run_in(dir: &Path, arg: &str) -> std::process::Output {
    Command::new(bin())
        .current_dir(dir)
        .arg(arg)
        .output()
        .unwrap_or_else(|error| panic!("failed to spawn srep {arg}: {error}"))
}

fn assert_no_side_effects(dir: &Path, before: &[(PathBuf, u64)]) {
    assert_eq!(
        snapshot(dir),
        before,
        "version/help must not create, delete, or rewrite files in the working directory"
    );
}

fn expected_version_line() -> String {
    format!("srep {}\n", env!("CARGO_PKG_VERSION"))
}

#[test]
fn version_long_and_short_flags_match_cargo_pkg_version_without_io() {
    let dir = temp_dir();
    let before = snapshot(&dir);
    let mut bodies = Vec::new();
    for flag in ["--version", "-V"] {
        let output = run_in(&dir, flag);
        assert!(
            output.status.success(),
            "{flag} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "{flag} must not write stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout, expected_version_line(), "{flag} stdout");
        assert_no_side_effects(&dir, &before);
        bodies.push(stdout);
    }
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(env!("CARGO_PKG_VERSION"), "0.1.0");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn help_lists_seed_size_target_chunk_and_software_vs_format_without_io() {
    let dir = temp_dir();
    let before = snapshot(&dir);
    let mut bodies = Vec::new();
    for flag in ["--help", "-h"] {
        let output = run_in(&dir, flag);
        assert!(
            output.status.success(),
            "{flag} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "{flag} must not write stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains("--seed-size"),
            "{flag} must document --seed-size"
        );
        assert!(
            stdout.contains("--target-chunk"),
            "{flag} must document --target-chunk"
        );
        assert!(
            stdout.contains("Software version"),
            "{flag} must distinguish software version from archive format"
        );
        assert!(
            stdout.contains("archive format"),
            "{flag} must mention archive format"
        );
        assert!(
            stdout.contains("SREP-NG v3"),
            "{flag} must name the default archive format"
        );
        assert!(
            stdout.contains("--version"),
            "{flag} must mention the version flag"
        );
        assert_no_side_effects(&dir, &before);
        bodies.push(stdout);
    }
    assert_eq!(bodies[0], bodies[1], "--help and -h must be identical");
    fs::remove_dir_all(dir).unwrap();
}
