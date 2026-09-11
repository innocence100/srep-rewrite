use md5::{Digest, Md5};
use srep::{
    ErrorKind, ResourceConfig, decompress_with_resources, inspect_with_resources,
    verify_with_resources,
};

const ORIGINAL: &[u8] = include_bytes!("fixtures/legacy/original.bin");
const HISTORICAL_112: &[u8] = include_bytes!("fixtures/legacy/historical-112.srep");
const HISTORICAL_1675: &[u8] = include_bytes!("fixtures/legacy/historical-1675.srep");

macro_rules! legacy_rows {
    ($body:ident) => {{
        $body!(1, md5);
        $body!(1, none);
        $body!(1, sha1);
        $body!(1, sha512);
        $body!(1, vmac);
        $body!(1, siphash);
        $body!(2, md5);
        $body!(2, none);
        $body!(2, sha1);
        $body!(2, sha512);
        $body!(2, vmac);
        $body!(2, siphash);
        $body!(3, md5);
        $body!(3, none);
        $body!(3, sha1);
        $body!(3, sha512);
        $body!(3, vmac);
        $body!(3, siphash);
        $body!(4, md5);
        $body!(4, none);
        $body!(4, sha1);
        $body!(4, sha512);
        $body!(4, vmac);
        $body!(4, siphash);
    }};
}

macro_rules! run_row {
    ($version:literal, $hash:ident) => {{
        let archive = include_bytes!(concat!(
            "fixtures/legacy/v",
            stringify!($version),
            "-",
            stringify!($hash),
            ".srep"
        ));
        let resources = ResourceConfig::default();
        let mut output = Vec::new();
        decompress_with_resources(&archive[..], &mut output, &resources).unwrap();
        assert_eq!(output, ORIGINAL, "v{} {}", $version, stringify!($hash));
        let stats = verify_with_resources(&archive[..], &resources).unwrap();
        assert_eq!(stats.original_size, ORIGINAL.len() as u64);
        assert!(stats.block_count > 0);
        let info = inspect_with_resources(&archive[..], &resources).unwrap();
        assert_eq!(info.version, $version);
        assert!(info.legacy_layout.is_some());
        assert!(info.legacy_checksum.is_some());
        assert!(
            info.semantic_match_count > 0,
            "fixture must contain matches"
        );
    }};
}

#[test]
fn committed_legacy_matrix_is_hermetic_and_complete() {
    legacy_rows!(run_row);
}

#[test]
fn copied_historical_vhash_fixtures_are_hermetic() {
    for (archive, expected_size) in [(HISTORICAL_112, 8u64), (HISTORICAL_1675, 10_259u64)] {
        let stats = verify_with_resources(archive, &ResourceConfig::default()).unwrap();
        assert_eq!(stats.original_size, expected_size);
        let mut output = Vec::new();
        decompress_with_resources(archive, &mut output, &ResourceConfig::default()).unwrap();
        assert_eq!(output.len(), expected_size as usize);
    }
}

#[test]
fn fixture_manifest_is_repository_relative_and_complete() {
    let manifest = std::str::from_utf8(include_bytes!("fixtures/legacy/manifest.json")).unwrap();
    assert_eq!(manifest.matches("\"checksum_id\":").count(), 29);
    assert!(manifest.contains("\"archive\": \"v1-md5.srep\""));
}

#[test]
fn invalid_descriptor_precedes_missing_declared_seed() {
    let mut archive = [0u8; 16];
    archive[..8].copy_from_slice(&[0x17, 0x18, 0x35, 0x26, 0x53, 0x52, 0x45, 0x50]);
    archive[8..12].copy_from_slice(&0x01ff0101u32.to_le_bytes());
    let error = verify_with_resources(&archive[..], &ResourceConfig::default()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UnknownChecksum);
}

