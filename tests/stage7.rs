use std::io::Cursor;

use srep::{
    Checksum, CompressionConfig, ErrorKind, Layout, MatchCandidate, Method, RepConfig,
    ResourceConfig, compress_with_candidates_with_context, compress_with_context, find_matches_m0,
    find_matches_m5, m5_seed_size, normalize_matches_with_budget, packed_slice_metadata,
};

fn config(minimum: u64) -> CompressionConfig {
    let mut config = CompressionConfig::for_method(Method::M5Exhaustive);
    config.min_match = minimum;
    config.block_size = 1024;
    config.resources = ResourceConfig {
        memory: 64 * 1024 * 1024,
        temp_limit: 256 * 1024 * 1024,
        ..ResourceConfig::default()
    };
    config
}

fn extend(input: &[u8], source: usize, target: usize, seed: usize) -> MatchCandidate {
    let distance = target - source;
    let mut backward = 0usize;
    while backward < source
        && backward < target
        && input[source - backward - 1] == input[target - backward - 1]
    {
        backward += 1;
    }
    let source = source - backward;
    let target = target - backward;
    let mut length = backward + seed;
    while target + length < input.len()
        && input[target + length] == input[source + length % distance]
    {
        length += 1;
    }
    MatchCandidate {
        src: source as u64,
        dst: target as u64,
        len: length as u64,
        insertion_ordinal: 0,
    }
}

fn m5_oracle(input: &[u8], minimum: usize) -> Vec<MatchCandidate> {
    m5_oracle_with_distance(input, minimum, None).0
}

fn m5_oracle_with_distance(
    input: &[u8],
    minimum: usize,
    max_distance: Option<u64>,
) -> (Vec<MatchCandidate>, u64) {
    let seed = m5_seed_size(minimum as u64).unwrap() as usize;
    if seed == 0 || input.len() < seed {
        return (Vec::new(), 0);
    }
    let mut result = Vec::new();
    let last_source = input.len() - seed;
    for target in 0..=last_source {
        let source_starts: Vec<_> = (0..=last_source)
            .step_by(seed)
            .filter(|&source| source < target)
            .filter(|&source| match max_distance {
                Some(limit) if limit != 0 => (target - source) as u64 <= limit,
                _ => true,
            })
            .collect();
        for source in source_starts.into_iter().rev() {
            if input[source..source + seed] != input[target..target + seed] {
                continue;
            }
            let mut candidate = extend(input, source, target, seed);
            candidate.insertion_ordinal = result.len() as u64;
            if candidate.len >= minimum as u64 {
                result.push(candidate);
            }
        }
    }
    let raw_count = result.len() as u64;
    result.sort_unstable_by(|a, b| {
        (a.src, a.dst, a.len, a.insertion_ordinal).cmp(&(b.src, b.dst, b.len, b.insertion_ordinal))
    });
    result.dedup_by(|a, b| {
        if (a.src, a.dst, a.len) == (b.src, b.dst, b.len) {
            a.insertion_ordinal = a.insertion_ordinal.min(b.insertion_ordinal);
            true
        } else {
            false
        }
    });
    result.sort_unstable_by_key(|candidate| candidate.insertion_ordinal);
    (result, raw_count)
}

fn dedup_exact_triples(mut candidates: Vec<MatchCandidate>) -> Vec<MatchCandidate> {
    candidates.sort_unstable_by(|a, b| {
        (a.src, a.dst, a.len, a.insertion_ordinal).cmp(&(b.src, b.dst, b.len, b.insertion_ordinal))
    });
    candidates.dedup_by(|a, b| {
        if (a.src, a.dst, a.len) == (b.src, b.dst, b.len) {
            a.insertion_ordinal = a.insertion_ordinal.min(b.insertion_ordinal);
            true
        } else {
            false
        }
    });
    candidates.sort_unstable_by_key(|candidate| candidate.insertion_ordinal);
    candidates
}

fn effective_overlay_distance(max_distance: Option<u64>, overlay_distance: u64) -> u64 {
    match max_distance {
        Some(limit) if limit != 0 => limit.min(overlay_distance),
        _ => overlay_distance,
    }
}

fn overlay_m0_candidates(
    input: &[u8],
    overlay_minimum: u64,
    effective_distance: u64,
) -> Vec<MatchCandidate> {
    let mut config = CompressionConfig::for_method(Method::M0Rep);
    config.min_match = overlay_minimum;
    config.max_distance = Some(effective_distance);
    config.block_size = 1024;
    config.resources = ResourceConfig {
        memory: 64 * 1024 * 1024,
        temp_limit: 256 * 1024 * 1024,
        ..ResourceConfig::default()
    };
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    find_matches_m0(Cursor::new(input), &config, &context)
        .unwrap()
        .as_slice()
        .to_vec()
}

