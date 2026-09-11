use std::io::Cursor;

use srep::{
    Checksum, CompressionConfig, ErrorKind, Layout, MatchCandidate, Method, RepConfig,
    ResourceConfig, compress_with_candidates, compress_with_context, find_matches_m3,
    find_matches_m4, inspect_matches,
};

fn config(method: Method) -> CompressionConfig {
    let mut config = CompressionConfig::for_method(method);
    config.block_size = 1024;
    config.min_match = 8;
    config.seed_size = Some(8);
    config.checksum = Checksum::Xxh3;
    config.resources = ResourceConfig {
        memory: 64 * 1024 * 1024,
        temp_limit: 256 * 1024 * 1024,
        ..ResourceConfig::default()
    };
    config
}

fn source(input: &[u8], start: usize, length: usize) -> &[u8] {
    &input[start..start + length]
}

fn m3_oracle(input: &[u8], seed: usize, minimum: usize) -> Vec<MatchCandidate> {
    let mut result = Vec::new();
    for target in 0..=input.len().saturating_sub(seed) {
        let source_starts = (0..=input.len().saturating_sub(seed))
            .step_by(seed)
            .filter(|&position| position < target)
            .collect::<Vec<_>>();
        for source_start in source_starts.into_iter().rev() {
            if source(input, source_start, seed) != source(input, target, seed) {
                continue;
            }
            let distance = target - source_start;
            let mut length = seed;
            while target + length + seed <= input.len()
                && (0..seed).all(|offset| {
                    input[target + length + offset]
                        == input[source_start + (length + offset) % distance]
                })
            {
                length += seed;
            }
            if length >= minimum {
                result.push(MatchCandidate {
                    src: source_start as u64,
                    dst: target as u64,
                    len: length as u64,
                    insertion_ordinal: result.len() as u64,
                });
            }
        }
    }
    result
}

fn m4_oracle(input: &[u8], seed: usize, minimum: usize) -> Vec<MatchCandidate> {
    let mut result = Vec::new();
    for target in 0..=input.len().saturating_sub(seed) {
        let source_starts = (0..=input.len().saturating_sub(seed))
            .step_by(seed)
            .filter(|&position| position < target)
            .collect::<Vec<_>>();
        for source_start in source_starts.into_iter().rev() {
            if source(input, source_start, seed) != source(input, target, seed) {
                continue;
            }
            let distance = target - source_start;
            let mut backward = 0;
            while backward < source_start
                && backward < target
                && input[source_start - backward - 1] == input[target - backward - 1]
            {
                backward += 1;
            }
            let source_shifted = source_start - backward;
            let target_shifted = target - backward;
            let mut length = backward + seed;
            while target_shifted + length < input.len()
                && input[target_shifted + length] == input[source_shifted + length % distance]
            {
                length += 1;
            }
            if length >= minimum {
                result.push(MatchCandidate {
                    src: source_shifted as u64,
                    dst: target_shifted as u64,
                    len: length as u64,
                    insertion_ordinal: result.len() as u64,
                });
            }
        }
    }
    result
}

fn production(input: &[u8], method: Method, minimum: u64, seed: u64) -> Vec<MatchCandidate> {
    let mut config = config(method);
    config.min_match = minimum;
    config.seed_size = Some(seed);
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let candidates = match method {
        Method::M3FixedDigest => find_matches_m3(Cursor::new(input), &config, &context),
        Method::M4Reread => find_matches_m4(Cursor::new(input), &config, &context),
        _ => unreachable!(),
    }
    .unwrap();
    candidates.as_slice().to_vec()
}

#[test]
fn fixed_finders_match_independent_oracles_for_grid_overlap_and_rounding() {
    let input = b"abcabcabcabcXYZabcabcabcabc";
    assert_eq!(
        production(input, Method::M3FixedDigest, 7, 3),
        m3_oracle(input, 3, 7)
    );
    assert_eq!(
        production(input, Method::M4Reread, 8, 8),
        m4_oracle(input, 8, 8)
    );
}

