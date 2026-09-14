use std::io::Cursor;

use srep::{
    Checksum, CompressionConfig, ErrorKind, Layout, MatchCandidate, Method, RepConfig,
    ResourceConfig, compress_with_candidates, compress_with_context, find_matches_m3,
    find_matches_m4, inspect_matches, normalize_matches,
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
fn overlay_compact_matches_full_ir_and_archive_for_uniform_runs() {
    let input = [0u8; 64];
    for method in [Method::M3FixedDigest, Method::M4Reread] {
        for (base_min, overlay_min) in [(8, 32), (32, 32), (40, 26)] {
            let mut config = config(method);
            config.min_match = base_min;
            config.seed_size = Some(8);
            config.rep_overlay = Some(RepConfig {
                distance: 32,
                min_match: overlay_min,
            });
            let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
            let full = match method {
                Method::M3FixedDigest => {
                    find_matches_m3(Cursor::new(input.as_slice()), &config, &context)
                }
                Method::M4Reread => {
                    find_matches_m4(Cursor::new(input.as_slice()), &config, &context)
                }
                _ => unreachable!(),
            }
            .unwrap();
            assert!(
                full.windows(2)
                    .all(|pair| pair[0].insertion_ordinal < pair[1].insertion_ordinal),
                "{method:?} min={base_min}/{overlay_min}"
            );
            let mut compact_archive = Vec::new();
            compress_with_context(
                Cursor::new(input.as_slice()),
                &mut compact_archive,
                &config,
                &context,
            )
            .unwrap();
            let mut full_archive = Vec::new();
            compress_with_candidates(
                Cursor::new(input.as_slice()),
                &mut full_archive,
                &config,
                full.iter().copied(),
            )
            .unwrap();
            assert_eq!(
                compact_archive, full_archive,
                "{method:?} min={base_min}/{overlay_min}"
            );
            let full_ir = normalize_matches(
                full.iter().copied(),
                input.len() as u64,
                base_min.min(overlay_min),
            )
            .unwrap();
            let inspected = inspect_matches(compact_archive.as_slice()).unwrap();
            assert_eq!(
                inspected.as_slice(),
                full_ir.as_slice(),
                "{method:?} min={base_min}/{overlay_min}"
            );
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
