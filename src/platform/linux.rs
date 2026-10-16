use super::{HardProbe, MemoryProbe};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const READ_CAP: u64 = 1024 * 1024;
const V1_UNLIMITED: u64 = 1 << 62;

pub(super) fn physical_memory() -> MemoryProbe {
    let Ok(text) = read_bounded(Path::new("/proc/meminfo")) else {
        return MemoryProbe::Unavailable;
    };
    parse_physical_memory(&text)
}

fn parse_physical_memory(text: &str) -> MemoryProbe {
    let mut found = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            if found.is_some() {
                return MemoryProbe::Unavailable;
            }
            let mut fields = rest.split_whitespace();
            let Some(kib) = fields.next().and_then(|s| s.parse::<u64>().ok()) else {
                return MemoryProbe::Unavailable;
            };
            if fields.next() != Some("kB") || fields.next().is_some() {
                return MemoryProbe::Unavailable;
            }
            found = Some(match kib.checked_mul(1024) {
                Some(0) => MemoryProbe::Invalid,
                Some(n) => MemoryProbe::Known(n),
                None => MemoryProbe::Unavailable,
            });
        }
    }
    found.unwrap_or(MemoryProbe::Unavailable)
}

fn read_bounded(path: &Path) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(READ_CAP + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > READ_CAP {
        return Err(std::io::Error::other("proc entry exceeds read cap"));
    }
    String::from_utf8(bytes).map_err(|_| std::io::Error::other("proc entry is not UTF-8"))
}

#[derive(Clone, Debug)]
struct Mount {
    root: PathBuf,
    point: PathBuf,
    v2: bool,
    controllers: Vec<String>,
}

pub(super) fn hard_limits() -> HardProbe {
    let Ok(cgroup) = read_bounded(Path::new("/proc/self/cgroup")) else {
        return HardProbe {
            identified: true,
            known_min: None,
            indeterminate: true,
        };
    };
    let Ok(mountinfo) = read_bounded(Path::new("/proc/self/mountinfo")) else {
        return HardProbe {
            identified: true,
            known_min: None,
            indeterminate: true,
        };
    };
    collect_limits(&cgroup, &mountinfo)
}

fn collect_limits(cgroup: &str, mountinfo: &str) -> HardProbe {
    let (memberships, bad_membership) = parse_memberships(cgroup);
    let (mounts, bad_mount) = parse_mounts(mountinfo);
    let malformed = bad_membership || bad_mount;
    if memberships.is_empty() {
        return HardProbe {
            identified: !cgroup.trim().is_empty() || malformed,
            known_min: None,
            indeterminate: !cgroup.trim().is_empty() || malformed,
        };
    }
    let mut result = HardProbe {
        identified: !memberships.is_empty(),
        known_min: None,
        indeterminate: malformed,
    };
    for (v2, member, controllers) in &memberships {
        let matching_mounts: Vec<_> = mounts
            .iter()
            .filter(|m| {
                m.v2 == *v2 && (*v2 || controllers.iter().any(|c| m.controllers.contains(c)))
            })
            .collect();
        if matching_mounts.is_empty() {
            result.indeterminate = true;
        }
        for mount in matching_mounts {
            result.identified = true;
            let Ok(relative) = member.strip_prefix(&mount.root) else {
                result.indeterminate = true;
                continue;
            };
            let Some(level) = safe_join(&mount.point, relative) else {
                result.indeterminate = true;
                continue;
            };
            let Ok(canonical_point) = fs::canonicalize(&mount.point) else {
                result.indeterminate = true;
                continue;
            };
            let mut current = level;
            loop {
                let file = current.join(if *v2 {
                    "memory.max"
                } else {
                    "memory.limit_in_bytes"
                });
                match read_limit(&file, *v2, &canonical_point) {
                    Limit::Finite(n) => {
                        result.known_min = Some(result.known_min.map_or(n, |old| old.min(n)))
                    }
                    Limit::Unlimited => (),
                    Limit::Unknown => result.indeterminate = true,
                }
                if current == canonical_point {
                    break;
                }
                let Some(parent) = current.parent() else {
                    result.indeterminate = true;
                    break;
                };
                if !parent.starts_with(&canonical_point) {
                    result.indeterminate = true;
                    break;
                }
                current = parent.to_path_buf();
            }
        }
    }
    for (v2, _, _) in &memberships {
        if !mounts.iter().any(|m| m.v2 == *v2) {
            result.indeterminate = true;
        }
    }
    result.indeterminate |= malformed;
    result
}

