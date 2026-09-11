use std::io::Cursor;

use srep::{
    Checksum, CompressionConfig, ErrorKind, Layout, MatchCandidate, Method, RepConfig,
    ResourceConfig, compress_with_context, find_matches_m5, m5_seed_size,
    normalize_matches_with_budget, packed_slice_metadata,
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
    let seed = m5_seed_size(minimum as u64).unwrap() as usize;
    let mut result = Vec::new();
    for target in 0..=input.len().saturating_sub(seed) {
        let source_starts: Vec<_> = (0..=input.len().saturating_sub(seed))
            .step_by(seed)
            .filter(|&source| source < target)
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
    result
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
            let header = srep::format::parse_archive_header(&archive[..80]).unwrap();
            assert_eq!(header.method, Method::M5Exhaustive);
            assert_eq!(header.semantic_flags, 0);
            assert_eq!(header.seed_size, m5_seed_size(config.min_match).unwrap());
            assert_eq!(header.min_match, config.min_match);
            let params_offset = 80 + 12;
            let params = srep::format::parse_method_parameters(
                &archive[params_offset..params_offset + 64],
                &header,
            )
            .unwrap();
            assert_eq!(params.flags, 0);
            assert_eq!(params.rep_distance, 0);
            assert_eq!(params.rep_min_match, 0);
            assert_eq!(params.rep_region_size, 0);
            assert_eq!(
                params.effective_min_match(config.min_match),
                config.min_match
            );
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
            let header = srep::format::parse_archive_header(&empty_archive[..80]).unwrap();
            assert_eq!(header.method, Method::M5Exhaustive);
            assert_eq!(header.seed_size, m5_seed_size(16).unwrap());
            assert_eq!(header.layout, layout);
            assert_eq!(header.checksum, checksum);
            let info = srep::inspect(empty_archive.as_slice()).unwrap();
            assert_eq!(info.block_count, 0);
            let header = srep::format::parse_archive_header(&empty_archive[..80]).unwrap();
            let mut offset = 80usize;
            let mut records = Vec::new();
            while offset + 12 + checksum.width() <= empty_archive.len() - 64 {
                let kind = empty_archive[offset];
                let payload_len =
                    u64::from_le_bytes(empty_archive[offset + 4..offset + 12].try_into().unwrap())
                        as usize;
                records.push(kind);
                offset += 12 + payload_len + checksum.width();
            }
            assert_eq!(
                records,
                if layout == Layout::Index {
                    vec![1, 5, 2, 4, 6]
                } else {
                    vec![1, 5, 6]
                }
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
    let input = b"0123456789abcdef".repeat(64);
    let expected = m5_oracle(&input, 8);
    let mut config = config(8);
    config.resources.memory = 2 * 1024 * 1024;
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let produced = find_matches_m5(Cursor::new(&input), &config, &context)
        .unwrap()
        .as_slice()
        .to_vec();
    assert_eq!(produced, expected);
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