#[test]
fn fixed_finders_apply_inclusive_distance_and_contiguous_ordinals() {
    let input = b"abcdefghabcdefghabcdefgh";
    for method in [Method::M3FixedDigest, Method::M4Reread] {
        let mut config = config(method);
        config.min_match = 8;
        config.seed_size = Some(8);
        config.max_distance = Some(8);
        let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
        let matches = match method {
            Method::M3FixedDigest => find_matches_m3(Cursor::new(input), &config, &context),
            Method::M4Reread => find_matches_m4(Cursor::new(input), &config, &context),
            _ => unreachable!(),
        }
        .unwrap();
        assert!(!matches.is_empty());
        assert!(matches.iter().all(|item| item.dst - item.src <= 8));
        assert!(
            matches
                .iter()
                .enumerate()
                .all(|(ordinal, item)| item.insertion_ordinal == ordinal as u64)
        );

        config.max_distance = Some(7);
        let excluded = match method {
            Method::M3FixedDigest => find_matches_m3(Cursor::new(input), &config, &context),
            Method::M4Reread => find_matches_m4(Cursor::new(input), &config, &context),
            _ => unreachable!(),
        }
        .unwrap();
        assert!(excluded.is_empty());
    }
}

#[test]
fn fixed_finder_context_releases_memory_after_result_drop() {
    for method in [Method::M3FixedDigest, Method::M4Reread] {
        let config = config(method);
        let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
        let matches = match method {
            Method::M3FixedDigest => {
                find_matches_m3(Cursor::new(b"01234567".repeat(20)), &config, &context)
            }
            Method::M4Reread => {
                find_matches_m4(Cursor::new(b"01234567".repeat(20)), &config, &context)
            }
            _ => unreachable!(),
        }
        .unwrap();
        assert!(context.memory.current() > 0);
        drop(matches);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }
}

#[test]
fn fixed_methods_route_real_references_across_layouts_and_checksums() {
    let input = b"0123456789abcdef".repeat(32);
    for method in [Method::M3FixedDigest, Method::M4Reread] {
        let mut expected = None;
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                let mut config = config(method);
                config.layout = layout;
                config.checksum = checksum;
                let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
                let mut archive = Vec::new();
                let stats =
                    compress_with_context(Cursor::new(&input), &mut archive, &config, &context)
                        .unwrap();
                assert!(stats.semantic_match_count > 0);
                assert!(stats.covered_bytes > 0);
                let matches = inspect_matches(archive.as_slice()).unwrap();
                if let Some(expected) = &expected {
                    assert_eq!(expected, &matches.as_slice().to_vec());
                } else {
                    expected = Some(matches.as_slice().to_vec());
                }
                let mut output = Vec::new();
                srep::decompress(archive.as_slice(), &mut output).unwrap();
                assert_eq!(output, input);
            }
        }
    }
}

#[test]
fn overlay_appends_candidates_and_applies_effective_distance() {
    let input = b"01234567local-local-local-local-local-local-local-01234567local-local-local-local-local-local-local-";
    let mut config = config(Method::M3FixedDigest);
    config.min_match = 16;
    config.seed_size = Some(16);
    config.max_distance = Some(20);
    config.rep_overlay = Some(RepConfig {
        distance: 8,
        min_match: 8,
    });
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let mut archive = Vec::new();
    let stats = compress_with_context(Cursor::new(input), &mut archive, &config, &context).unwrap();
    assert!(stats.semantic_match_count > 0);
    let matches = inspect_matches(archive.as_slice()).unwrap();
    assert!(matches.iter().any(|item| item.len >= 8));
    assert!(
        matches
            .iter()
            .all(|item| item.dst - item.src <= config.max_distance.unwrap())
    );
}

#[test]
fn overlay_matrix_round_trips_with_identical_ir() {
    let input = b"01234567local-local-local-local-local-local-local-01234567local-local-local-local-local-local-local-";
    for method in [Method::M3FixedDigest, Method::M4Reread] {
        let mut expected = None;
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                let mut config = config(method);
                config.layout = layout;
                config.checksum = checksum;
                config.min_match = 16;
                config.seed_size = Some(16);
                config.rep_overlay = Some(RepConfig {
                    distance: 128,
                    min_match: 8,
                });
                let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
                let mut archive = Vec::new();
                let stats =
                    compress_with_context(Cursor::new(input), &mut archive, &config, &context)
                        .unwrap();
                assert!(stats.semantic_match_count > 0);
                assert!(stats.covered_bytes > 0);
                let ir = inspect_matches(archive.as_slice())
                    .unwrap()
                    .as_slice()
                    .to_vec();
                if let Some(expected) = &expected {
                    assert_eq!(expected, &ir, "{method:?} {layout:?} {checksum:?}");
                } else {
                    expected = Some(ir);
                }
                let mut output = Vec::new();
                srep::decompress(archive.as_slice(), &mut output).unwrap();
                assert_eq!(output, input);
            }
        }
    }
}

