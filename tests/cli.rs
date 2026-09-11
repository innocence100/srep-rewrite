#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

fn temp_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!("srep-test-{}-{}", std::process::id(), unique()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_srep"))
}

fn wait_for_temp_file(dir: &Path) -> PathBuf {
    for _ in 0..500 {
        if let Some(entry) = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().contains(".srep.tmp-"))
        {
            return entry.path();
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("temporary output was not observed")
}

#[test]
fn explicit_and_default_paths_round_trip_and_info_verify() {
    let dir = temp_dir();
    let input = dir.join("input.bin");
    let compressed = dir.join("archive.srep");
    let restored = dir.join("restored.bin");
    let data = b"cross-block-value-".repeat(400);
    fs::write(&input, &data).unwrap();
    let status = Command::new(bin())
        .args([
            "compress",
            "--block-size",
            "1KiB",
            input.to_str().unwrap(),
            compressed.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let info = Command::new(bin())
        .args(["info", compressed.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(info.status.success());
    assert!(String::from_utf8_lossy(&info.stdout).contains("blocks:"));
    assert!(
        Command::new(bin())
            .args(["test", compressed.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new(bin())
            .args([
                "decompress",
                compressed.to_str().unwrap(),
                restored.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(fs::read(restored).unwrap(), data);

    let default_input = dir.join("default.dat");
    fs::write(&default_input, &data).unwrap();
    assert!(
        Command::new(bin())
            .args(["compress", default_input.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    let default_archive = dir.join("default.dat.srep");
    assert!(default_archive.exists());
    assert!(
        !Command::new(bin())
            .args(["decompress", default_archive.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new(bin())
            .args(["decompress", "--force", default_archive.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(fs::read(dir.join("default.dat")).unwrap(), data);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stdin_stdout_pipeline_and_force_collision_protection() {
    let data = b"streaming data ".repeat(100);
    let compressed = Command::new(bin())
        .args(["compress", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut compressed = compressed;
    compressed.stdin.take().unwrap().write_all(&data).unwrap();
    let archive = compressed.wait_with_output().unwrap();
    assert!(archive.status.success());
    let mut decompressed = Command::new(bin())
        .args(["decompress", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    decompressed
        .stdin
        .take()
        .unwrap()
        .write_all(&archive.stdout)
        .unwrap();
    let result = decompressed.wait_with_output().unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, data);

    let dir = temp_dir();
    let input = dir.join("same.bin");
    fs::write(&input, b"do not replace").unwrap();
    let output = Command::new(bin())
        .args(["compress", input.to_str().unwrap(), input.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read(&input).unwrap(), b"do not replace");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn corruption_does_not_leave_partial_destination() {
    let dir = temp_dir();
    let archive = dir.join("bad.srep");
    let output = dir.join("out.bin");
    let mut bytes = vec![0u8; 32];
    bytes[..8].copy_from_slice(b"SREPNG\0\x01");
    bytes[8] = 1;
    bytes[9] = 1;
    bytes[12..16].copy_from_slice(&(1024u32).to_le_bytes());
    bytes[16..20].copy_from_slice(&(4096u32).to_le_bytes());
    fs::write(&archive, bytes).unwrap();
    let result = Command::new(bin())
        .args([
            "decompress",
            archive.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("SREP_E_UNSUPPORTED_VERSION"));
    assert!(!output.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn relative_paths_work_from_current_directory() {
    let dir = temp_dir();
    let input = dir.join("relative-input.bin");
    let data = b"relative path data ".repeat(100);
    fs::write(&input, &data).unwrap();
    let status = Command::new(bin())
        .current_dir(&dir)
        .args(["compress", "relative-input.bin"])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(dir.join("relative-input.bin.srep").exists());
    let status = Command::new(bin())
        .current_dir(&dir)
        .args(["decompress", "--force", "relative-input.bin.srep"])
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read(&input).unwrap(), data);

    let explicit_input = dir.join("explicit-relative.bin");
    fs::write(&explicit_input, &data).unwrap();
    assert!(
        Command::new(bin())
            .current_dir(&dir)
            .args([
                "compress",
                "explicit-relative.bin",
                "explicit-relative.srep"
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new(bin())
            .current_dir(&dir)
            .args([
                "decompress",
                "explicit-relative.srep",
                "explicit-restored.bin"
            ])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(fs::read(dir.join("explicit-restored.bin")).unwrap(), data);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn non_utf8_paths_round_trip_with_native_default_names() {
    let dir = temp_dir();
    let input_name = OsString::from_vec(b"nonutf8-\xff.bin".to_vec());
    let input = dir.join(&input_name);
    let data = b"native path bytes".repeat(40);
    fs::write(&input, &data).unwrap();
    assert!(
        Command::new(bin())
            .current_dir(&dir)
            .args([OsString::from("compress"), input_name.clone()])
            .status()
            .unwrap()
            .success()
    );
    let mut archive_name = input_name.clone();
    archive_name.push(".srep");
    assert!(dir.join(&archive_name).exists());
    assert!(
        Command::new(bin())
            .current_dir(&dir)
            .args([
                OsString::from("decompress"),
                OsString::from("--force"),
                archive_name
            ])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(fs::read(input).unwrap(), data);

    let absolute_input = dir.join(OsString::from_vec(b"absolute-\xfe.bin".to_vec()));
    let absolute_output = dir.join(OsString::from_vec(b"absolute-\xfe.archive".to_vec()));
    fs::write(&absolute_input, &data).unwrap();
    assert!(
        Command::new(bin())
            .args([
                OsString::from("compress"),
                absolute_input.clone().into_os_string(),
                absolute_output.clone().into_os_string(),
            ])
            .status()
            .unwrap()
            .success()
    );
    let absolute_restored = dir.join(OsString::from_vec(b"absolute-\xfe.restored".to_vec()));
    assert!(
        Command::new(bin())
            .args([
                OsString::from("decompress"),
                absolute_output.into_os_string(),
                absolute_restored.clone().into_os_string(),
            ])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(fs::read(absolute_restored).unwrap(), data);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn no_force_existing_symlink_is_not_replaced() {
    let dir = temp_dir();
    let input = dir.join("source.bin");
    let target = dir.join("target.bin");
    let output = dir.join("archive.srep");
    fs::write(&input, b"source".repeat(100)).unwrap();
    fs::write(&target, b"target").unwrap();
    std::os::unix::fs::symlink(&target, &output).unwrap();
    let result = Command::new(bin())
        .args([
            "compress",
            input.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(!result.success());
    assert_eq!(fs::read(&target).unwrap(), b"target");
    assert!(
        fs::symlink_metadata(&output)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn no_force_publication_does_not_clobber_destination_created_after_temp() {
    let dir = temp_dir();
    let output = dir.join("race-output.srep");
    let mut child = Command::new(bin())
        .args(["compress", "-", output.to_str().unwrap()])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let temp = wait_for_temp_file(&dir);
    fs::write(&output, b"protected").unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"race data ".repeat(1000).as_slice())
        .unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert_eq!(fs::read(&output).unwrap(), b"protected");
    assert!(!temp.exists());
    assert_no_staging_files(&dir);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn force_publication_replaces_destination_created_after_temp() {
    let dir = temp_dir();
    let output = dir.join("force-race.srep");
    let mut child = Command::new(bin())
        .args(["compress", "--force", "-", output.to_str().unwrap()])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let temp = wait_for_temp_file(&dir);
    fs::write(&output, b"replace me").unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"force race data".repeat(100).as_slice())
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert!(fs::read(&output).unwrap().starts_with(b"SREPNG"));
    assert!(!temp.exists());
    assert_no_staging_files(&dir);
    fs::remove_dir_all(dir).unwrap();
}

fn assert_no_staging_files(dir: &Path) {
    let leaked: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().contains(".srep.tmp-"))
        .map(|entry| entry.path())
        .collect();
    assert!(leaked.is_empty(), "staging files leaked: {leaked:?}");
}

#[cfg(unix)]
#[test]
fn force_publication_replaces_symlink_not_target() {
    let dir = temp_dir();
    let input = dir.join("source.bin");
    let target = dir.join("target.bin");
    let output = dir.join("output.bin");
    fs::write(&input, b"replacement data".repeat(100)).unwrap();
    fs::write(&target, b"target remains").unwrap();
    std::os::unix::fs::symlink(&target, &output).unwrap();
    let result = Command::new(bin())
        .args([
            "compress",
            "--force",
            input.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(result.success());
    assert_eq!(fs::read(&target).unwrap(), b"target remains");
    assert!(
        !fs::symlink_metadata(&output)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn force_publication_replaces_symlink_to_input_not_input() {
    let dir = temp_dir();
    let input = dir.join("source.bin");
    let output = dir.join("output.bin");
    let original = b"source must remain unchanged".repeat(100);
    fs::write(&input, &original).unwrap();
    std::os::unix::fs::symlink(&input, &output).unwrap();

    let result = Command::new(bin())
        .args([
            "compress",
            "--force",
            input.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .status()
        .unwrap();

    assert!(result.success());
    assert_eq!(fs::read(&input).unwrap(), original);
    assert!(
        !fs::symlink_metadata(&output)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::read(&output).unwrap().starts_with(b"SREPNG"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn directory_destination_is_refused_even_with_force() {
    let dir = temp_dir();
    let input = dir.join("source.bin");
    let output = dir.join("directory");
    fs::write(&input, b"directory destination").unwrap();
    fs::create_dir(&output).unwrap();
    let result = Command::new(bin())
        .args([
            "compress",
            "--force",
            input.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(!result.success());
    assert_eq!(result.code(), Some(19));
    assert!(output.is_dir());
    assert_no_staging_files(&dir);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn hard_link_input_output_collision_is_rejected() {
    let dir = temp_dir();
    let input = dir.join("input.bin");
    let output = dir.join("hard-link.bin");
    let original = b"must not overwrite hard link";
    fs::write(&input, original).unwrap();
    fs::hard_link(&input, &output).unwrap();
    let result = Command::new(bin())
        .args([
            "compress",
            "--force",
            input.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(fs::read(&input).unwrap(), original);
    assert_eq!(fs::read(&output).unwrap(), original);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn distinct_input_and_output_files_are_not_rejected_as_a_collision() {
    let dir = temp_dir();
    let input = dir.join("distinct-input.bin");
    let output = dir.join("distinct-output.srep");
    fs::write(&input, b"distinct files are allowed".repeat(20)).unwrap();
    fs::write(&output, b"old output").unwrap();

    let result = Command::new(bin())
        .args([
            "compress",
            "--force",
            input.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(result.status.success());
    assert!(fs::read(&output).unwrap().starts_with(b"SREPNG"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn truncated_archive_is_rejected_without_partial_destination() {
    let dir = temp_dir();
    let input = dir.join("input.bin");
    let archive = dir.join("archive.srep");
    let output = dir.join("out.bin");
    fs::write(&input, b"truncation test ".repeat(100)).unwrap();
    assert!(
        Command::new(bin())
            .args([
                "compress",
                input.to_str().unwrap(),
                archive.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success()
    );
    let mut bytes = fs::read(&archive).unwrap();
    bytes.truncate(bytes.len() - 1);
    fs::write(&archive, bytes).unwrap();
    let result = Command::new(bin())
        .args([
            "decompress",
            archive.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!output.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn layout_and_checksum_cli_round_trips() {
    let dir = temp_dir();
    let input = dir.join("input.bin");
    let data = b"layout checksum matrix".repeat(50);
    fs::write(&input, &data).unwrap();
    for (layout, checksum) in [
        ("index", "xxh3"),
        ("future", "blake3"),
        ("io", "xxh3"),
        ("index", "blake3"),
        ("future", "xxh3"),
        ("io", "blake3"),
    ] {
        let archive = dir.join(format!("archive-{layout}-{checksum}.srep"));
        let restored = dir.join(format!("restored-{layout}-{checksum}.bin"));
        assert!(
            Command::new(bin())
                .args([
                    "compress",
                    "--layout",
                    layout,
                    "--checksum",
                    checksum,
                    "--block-size",
                    "1KiB",
                    input.to_str().unwrap(),
                    archive.to_str().unwrap(),
                ])
                .status()
                .unwrap()
                .success()
        );
        let info = Command::new(bin())
            .args(["info", archive.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(info.status.success());
        let stdout = String::from_utf8_lossy(&info.stdout);
        assert!(stdout.contains("format: SREP-NG v2"));
        assert!(stdout.contains(&format!("layout: {layout}")));
        assert!(stdout.contains(&format!("checksum: {checksum}")));
        assert!(!stdout.contains("semantic matches: 0"));
        assert!(
            Command::new(bin())
                .args(["test", archive.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new(bin())
                .args([
                    "decompress",
                    archive.to_str().unwrap(),
                    restored.to_str().unwrap()
                ])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(fs::read(&restored).unwrap(), data);
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn m0_and_default_m3_cli_find_matches() {
    let dir = temp_dir();
    let input = dir.join("input.bin");
    let m0_archive = dir.join("m0.srep");
    let m3_archive = dir.join("m3.srep");
    let data = b"0123456789abcdef".repeat(32);
    fs::write(&input, &data).unwrap();

    assert!(
        Command::new(bin())
            .args([
                "compress",
                "-m0",
                "--min-match",
                "8",
                input.to_str().unwrap(),
                m0_archive.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success()
    );
    let m0_info = Command::new(bin())
        .args(["info", m0_archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(m0_info.status.success());
    let m0_stdout = String::from_utf8_lossy(&m0_info.stdout);
    assert!(m0_stdout.contains("method: m0"));
    assert!(m0_stdout.contains("semantic matches: "));
    assert!(!m0_stdout.contains("semantic matches: 0"));

    assert!(
        Command::new(bin())
            .args([
                "compress",
                "--min-match",
                "8",
                input.to_str().unwrap(),
                m3_archive.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success()
    );
    let m3_info = Command::new(bin())
        .args(["info", m3_archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(m3_info.status.success());
    assert!(String::from_utf8_lossy(&m3_info.stdout).contains("method: m3"));
    assert!(!String::from_utf8_lossy(&m3_info.stdout).contains("semantic matches: 0"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn m0_cli_stdin_and_layout_checksum_matrix_round_trip() {
    let dir = temp_dir();
    let data = b"0123456789abcdef".repeat(80);
    for (layout, checksum) in [
        ("index", "xxh3"),
        ("index", "blake3"),
        ("future", "xxh3"),
        ("future", "blake3"),
        ("io", "xxh3"),
        ("io", "blake3"),
    ] {
        let archive = dir.join(format!("m0-{layout}-{checksum}.srep"));
        let restored = dir.join(format!("m0-{layout}-{checksum}.out"));
        let mut child = Command::new(bin())
            .args([
                "compress",
                "-m0",
                "--layout",
                layout,
                "--checksum",
                checksum,
                "--block-size",
                "1KiB",
                "--min-match",
                "512",
                "-",
                archive.to_str().unwrap(),
            ])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&data).unwrap();
        assert!(child.wait().unwrap().success(), "{layout}/{checksum}");

        let info = Command::new(bin())
            .args(["info", archive.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(info.status.success());
        let stdout = String::from_utf8_lossy(&info.stdout);
        assert!(stdout.contains("method: m0"));
        assert!(!stdout.contains("semantic matches: 0"));
        assert!(
            Command::new(bin())
                .args([
                    "decompress",
                    archive.to_str().unwrap(),
                    restored.to_str().unwrap()
                ])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(fs::read(restored).unwrap(), data);
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn m1_and_m2_cli_select_real_finders_and_round_trip() {
    let dir = temp_dir();
    let input = dir.join("cdc-input.bin");
    let data = b"0123456789abcdef".repeat(512);
    fs::write(&input, &data).unwrap();
    for method in ["m1", "m2"] {
        let archive = dir.join(format!("{method}.srep"));
        let restored = dir.join(format!("{method}.out"));
        let status = Command::new(bin())
            .args([
                "compress",
                &format!("-{method}"),
                "--target-chunk",
                "32",
                "--min-match",
                "32",
                input.to_str().unwrap(),
                archive.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "{method}");
        let info = Command::new(bin())
            .args(["info", archive.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(info.status.success());
        let stdout = String::from_utf8_lossy(&info.stdout);
        assert!(stdout.contains(&format!("method: {method}")));
        assert!(!stdout.contains("semantic matches: 0"));
        assert!(
            Command::new(bin())
                .args([
                    "decompress",
                    archive.to_str().unwrap(),
                    restored.to_str().unwrap()
                ])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(fs::read(&restored).unwrap(), data);
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn v2_checksum_failure_does_not_leave_partial_destination() {
    let dir = temp_dir();
    let input = dir.join("input.bin");
    let archive = dir.join("archive.srep");
    let output = dir.join("out.bin");
    fs::write(&input, b"checksum failure ".repeat(40)).unwrap();
    assert!(
        Command::new(bin())
            .args([
                "compress",
                input.to_str().unwrap(),
                archive.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success()
    );
    let mut bytes = fs::read(&archive).unwrap();
    let flip = bytes.len() - 70;
    bytes[flip] ^= 0xff;
    fs::write(&archive, bytes).unwrap();
    let result = Command::new(bin())
        .args([
            "decompress",
            archive.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!output.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn prototype_v1_cli_rejection_uses_stable_exit_code() {
    let dir = temp_dir();
    let archive = dir.join("old.srep");
    let output = dir.join("out.bin");
    fs::write(&archive, b"SREPNG\0\x01").unwrap();
    let result = Command::new(bin())
        .args([
            "decompress",
            archive.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("srep: SREP_E_UNSUPPORTED_VERSION: unsupported archive version"));
    assert!(!output.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sidecar_is_checked_after_configuration_and_never_opened() {
    let dir = temp_dir();
    let input = dir.join("input");
    let sidecar = dir.join("must-not-be-opened");
    fs::write(&input, b"input").unwrap();
    let invalid = Command::new(bin())
        .args([
            "compress",
            "--block-size",
            "1B",
            &format!("--index={}", sidecar.display()),
            input.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(!sidecar.exists());

    let valid = Command::new(bin())
        .args([
            "compress",
            &format!("--index={}", sidecar.display()),
            input.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(valid.status.code(), Some(4));
    assert!(!sidecar.exists());
    fs::remove_dir_all(dir).unwrap();
}