fn combined_overlay_oracle(
    input: &[u8],
    base_minimum: u64,
    overlay_minimum: u64,
    max_distance: Option<u64>,
    overlay_distance: u64,
) -> (Vec<MatchCandidate>, Vec<MatchCandidate>, u64) {
    let (base, raw_next) = m5_oracle_with_distance(input, base_minimum as usize, max_distance);
    let overlay = overlay_m0_candidates(
        input,
        overlay_minimum,
        effective_overlay_distance(max_distance, overlay_distance),
    );
    let mut combined = base.clone();
    combined.extend(overlay.iter().map(|candidate| MatchCandidate {
        insertion_ordinal: raw_next + candidate.insertion_ordinal,
        ..*candidate
    }));
    (dedup_exact_triples(combined), base, raw_next)
}

fn unique_seed_distance_input(distance: usize, minimum: usize) -> Vec<u8> {
    let seed = m5_seed_size(minimum as u64).unwrap() as usize;
    assert!(distance >= seed);
    let mut input = vec![0u8; distance + minimum + seed];
    for (index, slot) in input.iter_mut().enumerate() {
        *slot = (index as u8).wrapping_mul(37).wrapping_add(3);
    }
    let prefix = input[..minimum].to_vec();
    input[distance..distance + minimum].copy_from_slice(&prefix);
    input
}

fn production(input: &[u8], minimum: u64) -> Vec<MatchCandidate> {
    let config = config(minimum);
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    find_matches_m5(Cursor::new(input), &config, &context)
        .unwrap()
        .as_slice()
        .to_vec()
}

fn assert_exact_production_for_minimum(input: &[u8], minimum: usize) {
    assert_eq!(
        production(input, minimum as u64),
        m5_oracle(input, minimum),
        "minimum={minimum}"
    );
}

#[test]
fn formula_accepts_exact_power_boundary_minima() {
    for minimum in [2, 3, 6, 7, 8, 15, 16, 511, 512] {
        let seed = m5_seed_size(minimum).unwrap();
        let k = (minimum + 1).ilog2();
        assert_eq!(seed, 1u64 << (k - 1));
        assert!(minimum >= seed * 2 - 1);
    }
}

#[test]
fn every_short_interval_contains_an_aligned_source_seed() {
    for minimum in [2usize, 3, 6, 7, 8, 15, 16, 511, 512] {
        let seed = m5_seed_size(minimum as u64).unwrap() as usize;
        let input_len = minimum + seed * 3 + 5;
        for start in 0..=input_len - minimum {
            let end = start + minimum;
            assert!(
                (0..=input_len - seed)
                    .step_by(seed)
                    .any(|source| source >= start && source + seed <= end)
            );
        }
    }
}

#[test]
fn exhaustive_candidates_match_independent_oracle_for_nonaligned_repeats() {
    let input = b"xxabcabcabcabc--abcabcabcabczz";
    assert_eq!(production(input, 7), m5_oracle(input, 7));
}

#[test]
fn exhaustive_candidates_match_oracle_at_every_required_minimum() {
    for minimum in [2usize, 3, 6, 7, 8, 15, 16, 511, 512] {
        let seed = m5_seed_size(minimum as u64).unwrap() as usize;
        let mut input = Vec::with_capacity(minimum * 2 + seed * 2 + 3);
        input.extend((0..seed).map(|value| (value as u8).wrapping_mul(29)));
        input.extend((0..minimum + seed + 1).map(|value| (value as u8).wrapping_add(41)));
        let seed_prefix = input[..seed.min(input.len())].to_vec();
        let minimum_prefix = input[..minimum.min(input.len())].to_vec();
        input.extend_from_slice(&seed_prefix);
        input.extend_from_slice(&minimum_prefix);
        assert_exact_production_for_minimum(&input, minimum);
        assert!(
            production(&input, minimum as u64)
                .iter()
                .any(|candidate| candidate.len >= minimum as u64)
        );
    }
}

#[test]
fn nonaligned_required_minima_have_a_confirmed_witness() {
    for minimum in [3usize, 6, 7, 8, 15, 16] {
        let seed = m5_seed_size(minimum as u64).unwrap() as usize;
        let period = vec![0x3c; seed];
        let mut input = period.clone();
        input.push(period[0]);
        input.extend(period.iter().copied().cycle().take(minimum + seed + 2));
        let candidates = production(&input, minimum as u64);
        assert!(
            candidates.iter().any(|candidate| {
                candidate.dst % seed as u64 != 0 && candidate.len >= minimum as u64
            }),
            "minimum={minimum} seed={seed} candidates={candidates:?}"
        );
    }
}

#[test]
fn all_same_key_sources_are_examined_without_a_chain_cap() {
    let input = vec![b'x'; 66];
    let candidates = production(&input, 2);
    assert!(candidates.len() > 12);
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.dst + candidate.len == 66)
    );
}