#[test]
fn overlay_ir_is_deterministic_and_base_candidates_win_duplicates() {
    let input = b"012345670123456701234567";
    let mut config = config(Method::M4Reread);
    config.min_match = 8;
    config.seed_size = Some(8);
    config.rep_overlay = Some(RepConfig {
        distance: 64,
        min_match: 8,
    });
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let first = find_matches_m4(Cursor::new(input), &config, &context)
        .unwrap()
        .as_slice()
        .to_vec();
    let second = find_matches_m4(Cursor::new(input), &config, &context)
        .unwrap()
        .as_slice()
        .to_vec();
    assert_eq!(first, second);
    assert!(
        first
            .windows(2)
            .all(|pair| pair[0].insertion_ordinal < pair[1].insertion_ordinal)
    );
    assert!(first.windows(2).any(|pair| {
        (pair[0].src, pair[0].dst, pair[0].len) == (pair[1].src, pair[1].dst, pair[1].len)
    }));
}

#[test]
fn fixed_finder_memory_failure_keeps_output_empty_and_releases_resources() {
    for method in [Method::M3FixedDigest, Method::M4Reread] {
        let mut config = config(method);
        config.resources.memory = 1;
        let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
        let mut output = Vec::new();
        let error = compress_with_context(
            Cursor::new(b"01234567".repeat(32)),
            &mut output,
            &config,
            &context,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded);
        assert!(output.is_empty());
        assert_eq!(context.memory.current(), 0);
        assert!(context.memory.high_water() <= context.memory.limit());
    }
}

#[test]
fn m5_overlay_is_validated_and_runs_through_the_finder() {
    let mut config = config(Method::M5Exhaustive);
    config.min_match = 8;
    config.seed_size = None;
    config.rep_overlay = Some(RepConfig::default());
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let mut output = Vec::new();
    let stats = compress_with_context(
        Cursor::new(b"repeated repeated"),
        &mut output,
        &config,
        &context,
    )
    .unwrap();
    assert_eq!(stats.method, Some(Method::M5Exhaustive));
    assert_eq!(stats.semantic_match_count, 0);
    assert!(!output.is_empty());
}

#[test]
fn effective_minimum_is_the_lower_validated_base_or_overlay_minimum() {
    let mut config = config(Method::M3FixedDigest);
    config.min_match = 40;
    config.seed_size = Some(40);
    assert_eq!(config.effective_min_match().unwrap(), 40);
    config.rep_overlay = Some(RepConfig {
        distance: 64,
        min_match: 26,
    });
    assert_eq!(config.effective_min_match().unwrap(), 26);
}

#[test]
fn candidate_api_uses_effective_minimum_for_overlay_only() {
    let input = b"0123456789abcdefghijklmnopqrstuv0123456789abcdefghijklmnopqrstuv";
    let candidate = MatchCandidate {
        src: 0,
        dst: 32,
        len: 26,
        insertion_ordinal: 0,
    };
    let mut config = config(Method::M3FixedDigest);
    config.min_match = 40;
    config.seed_size = Some(40);
    config.rep_overlay = Some(RepConfig {
        distance: 64,
        min_match: 26,
    });
    let mut archive = Vec::new();
    let stats =
        compress_with_candidates(Cursor::new(input), &mut archive, &config, [candidate]).unwrap();
    assert_eq!(stats.semantic_match_count, 1);
    assert_eq!(stats.covered_bytes, 26);

    config.rep_overlay = None;
    let mut rejected = Vec::new();
    let error = compress_with_candidates(Cursor::new(input), &mut rejected, &config, [candidate])
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidMatch);
    assert!(rejected.is_empty());
}