#[test]
fn late_failure_does_not_publish_legacy_output() {
    let mut archive = include_bytes!("fixtures/legacy/v1-md5.srep").to_vec();
    archive[28] ^= 1;
    let mut output = Vec::new();
    assert_eq!(
        decompress_with_resources(&archive[..], &mut output, &ResourceConfig::default())
            .unwrap_err()
            .kind(),
        ErrorKind::ChecksumMismatch
    );
    assert!(output.is_empty());
}

#[test]
fn none_checksum_ignores_opaque_digest_field() {
    let mut archive = include_bytes!("fixtures/legacy/v1-none.srep").to_vec();
    archive[28] ^= 0x5a;
    let mut output = Vec::new();
    decompress_with_resources(&archive[..], &mut output, &ResourceConfig::default()).unwrap();
    assert_eq!(output, ORIGINAL);
}

#[test]
fn legacy_temp_limit_is_enforced_and_released() {
    let archive = include_bytes!("fixtures/legacy/v4-md5.srep");
    let resources = ResourceConfig {
        temp_limit: archive.len() as u64 - 1,
        ..ResourceConfig::default()
    };
    let error = verify_with_resources(archive.as_slice(), &resources).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TempBudgetExceeded);
}

#[test]
fn legacy_output_limit_is_checked_before_reconstruction() {
    let archive = include_bytes!("fixtures/legacy/v1-md5.srep");
    let resources = ResourceConfig {
        output_limit: ORIGINAL.len() as u64 - 1,
        ..ResourceConfig::default()
    };
    let error = verify_with_resources(archive.as_slice(), &resources).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::OutputLimitExceeded);
}

#[test]
fn optional_zero_terminator_is_accepted_only_at_eof() {
    let archive = include_bytes!("fixtures/legacy/v1-md5.srep");
    let mut marked = archive.to_vec();
    marked.extend_from_slice(&[0; 8]);
    let mut output = Vec::new();
    decompress_with_resources(&marked[..], &mut output, &ResourceConfig::default()).unwrap();
    assert_eq!(output, ORIGINAL);

    marked.push(0);
    assert_eq!(
        verify_with_resources(&marked[..], &ResourceConfig::default())
            .unwrap_err()
            .kind(),
        ErrorKind::CorruptRecord
    );
}

fn legacy_header(version: u8, checksum_id: u8, base_len: u32) -> Vec<u8> {
    let mut bytes = vec![0x17, 0x18, 0x35, 0x26, 0x53, 0x52, 0x45, 0x50];
    bytes.extend_from_slice(
        &u32::from(version)
            .wrapping_add(u32::from(checksum_id) << 8)
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&base_len.to_le_bytes());
    bytes
}

#[test]
fn directed_v1_fixture_has_final_trailing_literals() {
    let reconstructed = b"abcdabcdabcdabcd";
    let mut digest = Md5::new();
    digest.update(reconstructed);
    let mut archive = legacy_header(1, 0, 4);
    archive.extend_from_slice(&8u32.to_le_bytes());
    archive.extend_from_slice(&16u32.to_le_bytes());
    archive.extend_from_slice(&12u32.to_le_bytes());
    archive.extend_from_slice(&digest.finalize());
    archive.extend_from_slice(&4u32.to_le_bytes());
    archive.extend_from_slice(&1u32.to_le_bytes());
    archive.extend_from_slice(&1u32.to_le_bytes());
    archive.extend_from_slice(b"abcdabcd");
    let mut output = Vec::new();
    let stats =
        decompress_with_resources(&archive[..], &mut output, &ResourceConfig::default()).unwrap();
    assert_eq!(output, reconstructed);
    assert_eq!(stats.literal_bytes, 8);
}

fn future_stat(gap: u32, distance: u32, length_minus_base: u32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&gap.to_le_bytes());
    bytes[4..8].copy_from_slice(&distance.to_le_bytes());
    bytes[12..].copy_from_slice(&length_minus_base.to_le_bytes());
    bytes
}