#[test]
fn one_target_query_returns_exactly_64_sources_in_index_order() {
    let minimum = 7usize;
    let seed = m5_seed_size(minimum as u64).unwrap() as usize;
    let source_bytes = *b"abcd";
    let mut input = source_bytes.repeat(64);
    input.extend_from_slice(b"abcZ");
    let target = input.len();
    input.extend_from_slice(b"abcdabc");
    let candidates = production(&input, minimum as u64);
    let at_target: Vec<_> = candidates
        .iter()
        .filter(|candidate| {
            candidate.dst <= target as u64 && candidate.dst + candidate.len > target as u64
        })
        .filter(|candidate| candidate.dst == target as u64)
        .copied()
        .collect();
    let expected: Vec<_> = (0..64)
        .rev()
        .map(|index| MatchCandidate {
            src: (index * seed) as u64,
            dst: target as u64,
            len: minimum as u64,
            insertion_ordinal: 0,
        })
        .collect();
    assert_eq!(at_target.len(), 64);
    assert_eq!(
        at_target
            .iter()
            .map(|candidate| candidate.src)
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|candidate| candidate.src)
            .collect::<Vec<_>>(),
    );
    assert!(
        at_target
            .windows(2)
            .all(|pair| pair[0].insertion_ordinal < pair[1].insertion_ordinal)
    );
}

#[test]
fn packed_slice_vectors_cover_empty_small_and_large_layouts() {
    assert_eq!(packed_slice_metadata(&[]), 0);
    assert_eq!(packed_slice_metadata(&[1]), 1);
    assert_eq!(
        packed_slice_metadata(&(1u8..=7).collect::<Vec<_>>()),
        0x07654321
    );
    assert_eq!(
        packed_slice_metadata(&(1u8..=8).collect::<Vec<_>>()),
        0x87654321
    );
    let mut changed = vec![1; 256];
    changed[255] = 2;
    assert_ne!(
        packed_slice_metadata(&vec![1; 256]),
        packed_slice_metadata(&changed)
    );
}

#[test]
fn m5_overlay_keeps_base_first_and_uses_shared_effective_normalization() {
    let input: Vec<u8> = (0..40u8)
        .chain(0..40u8)
        .chain(100..120u8)
        .chain(100..120u8)
        .collect();
    let mut config = config(40);
    config.rep_overlay = Some(RepConfig {
        distance: 64,
        min_match: 8,
    });
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let base = {
        let mut base_config = config.clone();
        base_config.rep_overlay = None;
        find_matches_m5(Cursor::new(&input), &base_config, &context)
            .unwrap()
            .as_slice()
            .to_vec()
    };
    let combined = find_matches_m5(Cursor::new(&input), &config, &context).unwrap();
    assert!(combined.iter().any(|candidate| candidate.len < 40));
    let first_overlay = combined
        .iter()
        .position(|candidate| candidate.len < 40)
        .unwrap();
    assert!(
        combined.as_slice()[..first_overlay]
            .iter()
            .all(|candidate| candidate.len >= 40)
    );
    if let Some(first_base) = base.first() {
        assert_eq!(combined.first(), Some(first_base));
    }
    if let (Some(last_base), Some(first_overlay)) = (
        base.last(),
        combined.iter().find(|candidate| candidate.len < 40),
    ) {
        assert!(first_overlay.insertion_ordinal > last_base.insertion_ordinal);
    }
    assert_eq!(config.effective_min_match().unwrap(), 8);
}

fn overlay_input() -> Vec<u8> {
    let short: Vec<u8> = (0..24u8).map(|value| value.wrapping_mul(3)).collect();
    let long: Vec<u8> = (0..56u8).map(|value| value.wrapping_mul(5)).collect();
    let mut input = short.clone();
    input.extend_from_slice(&short);
    input.extend((0..32u8).map(|value| value.wrapping_add(101)));
    input.extend_from_slice(&long);
    input.extend_from_slice(&long);
    input
}