enum Limit {
    Finite(u64),
    Unlimited,
    Unknown,
}

fn read_limit(path: &Path, v2: bool, canonical_mount: &Path) -> Limit {
    let Ok(canonical_file) = fs::canonicalize(path) else {
        return Limit::Unknown;
    };
    if !canonical_file.starts_with(canonical_mount) {
        return Limit::Unknown;
    }
    let Ok(value) = read_bounded(&canonical_file) else {
        return Limit::Unknown;
    };
    let value = value.trim();
    if v2 && value == "max" {
        return Limit::Unlimited;
    }
    match value.parse::<u64>() {
        Ok(n) if !v2 && n >= V1_UNLIMITED => Limit::Unlimited,
        Ok(n) => Limit::Finite(n),
        Err(_) => Limit::Unknown,
    }
}

fn parse_memberships(text: &str) -> (Vec<(bool, PathBuf, Vec<String>)>, bool) {
    let mut entries = Vec::new();
    let mut malformed = false;
    for line in text.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.splitn(3, ':').collect();
        if fields.len() != 3 {
            malformed = true;
            continue;
        }
        let hierarchy = fields[0].parse::<u32>();
        let controllers = fields[1];
        let path = PathBuf::from(fields[2]);
        let valid_path =
            path.is_absolute() && !path.components().any(|c| c == Component::ParentDir);
        if controllers.is_empty() {
            if hierarchy != Ok(0) || !valid_path {
                malformed = true;
            } else {
                entries.push((true, path, Vec::new()));
            }
        } else if controllers.split(',').any(|c| c == "memory") {
            let valid_controllers = controllers.split(',').all(|c| {
                !c.is_empty()
                    && c.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            });
            if hierarchy.map_or(true, |id| id == 0) || !valid_controllers || !valid_path {
                malformed = true;
            } else {
                entries.push((
                    false,
                    path,
                    controllers.split(',').map(str::to_owned).collect(),
                ));
            }
        } else if hierarchy.map_or(true, |id| id == 0)
            || !valid_path
            || controllers.split(',').any(str::is_empty)
        {
            malformed = true;
        }
    }
    (entries, malformed)
}

fn parse_mounts(text: &str) -> (Vec<Mount>, bool) {
    let mut mounts = Vec::new();
    let mut malformed = false;
    for line in text.lines().filter(|line| !line.is_empty()) {
        let Some((left, right)) = line.split_once(" - ") else {
            if line
                .split_whitespace()
                .any(|field| field == "cgroup" || field == "cgroup2")
            {
                malformed = true;
            }
            continue;
        };
        let lf: Vec<_> = left.split_whitespace().collect();
        let rf: Vec<_> = right.split_whitespace().collect();
        if !matches!(rf.first(), Some(&"cgroup" | &"cgroup2")) {
            continue;
        }
        if rf.len() < 3 {
            malformed = true;
            continue;
        }
        let v2 = rf.first() == Some(&"cgroup2");
        let controllers: Vec<String> = rf
            .get(2)
            .into_iter()
            .flat_map(|s| s.split(','))
            .chain(rf.get(1).into_iter().flat_map(|s| s.split(',')))
            .map(str::to_owned)
            .collect();
        let v1 = rf.first() == Some(&"cgroup") && controllers.iter().any(|c| c == "memory");
        if rf.first() == Some(&"cgroup") && controllers.is_empty() {
            malformed = true;
            continue;
        }
        if !v1 && !v2 {
            continue;
        }
        let (Some(root), Some(point)) = (lf.get(3), lf.get(4)) else {
            malformed = true;
            continue;
        };
        let (Ok(root), Ok(point)) = (unescape(root), unescape(point)) else {
            malformed = true;
            continue;
        };
        let root = PathBuf::from(root);
        let point = PathBuf::from(point);
        if !root.is_absolute()
            || !point.is_absolute()
            || root.components().any(|c| c == Component::ParentDir)
            || point.components().any(|c| c == Component::ParentDir)
        {
            malformed = true;
            continue;
        }
        mounts.push(Mount {
            root,
            point,
            v2,
            controllers,
        });
    }
    (mounts, malformed)
}

