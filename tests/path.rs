use std::ffi::{OsStr, OsString};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use srep::path::strip_srep_suffix;

#[test]
fn strips_only_an_exact_terminal_lowercase_srep_extension() {
    let cases = [
        ("foo.bin.srep", Some("foo.bin")),
        ("relative.srep", Some("relative")),
        ("/absolute.name.srep", Some("/absolute.name")),
        ("stem.with.dots.srep", Some("stem.with.dots")),
        ("foo.srep.extra", None),
        ("foo.SREP", None),
        ("foo.srep ", None),
        (".srep", None),
    ];
    for (input, expected) in cases {
        let actual = strip_srep_suffix(Path::new(input));
        assert_eq!(
            actual.as_deref(),
            expected.map(OsStr::new),
            "input: {input}"
        );
    }
}

#[test]
fn preserves_relative_and_absolute_path_components() {
    assert_eq!(
        strip_srep_suffix(Path::new("dir/file.srep")),
        Some(OsString::from("dir/file"))
    );
    assert_eq!(
        strip_srep_suffix(Path::new("/var/tmp/file.srep")),
        Some(OsString::from("/var/tmp/file"))
    );
}

#[cfg(unix)]
#[test]
fn preserves_non_utf8_stem_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let mut bytes = b"native-\xff.srep".to_vec();
    let input = PathBuf::from(OsString::from_vec(bytes.split_off(0)));
    let expected = OsString::from_vec(b"native-\xff".to_vec());
    assert_eq!(strip_srep_suffix(&input), Some(expected));
}

#[cfg(unix)]
#[test]
fn native_path_helper_closes_the_old_utf8_conversion_gap() {
    use std::os::unix::ffi::OsStringExt;

    fn old_non_unix_behavior(input: &Path) -> Option<OsString> {
        let name = input.file_name()?;
        let bytes = name.as_encoded_bytes();
        let suffix = b".srep";
        if !bytes.ends_with(suffix) {
            return None;
        }
        let stem = std::str::from_utf8(&bytes[..bytes.len() - suffix.len()]).ok()?;
        let mut output = input.to_path_buf();
        output.set_file_name(stem);
        Some(output.into_os_string())
    }

    let input = PathBuf::from(OsString::from_vec(b"native-\xff.srep".to_vec()));
    assert_eq!(old_non_unix_behavior(&input), None);
    assert_eq!(
        strip_srep_suffix(&input),
        Some(OsString::from_vec(b"native-\xff".to_vec()))
    );
}

#[cfg(windows)]
#[test]
fn preserves_unpaired_surrogate_in_windows_stem() {
    use std::os::windows::ffi::OsStringExt;
    let input = OsString::from_wide(&[
        b'n' as u16,
        0xd800,
        b'.' as u16,
        b's' as u16,
        b'r' as u16,
        b'e' as u16,
        b'p' as u16,
    ]);
    let expected = OsString::from_wide(&[b'n' as u16, 0xd800]);
    assert_eq!(strip_srep_suffix(Path::new(&input)), Some(expected));
}

#[cfg(windows)]
#[test]
fn windows_native_stem_default_path_round_trips_or_preserves_helper_result() {
    use std::fs;
    use std::os::windows::ffi::OsStringExt;
    use std::process::Command;

    let directory = std::env::temp_dir().join(format!("srep-path-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let stem = OsString::from_wide(&[
        b'w' as u16,
        0xd800,
        b'.' as u16,
        b'b' as u16,
        b'i' as u16,
        b'n' as u16,
    ]);
    let input = directory.join(&stem);
    let mut archive_name = stem.clone();
    archive_name.push(".srep");
    let archive = directory.join(&archive_name);
    let data = b"windows native path".repeat(20);
    let write_result = fs::write(&input, &data);
    if write_result.is_ok() {
        let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_srep"));
        assert!(
            Command::new(&binary)
                .args([OsString::from("compress"), input.clone().into_os_string()])
                .status()
                .unwrap()
                .success()
        );
        assert!(archive.exists());
        assert_eq!(
            strip_srep_suffix(&archive),
            Some(input.clone().into_os_string())
        );
        assert!(
            Command::new(&binary)
                .args([
                    OsString::from("decompress"),
                    OsString::from("--force"),
                    archive.into_os_string(),
                ])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(fs::read(&input).unwrap(), data);
    } else {
        assert_eq!(
            strip_srep_suffix(Path::new(&archive)),
            Some(input.into_os_string())
        );
    }
    let _ = fs::remove_dir_all(directory);
}