#[test]
fn m5_overlay_relations_have_complete_matrix_and_threshold_specific_matches() {
    let input = overlay_input();
    let mut expected_ir = [None, None, None];
    for (base_minimum, rep_minimum) in [(40u64, 8u64), (16, 16), (8, 40)] {
        let mut combined = None;
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                let mut config = config(base_minimum);
                config.layout = layout;
                config.checksum = checksum;
                config.rep_overlay = Some(RepConfig {
                    distance: 1024,
                    min_match: rep_minimum,
                });
                let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
                let mut archive = Vec::new();
                let stats =
                    compress_with_context(Cursor::new(&input), &mut archive, &config, &context)
                        .unwrap();
                let inspected = srep::inspect_matches(archive.as_slice()).unwrap();
                let triples = inspected.as_slice().to_vec();
                let slot = match base_minimum {
                    40 => 0,
                    16 => 1,
                    _ => 2,
                };
                if let Some(expected) = &expected_ir[slot] {
                    assert_eq!(expected, &triples, "base={base_minimum} rep={rep_minimum}");
                } else {
                    expected_ir[slot] = Some(triples.clone());
                }
                if let Some(expected) = &combined {
                    assert_eq!(expected, &triples);
                } else {
                    combined = Some(triples);
                }
                assert_eq!(
                    stats.semantic_match_count,
                    inspected.as_slice().len() as u64
                );
                assert_eq!(
                    stats.covered_bytes,
                    inspected.iter().map(|item| item.len).sum::<u64>()
                );
                assert!(
                    inspected
                        .iter()
                        .all(|item| item.len >= base_minimum.min(rep_minimum))
                );
                let mut restored = Vec::new();
                srep::decompress(archive.as_slice(), &mut restored).unwrap();
                assert_eq!(restored, input);
            }
        }
        let raw = {
            let mut config = config(base_minimum);
            config.rep_overlay = Some(RepConfig {
                distance: 1024,
                min_match: rep_minimum,
            });
            let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
            find_matches_m5(Cursor::new(&input), &config, &context)
                .unwrap()
                .as_slice()
                .to_vec()
        };
        match (base_minimum, rep_minimum) {
            (40, 8) => assert!(
                raw.iter()
                    .any(|candidate| { candidate.len >= 8 && candidate.len < 40 })
            ),
            (16, 16) => assert!(raw.iter().any(|candidate| candidate.len >= 16)),
            (8, 40) => {
                assert!(
                    raw.iter()
                        .any(|candidate| candidate.len >= 8 && candidate.len < 40)
                );
                assert!(raw.iter().any(|candidate| candidate.len >= 40));
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn m5_overlay_candidate_api_rejects_valid_bytes_below_effective_minimum() {
    let input: Vec<u8> = (0..64u8).cycle().take(128).collect();
    let mut config = config(40);
    config.rep_overlay = Some(RepConfig {
        distance: 128,
        min_match: 32,
    });
    let candidate = MatchCandidate {
        src: 0,
        dst: 64,
        len: 26,
        insertion_ordinal: 0,
    };
    let mut archive = Vec::new();
    let error =
        srep::compress_with_candidates(Cursor::new(&input), &mut archive, &config, [candidate])
            .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidMatch);
    assert!(archive.is_empty());
}

#[test]
fn m5_roundtrip_matrix_has_identical_ir() {
    let input = b"0123456789abcdef0123456789abcdef".repeat(8);
    let mut expected = None;
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let mut config = config(8);
            config.layout = layout;
            config.checksum = checksum;
            let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
            let mut archive = Vec::new();
            let stats = compress_with_context(Cursor::new(&input), &mut archive, &config, &context)
                .unwrap();
            assert!(stats.semantic_match_count > 0);
            let matches = srep::inspect_matches(archive.as_slice())
                .unwrap()
                .as_slice()
                .to_vec();
            if let Some(expected) = &expected {
                assert_eq!(expected, &matches);
            } else {
                expected = Some(matches);
            }
            let mut output = Vec::new();
            srep::decompress(archive.as_slice(), &mut output).unwrap();
            assert_eq!(output, input);
        }
    }
}

#[test]
fn m5_overlay_disabled_matrix_matches_base_only_candidates_and_wire_state() {
    let input: Vec<u8> = (0u8..32).cycle().take(160).collect();
    let base_config = config(8);
    let context = srep::ResourceContext::with_resources(&base_config.resources).unwrap();
    let expected_raw = find_matches_m5(Cursor::new(&input), &base_config, &context)
        .unwrap()
        .as_slice()
        .to_vec();
    let expected_normalized = normalize_matches_with_budget(
        expected_raw.iter().copied(),
        input.len() as u64,
        base_config.min_match,
        &context.memory,
    )
    .unwrap();
    let expected_ir = expected_normalized.as_slice().to_vec();
    let mut expected_stats = None;
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let mut config = base_config.clone();
            config.layout = layout;
            config.checksum = checksum;
            assert!(config.rep_overlay.is_none());
            let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
            let mut archive = Vec::new();
            let stats = compress_with_context(Cursor::new(&input), &mut archive, &config, &context)
                .unwrap();
            let header = srep::format_v3::parse_archive_header(&archive[..80]).unwrap();
            assert_eq!(header.method, Method::M5Exhaustive);
            assert_eq!(header.semantic_flags, 0);
            assert_eq!(header.seed_size, m5_seed_size(config.min_match).unwrap());
            assert_eq!(header.min_match, config.min_match);
            assert_eq!(header.semantic_flags, 0);
            assert_eq!(header.rep_distance, 0);
            assert_eq!(header.rep_min_match, 0);
            let inspected = srep::inspect_matches(archive.as_slice()).unwrap();
            assert_eq!(inspected.as_slice(), expected_ir.as_slice());
            assert_eq!(stats.semantic_match_count, expected_ir.len() as u64);
            assert_eq!(stats.covered_bytes, expected_normalized.covered_bytes);
            assert_eq!(stats.literal_bytes, expected_normalized.literal_bytes);
            if let Some((count, covered, literal)) = expected_stats {
                assert_eq!(
                    (count, covered, literal),
                    (
                        stats.semantic_match_count,
                        stats.covered_bytes,
                        stats.literal_bytes
                    )
                );
            } else {
                expected_stats = Some((
                    stats.semantic_match_count,
                    stats.covered_bytes,
                    stats.literal_bytes,
                ));
            }
            let mut restored = Vec::new();
            srep::decompress(archive.as_slice(), &mut restored).unwrap();
            assert_eq!(restored, input);
        }
    }
}