fn refresh_record_checksum(archive: &mut [u8], offset: usize, payload_len: usize) {
    let header = srep::format::parse_archive_header(&archive[..80]).unwrap();
    let frame: [u8; 12] = archive[offset..offset + 12].try_into().unwrap();
    let payload = &archive[offset + 12..offset + 12 + payload_len];
    let digest = srep::checksum::record_checksum(header.checksum, &frame, payload);
    archive[offset + 12 + payload_len..offset + 12 + payload_len + digest.len()]
        .copy_from_slice(&digest);
}

fn refresh_block_checksum(archive: &mut [u8], offset: usize, payload_len: usize, input: &[u8]) {
    let header = srep::format::parse_archive_header(&archive[..80]).unwrap();
    let frame: [u8; 12] = archive[offset..offset + 12].try_into().unwrap();
    let payload = &archive[offset + 12..offset + 12 + payload_len];
    let digest =
        srep::checksum::block_checksum(header.checksum, &frame, payload, 0, 0, input).unwrap();
    archive[offset + 12 + payload_len..offset + 12 + payload_len + digest.len()]
        .copy_from_slice(&digest);
}

fn record_offsets(archive: &[u8]) -> Vec<(u8, usize, usize)> {
    let header = srep::format::parse_archive_header(&archive[..80]).unwrap();
    let mut offset = 80;
    let mut records = Vec::new();
    while offset + 12 <= archive.len() - 64 {
        let kind = archive[offset];
        let payload_len =
            u64::from_le_bytes(archive[offset + 4..offset + 12].try_into().unwrap()) as usize;
        records.push((kind, offset, payload_len));
        offset += 12 + payload_len + header.checksum.width();
    }
    records
}

#[test]
fn checksum_valid_match_below_effective_minimum_is_invalid_for_every_layout() {
    let input = b"0123456789abcdefghijklmnopqrst".repeat(3);
    let candidates = [
        MatchCandidate {
            src: 0,
            dst: 30,
            len: 30,
            insertion_ordinal: 0,
        },
        MatchCandidate {
            src: 30,
            dst: 60,
            len: 30,
            insertion_ordinal: 1,
        },
    ];
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let mut config = config(Method::M3FixedDigest);
        config.layout = layout;
        config.min_match = 40;
        config.seed_size = Some(40);
        config.rep_overlay = Some(RepConfig {
            distance: 64,
            min_match: 26,
        });
        let mut archive = Vec::new();
        compress_with_candidates(Cursor::new(&input), &mut archive, &config, candidates).unwrap();
        let (kind, offset, payload_len) = record_offsets(&archive)
            .into_iter()
            .find(|(kind, _, _)| *kind == srep::format::RECORD_DATA_BLOCK)
            .unwrap();
        match layout {
            Layout::Index => {
                let (_, index_offset, _) = record_offsets(&archive)
                    .into_iter()
                    .find(|(kind, _, _)| *kind == srep::format::RECORD_INDEX_SECTION)
                    .unwrap();
                archive[index_offset + 12 + 32 + 16..index_offset + 12 + 32 + 24]
                    .copy_from_slice(&25u64.to_le_bytes());
                archive[index_offset + 12 + 32 + 40..index_offset + 12 + 32 + 48]
                    .copy_from_slice(&35u64.to_le_bytes());
                let index_len = u64::from_le_bytes(
                    archive[index_offset + 4..index_offset + 12]
                        .try_into()
                        .unwrap(),
                ) as usize;
                refresh_record_checksum(&mut archive, index_offset, index_len);
            }
            Layout::Future => {
                archive[offset + 12 + 48 + 24..offset + 12 + 48 + 32]
                    .copy_from_slice(&25u64.to_le_bytes());
                refresh_block_checksum(&mut archive, offset, payload_len, &input);
            }
            Layout::Io => {
                let payload_start = offset + 12;
                let payload_end = payload_start + payload_len;
                let mut match_positions = Vec::new();
                let mut match_start = payload_start + 48;
                while match_start < payload_end {
                    if archive[match_start] == 1 {
                        match_positions.push(match_start);
                    }
                    let encoded = u32::from_le_bytes(
                        archive[match_start + 4..match_start + 8]
                            .try_into()
                            .unwrap(),
                    ) as usize;
                    match_start += encoded;
                }
                assert_eq!(match_positions.len(), 2);
                archive[match_positions[0] + 32..match_positions[0] + 40]
                    .copy_from_slice(&25u64.to_le_bytes());
                archive[match_positions[1] + 24..match_positions[1] + 32]
                    .copy_from_slice(&55u64.to_le_bytes());
                archive[match_positions[1] + 32..match_positions[1] + 40]
                    .copy_from_slice(&35u64.to_le_bytes());
                refresh_block_checksum(&mut archive, offset, payload_len, &input);
            }
        }
        let mut output = Vec::new();
        let error = srep::decompress(archive.as_slice(), &mut output).unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::InvalidMatch,
            "{kind} {layout:?}: {:?}",
            error.context()
        );
        assert!(output.is_empty(), "{layout:?} published malformed output");
    }
}