#[test]
fn directed_future_same_source_records_are_accepted() {
    for version in [3u8, 4u8] {
        let stats = [future_stat(0, 4, 4), future_stat(0, 8, 4)];
        let literals = b"abcd";
        let mut archive = legacy_header(version, 1, 0);
        archive.extend_from_slice(&4u32.to_le_bytes());
        archive.extend_from_slice(&12u32.to_le_bytes());
        archive.extend_from_slice(&if version == 3 { 32u32 } else { 0u32 }.to_le_bytes());
        archive.extend_from_slice(&[0; 16]);
        if version == 3 {
            archive.extend_from_slice(&stats.concat());
        }
        archive.extend_from_slice(literals);
        if version == 4 {
            archive.extend_from_slice(&stats.concat());
            archive.extend_from_slice(&32u32.to_le_bytes());
            archive.extend_from_slice(&32u32.to_le_bytes());
            archive.extend_from_slice(&0u32.to_le_bytes());
            archive.extend_from_slice(&28u32.to_le_bytes());
            archive.extend_from_slice(&1u32.to_le_bytes());
            archive.extend_from_slice(&0xafbaadacu32.to_le_bytes());
            archive.extend_from_slice(&0xd9cae7e8u32.to_le_bytes());
        }
        let mut output = Vec::new();
        decompress_with_resources(&archive[..], &mut output, &ResourceConfig::default()).unwrap();
        assert_eq!(output, b"abcdabcdabcd", "version {version}");
    }
}

#[test]
fn optional_terminators_have_exact_boundary_rules_for_v1_to_v3() {
    for version in 1..=3u8 {
        let source = match version {
            1 => include_bytes!("fixtures/legacy/v1-md5.srep").to_vec(),
            2 => include_bytes!("fixtures/legacy/v2-md5.srep").to_vec(),
            _ => include_bytes!("fixtures/legacy/v3-md5.srep").to_vec(),
        };
        let mut clean_marker = source.clone();
        clean_marker.extend_from_slice(&[0; 8]);
        assert!(verify_with_resources(&clean_marker[..], &ResourceConfig::default()).is_ok());
        for length in 1..8 {
            let mut partial = source.clone();
            partial.extend(std::iter::repeat_n(0, length));
            assert_eq!(
                verify_with_resources(&partial[..], &ResourceConfig::default())
                    .unwrap_err()
                    .kind(),
                ErrorKind::TruncatedArchive,
                "v{version} partial marker length {length}"
            );
        }
        let mut trailing = clean_marker;
        trailing.push(1);
        assert_eq!(
            verify_with_resources(&trailing[..], &ResourceConfig::default())
                .unwrap_err()
                .kind(),
            ErrorKind::CorruptRecord,
            "v{version} marker trailing byte"
        );
        let mut partial_header = source;
        partial_header.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            verify_with_resources(&partial_header[..], &ResourceConfig::default())
                .unwrap_err()
                .kind(),
            ErrorKind::TruncatedArchive,
            "v{version} nonzero partial header"
        );
    }
}

#[test]
fn future_fragment_sort_delivers_all_two_thousand_one_adjacent_records() {
    let count = 8193u32;
    let mut archive = legacy_header(3, 1, 0);
    archive.extend_from_slice(&2u32.to_le_bytes());
    archive.extend_from_slice(&(count + 2).to_le_bytes());
    archive.extend_from_slice(&(count * 16).to_le_bytes());
    archive.extend_from_slice(&[0; 16]);
    archive.extend_from_slice(&future_stat(0, 1, 1));
    for _ in 1..count {
        archive.extend_from_slice(&future_stat(1, 1, 1));
    }
    archive.extend_from_slice(b"xx");
    let mut output = Vec::new();
    decompress_with_resources(&archive[..], &mut output, &ResourceConfig::default()).unwrap();
    assert_eq!(output.len(), count as usize + 2);
    assert!(output.iter().all(|&byte| byte == b'x'));
}