#[test]
fn m5_empty_and_nonempty_headers_and_structures_are_valid_for_every_layout() {
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let mut empty_config = config(16);
            empty_config.layout = layout;
            empty_config.checksum = checksum;
            let mut empty_archive = Vec::new();
            let empty_stats = srep::compress(&[][..], &mut empty_archive, &empty_config).unwrap();
            assert_eq!(empty_stats.semantic_match_count, 0);
            assert_eq!(empty_stats.covered_bytes, 0);
            assert_eq!(empty_stats.literal_bytes, 0);
            let header = srep::format_v3::parse_archive_header(&empty_archive[..80]).unwrap();
            assert_eq!(header.method, Method::M5Exhaustive);
            assert_eq!(header.seed_size, m5_seed_size(16).unwrap());
            assert_eq!(header.layout, layout);
            assert_eq!(header.checksum, checksum);
            let info = srep::inspect(empty_archive.as_slice()).unwrap();
            assert_eq!(info.block_count, 0);
            let header = srep::format_v3::parse_archive_header(&empty_archive[..80]).unwrap();
            assert_eq!(header.layout, layout);
            assert_eq!(
                empty_archive.len() as u64,
                srep::format_v3::empty_archive_len(layout, checksum)
            );
            assert_eq!(header.seed_size, m5_seed_size(16).unwrap());
            let mut restored = Vec::new();
            srep::decompress(&empty_archive[..], &mut restored).unwrap();
            assert!(restored.is_empty());

            let input = b"0123456701234567".repeat(8);
            let mut archive = Vec::new();
            let stats = srep::compress(&input[..], &mut archive, &empty_config).unwrap();
            assert!(stats.semantic_match_count > 0);
            let mut restored = Vec::new();
            srep::decompress(&archive[..], &mut restored).unwrap();
            assert_eq!(restored, input);
        }
    }
}

#[test]
fn m5_memory_failure_does_not_publish_output() {
    let mut config = config(2);
    config.resources.memory = 1;
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let mut output = Vec::new();
    let error = compress_with_context(Cursor::new(vec![b'x'; 128]), &mut output, &config, &context)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded);
    assert!(output.is_empty());
    assert_eq!(context.memory.current(), 0);
    assert!(context.memory.high_water() <= context.memory.limit());
}

#[test]
fn far_distance_independent_extension_matches_oracle_without_interval_state() {
    let prefix: Vec<u8> = (0..2048).map(|index| (index * 17) as u8).collect();
    let mut input = prefix.clone();
    input.extend((0..4096).map(|index| (index * 31 + 9) as u8));
    input.extend_from_slice(&prefix);
    let minimum = 512usize;
    let produced = production(&input, minimum as u64);
    assert_eq!(produced, m5_oracle(&input, minimum));
    assert!(
        produced.iter().any(|candidate| {
            candidate.src == 0
                && candidate.dst == (input.len() - prefix.len()) as u64
                && candidate.len >= prefix.len() as u64
        }),
        "missing independent far-distance candidate: {produced:?}"
    );
}

#[test]
fn snapshot_resource_fallback_keeps_exact_candidates() {
    let mut input = b"0123456789abcdef".repeat(64);
    let shifted = input[7..521].to_vec();
    input[510..1024].copy_from_slice(&shifted[..514]);
    let expected = m5_oracle(&input, 8);
    let mut fallback_config = config(8);
    fallback_config.resources.memory = 2 * 1024 * 1024;
    let fallback_context =
        srep::ResourceContext::with_resources(&fallback_config.resources).unwrap();
    let held = fallback_context
        .memory
        .reserve(fallback_context.memory.limit() - 512 * 1024)
        .unwrap();
    let fallback = find_matches_m5(Cursor::new(&input), &fallback_config, &fallback_context)
        .unwrap()
        .as_slice()
        .to_vec();
    drop(held);
    assert_eq!(fallback, expected);
    assert!(
        fallback
            .iter()
            .any(|candidate| { candidate.dst - candidate.src < candidate.len })
    );
    assert!(
        fallback
            .iter()
            .any(|candidate| { candidate.src % 8 != 0 || candidate.dst % 8 != 0 })
    );
    assert!(fallback_context.memory.high_water() <= fallback_context.memory.limit());
    assert!(fallback_context.temp.high_water() <= fallback_context.temp.limit());
    assert_eq!(fallback_context.memory.current(), 0);
    assert_eq!(fallback_context.temp.current(), 0);

    let normalized = normalize_matches_with_budget(
        expected.iter().copied(),
        input.len() as u64,
        fallback_config.min_match,
        &fallback_context.memory,
    )
    .unwrap();
    let mut fallback_archive = Vec::new();
    let fallback_stats = compress_with_context(
        Cursor::new(&input),
        &mut fallback_archive,
        &fallback_config,
        &fallback_context,
    )
    .unwrap();
    let fallback_ir = srep::inspect_matches(Cursor::new(&fallback_archive)).unwrap();
    assert_eq!(fallback_ir.as_slice(), normalized.as_slice());
    assert_eq!(fallback_stats.semantic_match_count, normalized.len() as u64);
    let mut restored = Vec::new();
    srep::decompress(Cursor::new(&fallback_archive), &mut restored).unwrap();
    assert_eq!(restored, input);

    let accelerated_config = config(8);
    let accelerated_context =
        srep::ResourceContext::with_resources(&accelerated_config.resources).unwrap();
    let accelerated = find_matches_m5(
        Cursor::new(&input),
        &accelerated_config,
        &accelerated_context,
    )
    .unwrap()
    .as_slice()
    .to_vec();
    assert_eq!(fallback, accelerated);
    assert!(accelerated_context.memory.high_water() <= accelerated_context.memory.limit());
    assert!(accelerated_context.temp.high_water() <= accelerated_context.temp.limit());
    assert_eq!(accelerated_context.memory.current(), 0);
    assert_eq!(accelerated_context.temp.current(), 0);

    let mut accelerated_archive = Vec::new();
    compress_with_context(
        Cursor::new(&input),
        &mut accelerated_archive,
        &accelerated_config,
        &accelerated_context,
    )
    .unwrap();
    assert_eq!(
        srep::inspect_matches(Cursor::new(&accelerated_archive))
            .unwrap()
            .as_slice(),
        normalized.as_slice()
    );
}

