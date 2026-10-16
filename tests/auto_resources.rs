//! Real-subprocess regression tests for `srep compress` automatic resource
//! defaults and the `srep: resources:` stderr diagnostic.
//!
//! These tests exercise the actual release CLI surface over a real pipeline:
//!
//! * an ordinary regular file gets `temp_limit=32 GiB (auto)` and a positive
//!   automatic memory budget whose origin is `auto` or `fallback`;
//! * explicit `--memory`/`--temp-limit` win per resource and report `explicit`;
//! * stdin (pipe) and stdin redirected from a regular file both stay unknown
//!   and get `temp_limit=32 GiB`, proving stdin is never fstat'd;
//! * the archive goes to stdout while the resources line stays on stderr;
//! * `--quiet` suppresses the resources line;
//! * non-compress commands emit no auto resources line.
//!
//! The tests are serialized with a process-local mutex so the single automatic
//! memory run cannot overlap the others and spike peak RSS. No test fabricates
//! a fixed 256 MiB/4 GiB budget that the binary did not actually apply.

#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;
const AUTO_TEMP_32GIB: u64 = 32 * GIB;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_srep"))
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn temp_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "srep-auto-resources-{tag}-{}-{}",
        std::process::id(),
        unique()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

/// Serializes the tests in this file. `compress` with automatic memory probes
/// half of physical RAM; running several of those concurrently could spike peak
/// RSS, so they must not overlap.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poison| poison.into_inner())
}

struct Resources {
    memory: u64,
    memory_origin: String,
    temp_limit: u64,
    temp_limit_origin: String,
}

/// Parse exactly `srep: resources: memory=<bytes> (<origin>) temp_limit=<bytes> (<origin>)`.
fn parse_resources(stderr: &str) -> Option<Resources> {
    let mut lines = stderr
        .lines()
        .filter(|line| line.starts_with("srep: resources: "));
    let line = lines.next()?;
    assert!(lines.next().is_none(), "duplicate resources line in stderr");
    let rest = line.strip_prefix("srep: resources: ")?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    if tokens.len() != 4 {
        return None;
    }
    let memory = tokens[0].strip_prefix("memory=")?.parse::<u64>().ok()?;
    let memory_origin = tokens[1].strip_prefix('(')?.strip_suffix(')')?.to_owned();
    let temp_limit = tokens[2].strip_prefix("temp_limit=")?.parse::<u64>().ok()?;
    let temp_limit_origin = tokens[3].strip_prefix('(')?.strip_suffix(')')?.to_owned();
    for origin in [&memory_origin, &temp_limit_origin] {
        assert!(
            matches!(origin.as_str(), "auto" | "explicit" | "fallback"),
            "unexpected origin {origin:?}"
        );
    }
    Some(Resources {
        memory,
        memory_origin,
        temp_limit,
        temp_limit_origin,
    })
}

fn decompress(archive: &Path, restored: &Path) -> std::process::Output {
    Command::new(bin())
        .arg("decompress")
        .arg(archive)
        .arg(restored)
        .output()
        .expect("spawn srep decompress")
}

fn sample_data() -> Vec<u8> {
    b"auto-resource-defaults ".repeat(8)
}

fn compress_file(input: &Path, archive: &Path, extra: &[&str]) -> std::process::Output {
    let mut command = Command::new(bin());
    command.arg("compress").args(extra).arg(input).arg(archive);
    command.output().expect("spawn srep compress")
}