fn safe_join(point: &Path, relative: &Path) -> Option<PathBuf> {
    if relative
        .components()
        .any(|c| c == Component::ParentDir || c == Component::RootDir)
    {
        return None;
    }
    let canonical_point = fs::canonicalize(point).ok()?;
    let joined = point.join(relative);
    let mut existing = joined.as_path();
    let mut suffix = Vec::new();
    let canonical_existing = loop {
        match fs::canonicalize(existing) {
            Ok(canonical) => break canonical,
            Err(_) => {
                let name = existing.file_name()?.to_os_string();
                suffix.push(name);
                existing = existing.parent()?;
            }
        }
    };
    if !canonical_existing.starts_with(&canonical_point) {
        return None;
    }
    let mut target = canonical_existing;
    for component in suffix.iter().rev() {
        target.push(component);
    }
    Some(target)
}

fn unescape(value: &str) -> Result<String, ()> {
    let mut result = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if i + 3 >= bytes.len() || !bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
                return Err(());
            }
            let decoded = match &bytes[i + 1..i + 4] {
                b"040" => Some(b' '),
                b"011" => Some(b'\t'),
                b"012" => Some(b'\n'),
                b"134" => Some(b'\\'),
                _ => None,
            };
            result.push(decoded.ok_or(())?);
            i += 4;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(result).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_proc_parser_and_unescape_rules() {
        assert_eq!(unescape("/foo\\040bar\\134baz").unwrap(), "/foo bar\\baz");
        assert!(unescape("/foo\\é").is_err());
        let (traversal, traversal_bad) = parse_memberships("0::/foo/../bar");
        assert!(traversal.is_empty() && traversal_bad);
        let (valid, valid_bad) = parse_memberships("0::/foo");
        assert_eq!(valid.len(), 1);
        assert!(!valid_bad);
        let (valid_plus_bad, malformed) =
            parse_memberships("0::/valid\nmalformed-relevant-memory-membership");
        assert_eq!(valid_plus_bad.len(), 1);
        assert!(malformed);
        let (mounts, malformed_mount) =
            parse_mounts("1 0 0:1 / /valid rw - cgroup2 cgroup rw\n2 0 cgroup2 malformed");
        assert_eq!(mounts.len(), 1);
        assert!(malformed_mount);
    }

    #[test]
    fn physical_memtotal_parser_handles_missing_invalid_zero_overflow_and_duplicates() {
        assert_eq!(
            parse_physical_memory("MemTotal: 123 kB\n"),
            MemoryProbe::Known(123 * 1024)
        );
        assert_eq!(
            parse_physical_memory("MemAvailable: 10 kB\n"),
            MemoryProbe::Unavailable
        );
        assert_eq!(
            parse_physical_memory("MemTotal: nope kB\n"),
            MemoryProbe::Unavailable
        );
        assert_eq!(
            parse_physical_memory("MemTotal: 0 kB\n"),
            MemoryProbe::Invalid
        );
        assert_eq!(
            parse_physical_memory("MemTotal: 18446744073709551615 kB\n"),
            MemoryProbe::Unavailable
        );
        assert_eq!(
            parse_physical_memory("MemTotal: 1 kB\nMemTotal: 2 kB\n"),
            MemoryProbe::Unavailable
        );
    }

    #[test]
    fn collector_walks_visible_levels_and_retains_missing_child_uncertainty() {
        let root = std::env::temp_dir().join(format!(
            "srep-cgroup-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("foo/child")).unwrap();
        std::fs::write(root.join("memory.max"), "3221225472\n").unwrap();
        let mountinfo = format!("1 0 0:1 / {0} rw - cgroup2 cgroup rw", root.display());
        let missing_child = collect_limits("0::/foo/child", &mountinfo);
        assert_eq!(missing_child.known_min, Some(3 * 1024 * 1024 * 1024));
        assert!(missing_child.identified && missing_child.indeterminate);

        std::fs::write(root.join("foo/child/memory.max"), "max\n").unwrap();
        std::fs::write(root.join("foo/memory.max"), "max\n").unwrap();
        std::fs::remove_file(root.join("memory.max")).unwrap();
        let missing_mount_root = collect_limits("0::/foo/child", &mountinfo);
        assert!(missing_mount_root.indeterminate);
        assert_eq!(missing_mount_root.known_min, None);

        std::fs::write(root.join("memory.max"), "max\n").unwrap();
        let all_unlimited = collect_limits("0::/foo/child", &mountinfo);
        assert_eq!(all_unlimited.known_min, None);
        assert!(!all_unlimited.indeterminate);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collector_rejects_unmapped_namespace_root_and_path_escape() {
        let mount = std::env::temp_dir().join(format!(
            "srep-cgroup-namespace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&mount).unwrap();
        let mountinfo = format!("1 0 0:1 / {0} rw - cgroup2 cgroup rw", mount.display());
        let namespace = collect_limits("0::/foo", &mountinfo);
        assert!(namespace.identified && namespace.indeterminate);
        std::fs::remove_dir_all(mount).unwrap();
    }

    #[test]
    fn collector_combines_v1_finite_and_uncertain_v2_and_rejects_escapes() {
        let root = std::env::temp_dir().join(format!(
            "srep-cgroup-mixed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let v1 = root.join("v1");
        let v2 = root.join("v2");
        let outside = root.join("outside");
        std::fs::create_dir_all(&v1).unwrap();
        std::fs::create_dir_all(&v2).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(v1.join("memory.limit_in_bytes"), "3221225472\n").unwrap();
        std::fs::write(v2.join("memory.max"), "malformed\n").unwrap();
        let mountinfo = format!(
            "1 0 0:1 / {0} rw - cgroup cgroup rw,memory\n2 0 0:2 / {1} rw - cgroup2 cgroup rw",
            v1.display(),
            v2.display()
        );
        let mixed = collect_limits("5:memory:/\n0::/", &mountinfo);
        assert_eq!(mixed.known_min, Some(3 * 1024 * 1024 * 1024));
        assert!(mixed.identified && mixed.indeterminate);
        let with_malformed_membership = collect_limits(
            "5:memory:/\n0::/\nmalformed-relevant-memory-membership",
            &mountinfo,
        );
        assert_eq!(with_malformed_membership.known_min, mixed.known_min);
        assert!(with_malformed_membership.indeterminate);

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside, v2.join("escape")).unwrap();
            assert!(safe_join(&v2, Path::new("escape/child")).is_none());
        }
        std::fs::write(v1.join("oversized"), vec![b'x'; (READ_CAP + 10) as usize]).unwrap();
        assert!(read_bounded(&v1.join("oversized")).is_err());
        let canonical_mount = std::fs::canonicalize(&v2).unwrap();
        assert!(matches!(
            read_limit(&v2.join("memory.max"), true, &canonical_mount),
            Limit::Unknown
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            std::fs::remove_file(v2.join("memory.max")).unwrap();
            std::fs::write(outside.join("memory.max"), "0\n").unwrap();
            symlink(outside.join("memory.max"), v2.join("memory.max")).unwrap();
            assert!(matches!(
                read_limit(&v2.join("memory.max"), true, &canonical_mount),
                Limit::Unknown
            ));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn production_limit_reader_classifies_zero_max_v1_sentinel_and_malformed() {
        let root = std::env::temp_dir().join(format!(
            "srep-limit-reader-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("limit");
        let mount = std::fs::canonicalize(&root).unwrap();
        for (text, v2, expected) in [
            ("0\n", true, 0),
            ("0\n", false, 0),
            ("9223372036854771712\n", false, 1),
        ] {
            std::fs::write(&file, text).unwrap();
            match (read_limit(&file, v2, &mount), expected) {
                (Limit::Finite(0), 0) => (),
                (Limit::Unlimited, 1) => (),
                _ => panic!("unexpected limit classification"),
            }
        }
        std::fs::write(&file, "max\n").unwrap();
        assert!(matches!(read_limit(&file, true, &mount), Limit::Unlimited));
        std::fs::write(&file, "malformed\n").unwrap();
        assert!(matches!(read_limit(&file, true, &mount), Limit::Unknown));
        std::fs::remove_dir_all(root).unwrap();
    }
}