#[test]
fn normalization_accepts_m5_output_at_effective_minimum() {
    let config = config(40);
    let candidate = MatchCandidate {
        src: 0,
        dst: 26,
        len: 26,
        insertion_ordinal: 0,
    };
    let budget = srep::MemoryBudget::new(1024 * 1024);
    let result = normalize_matches_with_budget([candidate], 60, 26, &budget).unwrap();
    assert_eq!(result.covered_bytes, 26);
    assert_eq!(config.effective_min_match().unwrap(), 40);
}

#[test]
fn m5_oracle_preserves_raw_counter_and_known_witnesses() {
    let zeros = [0u8; 64];
    let (dedup, raw_count) = m5_oracle_with_distance(&zeros, 32, None);
    assert_eq!(raw_count, 80);
    assert!(
        dedup
            .iter()
            .any(|candidate| candidate.src == 0 && candidate.dst == 2 && candidate.len == 62)
    );
    let max_retained = dedup
        .iter()
        .map(|candidate| candidate.insertion_ordinal)
        .max()
        .unwrap();
    assert_eq!(max_retained + 1, 48);
    assert!(raw_count > max_retained + 1);
    assert_eq!(production(&zeros, 32), dedup);

    let mut nonaligned = vec![1u8; 8];
    nonaligned.push(1);
    let (min8, min8_raw) = m5_oracle_with_distance(&nonaligned, 8, None);
    assert!(min8_raw > 0);
    assert!(
        min8.iter()
            .any(|candidate| candidate.src == 0 && candidate.dst == 1 && candidate.len == 8),
        "target=1 seed=4 must include aligned source 0: {min8:?}"
    );
    assert_eq!(production(&nonaligned, 8), min8);
    assert!(m5_oracle(b"", 8).is_empty());
    assert!(m5_oracle(b"abc", 8).is_empty());
    assert!(production(b"", 8).is_empty());
}

#[test]
fn m5_codec_and_public_finder_match_oracle_ir_and_archive() {
    let input = [0u8; 64];
    let (expected, raw_next) = m5_oracle_with_distance(&input, 32, None);
    assert_eq!(raw_next, 80);
    assert!(
        expected
            .iter()
            .any(|candidate| candidate.src == 0 && candidate.dst == 2 && candidate.len == 62)
    );
    let public = production(&input, 32);
    assert_eq!(public, expected);
    let mut combinations = 0usize;
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let mut cfg = config(32);
            cfg.layout = layout;
            cfg.checksum = checksum;
            let context = srep::ResourceContext::with_resources(&cfg.resources).unwrap();
            let mut ordinary = Vec::new();
            let ordinary_stats =
                compress_with_context(Cursor::new(&input), &mut ordinary, &cfg, &context).unwrap();
            let mut from_oracle = Vec::new();
            let oracle_stats = compress_with_candidates_with_context(
                Cursor::new(&input),
                &mut from_oracle,
                &cfg,
                expected.iter().copied(),
                &context,
            )
            .unwrap();
            assert_eq!(
                ordinary, from_oracle,
                "layout={layout:?} checksum={checksum:?}"
            );
            assert_eq!(ordinary_stats, oracle_stats);
            let inspected = srep::inspect_matches(ordinary.as_slice()).unwrap();
            let normalized = normalize_matches_with_budget(
                expected.iter().copied(),
                input.len() as u64,
                cfg.min_match,
                &context.memory,
            )
            .unwrap();
            assert_eq!(inspected.as_slice(), normalized.as_slice());
            assert_eq!(ordinary_stats.semantic_match_count, normalized.len() as u64);
            assert_eq!(ordinary_stats.covered_bytes, normalized.covered_bytes);
            assert!(
                expected.iter().any(|candidate| candidate.src == 0
                    && candidate.dst == 2
                    && candidate.len == 62)
            );

            let mut omitted = expected.clone();
            omitted.retain(|candidate| {
                !inspected.iter().any(|item| {
                    item.src == candidate.src
                        && item.dst == candidate.dst
                        && item.len == candidate.len
                })
            });
            assert!(
                omitted.len() < expected.len(),
                "IR-selected matches must be present in the oracle set"
            );
            let mut omitted_archive = Vec::new();
            let omitted_stats = compress_with_candidates_with_context(
                Cursor::new(&input),
                &mut omitted_archive,
                &cfg,
                omitted,
                &context,
            )
            .unwrap();
            assert_ne!(
                omitted_archive, ordinary,
                "dropping IR-selected candidates must change archive layout={layout:?} checksum={checksum:?}"
            );
            assert_ne!(
                (
                    omitted_stats.semantic_match_count,
                    omitted_stats.covered_bytes
                ),
                (
                    ordinary_stats.semantic_match_count,
                    ordinary_stats.covered_bytes
                )
            );
            let mut omitted_public = public.clone();
            omitted_public.retain(|candidate| {
                !(candidate.src == 0 && candidate.dst == 2 && candidate.len == 62)
            });
            assert_ne!(omitted_public, public);

            let mut tampered = ordinary.clone();
            let mutate_at = tampered.len() / 2;
            tampered[mutate_at] ^= 0xa5;
            assert_ne!(tampered.as_slice(), ordinary.as_slice());
            let inspect_tampered = srep::inspect_matches(tampered.as_slice());
            let mut restored_tampered = Vec::new();
            let decompress_tampered = srep::decompress(tampered.as_slice(), &mut restored_tampered);
            assert!(
                inspect_tampered.is_err()
                    || inspect_tampered.unwrap().as_slice() != inspected.as_slice()
                    || decompress_tampered.is_err()
                    || restored_tampered != input,
                "isolated archive copy mutation must be detectable layout={layout:?} checksum={checksum:?}"
            );

            let mut restored = Vec::new();
            srep::decompress(ordinary.as_slice(), &mut restored).unwrap();
            assert_eq!(restored, input);
            combinations += 1;
            drop(normalized);
            drop(inspected);
            drop(ordinary);
            drop(from_oracle);
            drop(omitted_archive);
            assert_eq!(context.memory.current(), 0);
            assert_eq!(context.temp.current(), 0);
        }
    }
    assert_eq!(
        combinations, 6,
        "all 3 layouts × 2 checksums must run independently"
    );
}

