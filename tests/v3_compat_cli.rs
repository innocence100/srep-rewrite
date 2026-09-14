//! CLI `info` format-family labeling for NG and legacy archives (V3-R5 C).
//!
//! The label must reflect the archive *family*, not the version number alone.
//! Legacy SREP v2/v3 share version numbers with NG releases, so the CLI
//! must use the typed-vs-legacy discriminator to avoid mislabeling legacy
//! archives as SREP-NG.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use srep::{Checksum, CompressionConfig, Layout, Method, ResourceConfig, compress};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_srep"))
}

fn temp_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "srep-v3-compat-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn info_format(bin: &PathBuf, archive: &Path) -> String {
    let output = Command::new(bin)
        .args(["info", archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "info failed for {}: {}",
        archive.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("format: ").map(str::to_owned))
        .expect("info must print a format line")
}

fn v3_config() -> CompressionConfig {
    let mut config = CompressionConfig::for_method(Method::M3FixedDigest);
    config.layout = Layout::Io;
    config.checksum = Checksum::Xxh3;
    config.block_size = 8192;
    config.min_match = 16;
    config.seed_size = Some(16);
    config.resources = ResourceConfig {
        temp_dir: std::env::temp_dir().join("srep-v3-compat-cli-v2"),
        ..ResourceConfig::default()
    };
    config
}

#[test]
fn legacy_fixtures_are_labeled_legacy_across_versions() {
    let binary = bin();
    // Legacy v2 and v3 collide numerically with SREP-NG v2/v3; each must
    // still report the legacy family.
    for version in [1u8, 2, 3, 4] {
        for checksum in ["md5", "none", "sha1", "sha512", "vmac", "siphash"] {
            let relative = format!("tests/fixtures/legacy/v{version}-{checksum}.srep");
            let archive = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(&relative);
            assert!(archive.is_file(), "missing legacy fixture {relative}");
            let format = info_format(&binary, &archive);
            assert!(
                format == format!("legacy SREP v{version}"),
                "legacy v{version}/{checksum} must report the legacy family, got {format}"
            );
        }
    }
}

#[test]
fn ng_v3_writer_archive_is_labeled_srep_ng_v3() {
    let dir = temp_dir();
    let archive = dir.join("ng-v3.srep");
    let input = b"ng v2 family discriminator".repeat(64);
    let mut bytes = Vec::new();
    srep::compress(Cursor::new(&input), &mut bytes, &v3_config()).unwrap();
    assert_eq!(&bytes[..8], b"SREPNG3\0");
    fs::write(&archive, &bytes).unwrap();

    let format = info_format(&bin(), &archive);
    assert_eq!(format, "SREP-NG v3");
    // The NG family carries typed fields, never the legacy string fields.
    let info = srep::inspect(Cursor::new(&bytes)).unwrap();
    assert_eq!(info.version, 3);
    assert!(info.legacy_layout.is_none());
    assert!(info.legacy_checksum.is_none());
    assert_eq!(info.layout, Some(Layout::Io));
    assert_eq!(info.checksum, Some(Checksum::Xxh3));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn default_cli_writer_archive_is_labeled_srep_ng_v3() {
    let dir = temp_dir();
    let archive = dir.join("ng-v3.srep");
    let input = b"ng v3 default writer".repeat(64);
    let config = CompressionConfig {
        resources: ResourceConfig {
            temp_dir: dir.join("temp"),
            ..ResourceConfig::default()
        },
        ..CompressionConfig::default()
    };
    let mut bytes = Vec::new();
    compress(Cursor::new(&input), &mut bytes, &config).unwrap();
    assert_eq!(&bytes[..8], b"SREPNG3\0");
    fs::write(&archive, &bytes).unwrap();

    let format = info_format(&bin(), &archive);
    assert_eq!(format, "SREP-NG v3");
    let info = srep::inspect(Cursor::new(&bytes)).unwrap();
    assert_eq!(info.version, 3);
    assert!(info.legacy_layout.is_none());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cli_compress_default_archive_reports_ng_v3_not_legacy() {
    let dir = temp_dir();
    let input = dir.join("input.bin");
    let archive = dir.join("input.bin.srep");
    fs::write(&input, b"cli default compress family".repeat(64)).unwrap();
    let status = Command::new(bin())
        .args([
            "compress",
            "--block-size",
            "8KiB",
            input.to_str().unwrap(),
            archive.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(info_format(&bin(), &archive), "SREP-NG v3");
    fs::remove_dir_all(dir).unwrap();
}