#[test]
fn overlay_effective_minimum_accepts_rep_match_below_base_for_both_fixed_methods() {
    let seed = b"abcdefghijklmnopqrstuvwxyz";
    let input = [seed.as_slice(), seed.as_slice(), b"!"].concat();
    for method in [Method::M3FixedDigest, Method::M4Reread] {
        let mut finder_config = config(method);
        finder_config.min_match = 40;
        finder_config.seed_size = Some(40);
        finder_config.rep_overlay = Some(RepConfig {
            distance: 128,
            min_match: 26,
        });
        let context = srep::ResourceContext::with_resources(&finder_config.resources).unwrap();
        let candidates = match method {
            Method::M3FixedDigest => find_matches_m3(Cursor::new(&input), &finder_config, &context),
            Method::M4Reread => find_matches_m4(Cursor::new(&input), &finder_config, &context),
            _ => unreachable!(),
        }
        .unwrap();
        assert!(
            candidates
                .iter()
                .any(|candidate| { candidate.len >= 26 && candidate.len < 40 }),
            "{method:?}: {:?}",
            candidates.as_slice()
        );
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                let mut archive_config = config(method);
                archive_config.layout = layout;
                archive_config.checksum = checksum;
                archive_config.min_match = 40;
                archive_config.seed_size = Some(40);
                archive_config.rep_overlay = Some(RepConfig {
                    distance: 128,
                    min_match: 26,
                });
                let archive_context =
                    srep::ResourceContext::with_resources(&archive_config.resources).unwrap();
                let mut archive = Vec::new();
                let stats = compress_with_context(
                    Cursor::new(&input),
                    &mut archive,
                    &archive_config,
                    &archive_context,
                )
                .unwrap();
                let inspected = inspect_matches(archive.as_slice()).unwrap();
                assert!(inspected.iter().any(|item| item.len >= 26 && item.len < 40));
                assert_eq!(
                    stats.semantic_match_count,
                    inspected.as_slice().len() as u64
                );
                assert_eq!(
                    stats.covered_bytes,
                    inspected.iter().map(|item| item.len).sum::<u64>()
                );
                let mut output = Vec::new();
                srep::decompress(archive.as_slice(), &mut output).unwrap();
                assert_eq!(output, input);
            }
        }
    }
}

#[test]
fn overlay_effective_minimum_never_weakens_base_finder_when_rep_is_larger() {
    let input = b"0123456789abcdef".repeat(16);
    for method in [Method::M3FixedDigest, Method::M4Reread] {
        let mut config = config(method);
        config.min_match = 8;
        config.seed_size = Some(8);
        config.rep_overlay = Some(RepConfig {
            distance: 128,
            min_match: 40,
        });
        let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
        let candidates = match method {
            Method::M3FixedDigest => find_matches_m3(Cursor::new(&input), &config, &context),
            Method::M4Reread => find_matches_m4(Cursor::new(&input), &config, &context),
            _ => unreachable!(),
        }
        .unwrap();
        assert!(candidates.iter().any(|candidate| candidate.len >= 8));
        assert!(
            candidates
                .iter()
                .all(|candidate| { candidate.len >= 40 || candidate.src % 8 == 0 })
        );
    }
}