#[test]
fn m5_overlay_min_and_distance_matrix_matches_oracle() {
    let input = overlay_input();
    let distance = 48usize;
    let planted = unique_seed_distance_input(distance, 32);
    let inclusive = production_with_distance(&planted, 32, Some(distance as u64));
    let exclusive = production_with_distance(&planted, 32, Some(distance as u64 - 1));
    let (oracle_inclusive, _) = m5_oracle_with_distance(&planted, 32, Some(distance as u64));
    let (oracle_exclusive, _) = m5_oracle_with_distance(&planted, 32, Some(distance as u64 - 1));
    let unique_inclusive: Vec<_> = oracle_inclusive
        .iter()
        .copied()
        .filter(|candidate| candidate.dst - candidate.src == distance as u64)
        .collect();
    assert_eq!(
        unique_inclusive.len(),
        1,
        "pre-extension seed distance {distance} must have one unique retained witness: {unique_inclusive:?}"
    );
    assert_eq!(inclusive, oracle_inclusive);
    assert_eq!(exclusive, oracle_exclusive);
    assert!(inclusive.iter().any(|candidate| {
        candidate.src == unique_inclusive[0].src
            && candidate.dst == unique_inclusive[0].dst
            && candidate.len == unique_inclusive[0].len
    }));
    assert!(!exclusive.iter().any(|candidate| {
        candidate.src == unique_inclusive[0].src
            && candidate.dst == unique_inclusive[0].dst
            && candidate.len == unique_inclusive[0].len
    }));

    for (base_minimum, overlay_minimum) in [(40u64, 8u64), (16, 16), (8, 40)] {
        for (max_distance, overlay_distance) in [
            (None, 1024u64),
            (Some(0), 1024),
            (Some(64), 1024),
            (Some(1024), 32),
            (Some(32), 32),
        ] {
            let mut cfg = config(base_minimum);
            cfg.max_distance = max_distance;
            cfg.rep_overlay = Some(RepConfig {
                distance: overlay_distance,
                min_match: overlay_minimum,
            });
            let context = srep::ResourceContext::with_resources(&cfg.resources).unwrap();
            let public = find_matches_m5(Cursor::new(&input), &cfg, &context)
                .unwrap()
                .as_slice()
                .to_vec();
            let (combined_oracle, base, raw_next) = combined_overlay_oracle(
                &input,
                base_minimum,
                overlay_minimum,
                max_distance,
                overlay_distance,
            );
            let public_base: Vec<_> = public
                .iter()
                .copied()
                .filter(|candidate| candidate.insertion_ordinal < raw_next)
                .collect();
            assert_eq!(
                public_base, base,
                "base candidates must equal the full independent oracle, not a subset; base_min={base_minimum} overlay_min={overlay_minimum} max={max_distance:?}"
            );
            assert_eq!(public, combined_oracle);
            let overlay_only: Vec<_> = public
                .iter()
                .copied()
                .filter(|candidate| {
                    !base.iter().any(|expected| {
                        (expected.src, expected.dst, expected.len)
                            == (candidate.src, candidate.dst, candidate.len)
                    })
                })
                .collect();
            if base_minimum == 40 && overlay_minimum == 8 {
                assert!(
                    !overlay_only.is_empty(),
                    "designated overlay-only witness required for base=40 overlay=8 max={max_distance:?} overlay_distance={overlay_distance}: {public:?}"
                );
                assert!(
                    overlay_only
                        .iter()
                        .all(|candidate| candidate.insertion_ordinal >= raw_next)
                );
            }
            for layout in [Layout::Index, Layout::Future, Layout::Io] {
                for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                    let mut archive_config = cfg.clone();
                    archive_config.layout = layout;
                    archive_config.checksum = checksum;
                    let mut archive = Vec::new();
                    let stats = compress_with_context(
                        Cursor::new(&input),
                        &mut archive,
                        &archive_config,
                        &context,
                    )
                    .unwrap();
                    let mut from_oracle = Vec::new();
                    let oracle_stats = compress_with_candidates_with_context(
                        Cursor::new(&input),
                        &mut from_oracle,
                        &archive_config,
                        combined_oracle.iter().copied(),
                        &context,
                    )
                    .unwrap();
                    assert_eq!(archive, from_oracle);
                    assert_eq!(stats, oracle_stats);
                    let inspected = srep::inspect_matches(archive.as_slice()).unwrap();
                    let normalized = normalize_matches_with_budget(
                        combined_oracle.iter().copied(),
                        input.len() as u64,
                        archive_config.effective_min_match().unwrap(),
                        &context.memory,
                    )
                    .unwrap();
                    assert_eq!(inspected.as_slice(), normalized.as_slice());
                    assert_eq!(stats.semantic_match_count, normalized.len() as u64);
                    let mut restored = Vec::new();
                    srep::decompress(archive.as_slice(), &mut restored).unwrap();
                    assert_eq!(restored, input);
                    drop(normalized);
                    drop(inspected);
                }
            }
            drop(context);
        }
    }
}