fn assert_roundtrip(archive: &Path, restored: &Path, data: &[u8]) {
    let output = decompress(archive, restored);
    assert!(
        output.status.success(),
        "decompress failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(restored).unwrap(), data);
}

/// A fallback origin must always be accompanied by a stderr warning that embeds
/// the actual resolved byte budget.  The exact prose is core-owned, so only the
/// stable contract (`srep: warning:` prefix + byte count) is asserted.
fn assert_fallback_warning(stderr: &str, bytes: u64) {
    let warning = stderr
        .lines()
        .find(|line| line.starts_with("srep: warning: "))
        .unwrap_or_else(|| panic!("fallback origin must emit an stderr warning: {stderr}"));
    assert!(
        warning.contains(&bytes.to_string()),
        "fallback warning must embed the resolved budget {bytes}: {warning}"
    );
}

#[test]
fn ordinary_file_auto_temp_is_32gib_and_roundtrips() {
    let _guard = serial();
    let dir = temp_dir("auto-file");
    let input = dir.join("input.bin");
    let archive = dir.join("archive.srep");
    let restored = dir.join("restored.bin");
    let data = sample_data();
    fs::write(&input, &data).unwrap();

    let output = compress_file(&input, &archive, &[]);
    assert!(
        output.status.success(),
        "compress failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "file->file compress must not write the archive to stdout"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let resources = parse_resources(&stderr).expect("resources line on stderr");
    // An ordinary regular file gets the fixed 32 GiB unknown/derived floor.
    assert_eq!(resources.temp_limit, AUTO_TEMP_32GIB);
    assert_eq!(resources.temp_limit_origin, "auto");
    // Automatic memory must be a real, positive budget; the host may legitimately
    // fall back when a visible cgroup level is unreadable, so do not assume
    // half-RAM here.
    assert!(resources.memory > 0, "auto memory budget must be positive");
    assert!(
        matches!(resources.memory_origin.as_str(), "auto" | "fallback"),
        "unexpected auto memory origin {:?}",
        resources.memory_origin
    );
    if resources.memory_origin == "fallback" {
        assert_fallback_warning(&stderr, resources.memory);
    }
    assert_roundtrip(&archive, &restored, &data);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn explicit_memory_and_temp_report_exact_origins() {
    let _guard = serial();
    let dir = temp_dir("explicit");
    let input = dir.join("input.bin");
    let archive = dir.join("archive.srep");
    let restored = dir.join("restored.bin");
    let data = sample_data();
    fs::write(&input, &data).unwrap();

    let output = compress_file(
        &input,
        &archive,
        &["--memory", "64MiB", "--temp-limit", "8GiB"],
    );
    assert!(
        output.status.success(),
        "compress failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let resources = parse_resources(&stderr).expect("resources line on stderr");
    assert_eq!(resources.memory, 64 * MIB);
    assert_eq!(resources.memory_origin, "explicit");
    assert_eq!(resources.temp_limit, 8 * GIB);
    assert_eq!(resources.temp_limit_origin, "explicit");
    // Explicit memory emits no automatic-memory warning.
    assert!(
        !stderr.contains("srep: warning: "),
        "no fallback warning expected: {stderr}"
    );
    assert_roundtrip(&archive, &restored, &data);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mixed_flags_are_independent_per_resource() {
    let _guard = serial();
    let dir = temp_dir("mixed");
    let data = sample_data();

    // Explicit memory only: temp remains automatic at 32 GiB.
    let input_a = dir.join("a.bin");
    let archive_a = dir.join("a.srep");
    fs::write(&input_a, &data).unwrap();
    let output_a = compress_file(&input_a, &archive_a, &["--memory", "64MiB"]);
    assert!(output_a.status.success());
    let stderr_a = String::from_utf8_lossy(&output_a.stderr);
    let resources_a = parse_resources(&stderr_a).expect("resources line");
    assert_eq!(resources_a.memory, 64 * MIB);
    assert_eq!(resources_a.memory_origin, "explicit");
    assert_eq!(resources_a.temp_limit, AUTO_TEMP_32GIB);
    assert_eq!(resources_a.temp_limit_origin, "auto");

    // Explicit temp only: memory stays automatic (auto or fallback), and an
    // automatic-memory warning, if any, is not suppressed by --temp-limit.
    let input_b = dir.join("b.bin");
    let archive_b = dir.join("b.srep");
    fs::write(&input_b, &data).unwrap();
    let output_b = compress_file(&input_b, &archive_b, &["--temp-limit", "8GiB"]);
    assert!(output_b.status.success());
    let stderr_b = String::from_utf8_lossy(&output_b.stderr);
    let resources_b = parse_resources(&stderr_b).expect("resources line");
    assert_eq!(resources_b.temp_limit, 8 * GIB);
    assert_eq!(resources_b.temp_limit_origin, "explicit");
    assert!(resources_b.memory > 0);
    assert!(matches!(
        resources_b.memory_origin.as_str(),
        "auto" | "fallback"
    ));
    if resources_b.memory_origin == "fallback" {
        assert_fallback_warning(&stderr_b, resources_b.memory);
    }

    // Both archives are valid regardless of which resource was explicit.
    assert_roundtrip(&archive_a, &dir.join("a.out"), &data);
    assert_roundtrip(&archive_b, &dir.join("b.out"), &data);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stdin_pipe_temp_is_32gib_and_stdout_archive_is_separate() {
    let _guard = serial();
    let dir = temp_dir("stdin-pipe");
    let data = sample_data();

    let mut child = Command::new(bin())
        .args(["compress", "--memory", "64MiB", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn srep compress - -");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&data)
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait compress");
    assert!(
        output.status.success(),
        "stdin compress failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let resources = parse_resources(&stderr).expect("resources line on stderr");
    assert_eq!(resources.temp_limit, AUTO_TEMP_32GIB);
    assert_eq!(resources.temp_limit_origin, "auto");
    // Archive bytes are on stdout; diagnostics are only on stderr.
    assert!(!output.stdout.is_empty(), "stdout must carry the archive");
    assert!(
        !output.stdout.starts_with(b"srep: resources"),
        "diagnostics must not leak into the archive stream"
    );
    assert!(stderr.contains("srep: resources:"));

    let mut decode = Command::new(bin())
        .args(["decompress", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn srep decompress - -");
    decode
        .stdin
        .take()
        .unwrap()
        .write_all(&output.stdout)
        .expect("write archive to decompress stdin");
    let decoded = decode.wait_with_output().expect("wait decompress");
    assert!(
        decoded.status.success(),
        "decompress failed: {}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    assert_eq!(decoded.stdout, data);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stdin_redirected_from_regular_file_stays_unknown_32gib() {
    let _guard = serial();
    let dir = temp_dir("stdin-redirect");
    let input = dir.join("regular.bin");
    let archive = dir.join("redirect.srep");
    let data = sample_data();
    fs::write(&input, &data).unwrap();

    // The child's stdin is an open regular file, but because the argument is
    // `-` the binary must treat it as unknown and never fstat it for the size.
    let output = Command::new(bin())
        .args(["compress", "--memory", "64MiB", "-"])
        .arg(&archive)
        .stdin(Stdio::from(File::open(&input).unwrap()))
        .output()
        .expect("spawn srep with redirected stdin");
    assert!(
        output.status.success(),
        "redirected-stdin compress failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let resources = parse_resources(&stderr).expect("resources line on stderr");
    assert_eq!(resources.temp_limit, AUTO_TEMP_32GIB);
    assert_eq!(resources.temp_limit_origin, "auto");

    let verify = Command::new(bin())
        .arg("test")
        .arg(&archive)
        .output()
        .expect("spawn srep test");
    assert!(verify.status.success());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn quiet_suppresses_resources_line_but_archive_still_roundtrips() {
    let _guard = serial();
    let dir = temp_dir("quiet");
    let input = dir.join("input.bin");
    let archive = dir.join("archive.srep");
    let restored = dir.join("restored.bin");
    let data = sample_data();
    fs::write(&input, &data).unwrap();

    let output = compress_file(&input, &archive, &["--quiet", "--memory", "64MiB"]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("srep: resources:"),
        "--quiet must suppress the resources line: {stderr}"
    );
    assert_roundtrip(&archive, &restored, &data);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn non_compress_commands_emit_no_auto_resources_line() {
    let _guard = serial();
    let dir = temp_dir("noncompress");
    let input = dir.join("input.bin");
    let archive = dir.join("archive.srep");
    let restored = dir.join("restored.bin");
    let data = sample_data();
    fs::write(&input, &data).unwrap();
    assert!(
        compress_file(&input, &archive, &["--memory", "64MiB"])
            .status
            .success()
    );

    for command in ["info", "test"] {
        let output = Command::new(bin())
            .arg(command)
            .arg(&archive)
            .output()
            .expect("spawn srep");
        assert!(output.status.success(), "{command} failed");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("srep: resources:"),
            "{command} must not emit an auto resources line: {stderr}"
        );
    }

    let decompressed = decompress(&archive, &restored);
    assert!(decompressed.status.success());
    let stderr = String::from_utf8_lossy(&decompressed.stderr);
    assert!(
        !stderr.contains("srep: resources:"),
        "decompress must not emit an auto resources line: {stderr}"
    );
    assert_eq!(fs::read(&restored).unwrap(), data);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn fifo_input_is_non_regular_and_gets_32gib_temp() {
    let _guard = serial();
    let dir = temp_dir("fifo");
    let fifo = dir.join("input.fifo");
    let archive = dir.join("fifo.srep");
    let restored = dir.join("fifo.out");
    let data = sample_data();

    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("this posix test requires an external `mkfifo`");
    assert!(status.success(), "mkfifo failed: {status}");

    let child = Command::new(bin())
        .args(["compress", "--memory", "64MiB"])
        .arg(&fifo)
        .arg(&archive)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn srep compress <fifo>");
    // The child blocks opening the FIFO for reading until a writer appears.
    let mut writer = OpenOptions::new()
        .write(true)
        .open(&fifo)
        .expect("open fifo write end");
    writer.write_all(&data).expect("write fifo");
    drop(writer);

    let output = child.wait_with_output().expect("wait fifo compress");
    assert!(
        output.status.success(),
        "fifo compress failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let resources = parse_resources(&stderr).expect("resources line on stderr");
    assert_eq!(resources.temp_limit, AUTO_TEMP_32GIB);
    assert_eq!(resources.temp_limit_origin, "auto");
    assert_roundtrip(&archive, &restored, &data);
    fs::remove_dir_all(dir).unwrap();
}