#[test]
fn terminal_v4_cuts_follow_observable_error_rule() {
    let archive = include_bytes!("fixtures/legacy/v4-md5.srep");
    assert_eq!(
        verify_with_resources(&archive[..23], &ResourceConfig::default())
            .unwrap_err()
            .kind(),
        ErrorKind::TruncatedArchive
    );
    for amount in [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 16, 20, 24, 32, 40, 48, 56, 64, 72, 80,
    ] {
        let shortened = &archive[..archive.len() - amount];
        let error = verify_with_resources(shortened, &ResourceConfig::default()).unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::CorruptIndex,
            "present tail cut {amount}"
        );
    }
}

#[test]
fn v4_physical_edits_follow_observable_error_rule() {
    let archive = include_bytes!("fixtures/legacy/v4-md5.srep");
    for offset in 16..archive.len() {
        let mut deleted = archive.to_vec();
        deleted.remove(offset);
        let error = verify_with_resources(&deleted[..], &ResourceConfig::default()).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                ErrorKind::CorruptIndex | ErrorKind::TruncatedArchive | ErrorKind::ChecksumMismatch
            ),
            "physical deletion at {offset}: {:?}",
            error.kind()
        );
    }
    for offset in 272..archive.len() {
        let mut mutated = archive.to_vec();
        mutated[offset] ^= 1;
        assert_eq!(
            verify_with_resources(&mutated[..], &ResourceConfig::default())
                .unwrap_err()
                .kind(),
            ErrorKind::CorruptIndex,
            "present footer mutation at {offset}"
        );
    }
}

#[test]
fn all_v4_fixture_footer_mutations_are_corrupt_and_internal_edits_are_observable() {
    macro_rules! fixture {
        ($name:literal) => {{
            let archive = include_bytes!(concat!("fixtures/legacy/", $name, ".srep"));
            let footer_start = archive.len() - 24;
            for offset in footer_start..archive.len() {
                let mut mutated = archive.to_vec();
                mutated[offset] ^= 1;
                assert_eq!(
                    verify_with_resources(&mutated[..], &ResourceConfig::default())
                        .unwrap_err()
                        .kind(),
                    ErrorKind::CorruptIndex,
                    "footer mutation {offset} in {}",
                    $name
                );
            }
            for offset in 16..archive.len() {
                let mut deleted = archive.to_vec();
                deleted.remove(offset);
                let error =
                    verify_with_resources(&deleted[..], &ResourceConfig::default()).unwrap_err();
                assert!(
                    matches!(
                        error.kind(),
                        ErrorKind::CorruptIndex
                            | ErrorKind::TruncatedArchive
                            | ErrorKind::ChecksumMismatch
                    ),
                    "deletion {offset} in {}: {:?}",
                    $name,
                    error.kind()
                );
            }
        }};
    }
    fixture!("v4-md5");
    fixture!("v4-none");
    fixture!("v4-sha1");
    fixture!("v4-sha512");
    fixture!("v4-vmac");
    fixture!("v4-siphash");
}

#[test]
fn future_duplicate_destination_is_rejected_without_omission() {
    let mut archive = legacy_header(3, 1, 1);
    archive.extend_from_slice(&4u32.to_le_bytes());
    archive.extend_from_slice(&6u32.to_le_bytes());
    archive.extend_from_slice(&32u32.to_le_bytes());
    archive.extend_from_slice(&[0; 16]);
    archive.extend_from_slice(&future_stat(0, 4, 0));
    archive.extend_from_slice(&future_stat(0, 4, 0));
    archive.extend_from_slice(b"abcd");
    assert_eq!(
        verify_with_resources(&archive[..], &ResourceConfig::default())
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidMatch
    );
}

#[test]
fn legacy_matrix_decodes_with_one_byte_memory() {
    macro_rules! check {
        ($version:literal, $checksum:ident) => {
            let archive = include_bytes!(concat!(
                "fixtures/legacy/v",
                stringify!($version),
                "-",
                stringify!($checksum),
                ".srep"
            ));
            let resources = ResourceConfig {
                memory: 1,
                ..ResourceConfig::default()
            };
            let mut output = Vec::new();
            decompress_with_resources(&archive[..], &mut output, &resources).unwrap();
            assert_eq!(output, ORIGINAL);
        };
    }
    legacy_rows!(check);
}