fn production_with_distance(
    input: &[u8],
    minimum: u64,
    max_distance: Option<u64>,
) -> Vec<MatchCandidate> {
    let mut cfg = config(minimum);
    cfg.max_distance = max_distance;
    let context = srep::ResourceContext::with_resources(&cfg.resources).unwrap();
    find_matches_m5(Cursor::new(input), &cfg, &context)
        .unwrap()
        .as_slice()
        .to_vec()
}

#[test]
fn m5_overlay_on_zero_block_starts_at_full_next_eighty_not_max_retained_forty_eight() {
    let input = [0u8; 64];
    let (base, raw_next) = m5_oracle_with_distance(&input, 32, None);
    assert_eq!(raw_next, 80);
    let max_retained = base
        .iter()
        .map(|candidate| candidate.insertion_ordinal)
        .max()
        .unwrap();
    assert_eq!(max_retained + 1, 48);
    let mut cfg = config(32);
    cfg.rep_overlay = Some(RepConfig {
        distance: 64,
        min_match: 16,
    });
    let context = srep::ResourceContext::with_resources(&cfg.resources).unwrap();
    let combined = find_matches_m5(Cursor::new(&input), &cfg, &context).unwrap();
    let overlay_only: Vec<_> = combined
        .iter()
        .copied()
        .filter(|candidate| {
            !base.iter().any(|expected| {
                (expected.src, expected.dst, expected.len)
                    == (candidate.src, candidate.dst, candidate.len)
            })
        })
        .collect();
    assert!(
        overlay_only
            .iter()
            .all(|candidate| candidate.insertion_ordinal >= raw_next),
        "surviving overlay-only may start later than raw next {raw_next} after exact-triple dedup, but never below it or at max retained {}: {overlay_only:?}",
        max_retained + 1
    );
    assert!(
        overlay_only
            .iter()
            .any(|candidate| candidate.insertion_ordinal > max_retained + 1),
        "pipeline truth is full next {raw_next}, not max retained {}",
        max_retained + 1
    );
    let mut none = cfg.clone();
    none.rep_overlay = None;
    let without = find_matches_m5(Cursor::new(&input), &none, &context)
        .unwrap()
        .as_slice()
        .to_vec();
    assert_eq!(without, base);

    let nonuniform = overlay_input();
    let (nonuniform_base, nonuniform_next) = m5_oracle_with_distance(&nonuniform, 40, None);
    let mut nonuniform_config = config(40);
    nonuniform_config.rep_overlay = Some(RepConfig {
        distance: 1024,
        min_match: 8,
    });
    let nonuniform_combined =
        find_matches_m5(Cursor::new(&nonuniform), &nonuniform_config, &context).unwrap();
    let first_overlay = nonuniform_combined.iter().find(|candidate| {
        !nonuniform_base.iter().any(|expected| {
            (expected.src, expected.dst, expected.len)
                == (candidate.src, candidate.dst, candidate.len)
        })
    });
    assert_eq!(
        first_overlay.map(|candidate| candidate.insertion_ordinal),
        Some(nonuniform_next),
        "non-uniform overlay survivor must keep the raw full next ordinal {nonuniform_next}: {first_overlay:?}"
    );
}
