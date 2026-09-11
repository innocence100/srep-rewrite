use std::ffi::{OsStr, OsString};
use std::path::{Component, Path};

/// Strip an exact terminal lowercase `.srep` suffix using native path APIs.
///
/// Only the final extension equal to `srep` is removed. `foo.SREP`,
/// `foo.srep.extra`, and a path whose only component is `.srep` are left unchanged.
pub fn strip_srep_suffix(input: &Path) -> Option<OsString> {
    if input.extension() != Some(OsStr::new("srep")) {
        return None;
    }
    if matches!(input.file_name(), Some(name) if name == OsStr::new(".srep")) {
        return None;
    }
    let mut output = input.to_path_buf();
    output.set_extension("");
    if trailing_dot_component(&output) {
        return None;
    }
    Some(output.into_os_string())
}

fn trailing_dot_component(path: &Path) -> bool {
    matches!(path.components().next_back(), Some(Component::CurDir))
        && path.as_os_str().as_encoded_bytes().ends_with(b".")
}

/// Compatibility alias used by older Stage-0 tests; strips `.srep`.
pub fn strip_srep2_suffix(input: &Path) -> Option<OsString> {
    strip_srep_suffix(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn strips_only_exact_terminal_lowercase_srep() {
        assert_eq!(
            strip_srep_suffix(Path::new("foo.bin.srep")).as_deref(),
            Some(OsStr::new("foo.bin"))
        );
        assert_eq!(strip_srep_suffix(Path::new("foo.SREP")), None);
        assert_eq!(strip_srep_suffix(Path::new("foo.srep.extra")), None);
        assert_eq!(strip_srep_suffix(Path::new(".srep")), None);
    }
}
