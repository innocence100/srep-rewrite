use std::io::Cursor;

use srep::{
    Checksum, CompressionConfig, Layout, MatchCandidate, Method, ResourceConfig,
    compress_with_context, find_matches_m1, find_matches_m2, inspect_matches,
};

const BASE: u64 = 153_191;

fn config(method: Method, layout: Layout, checksum: Checksum) -> CompressionConfig {
    let mut config = CompressionConfig::for_method(method);
    config.layout = layout;
    config.checksum = checksum;
    config.block_size = 1024;
    config.min_match = 32;
    config.target_chunk = Some(32);
    config.resources = ResourceConfig {
        memory: 64 * 1024 * 1024,
        temp_limit: 256 * 1024 * 1024,
        ..ResourceConfig::default()
    };
    config
}

fn polynomial(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |hash, &byte| {
        hash.wrapping_mul(BASE).wrapping_add(u64::from(byte))
    })
}

fn m1_oracle(
    input: &[u8],
    block_size: usize,
    min_match: usize,
    target: u64,
) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    for block_start in (0..input.len()).step_by(block_size) {
        let block_end = (block_start + block_size).min(input.len());
        if block_end - block_start <= 48 {
            result.push((block_start, block_end));
            continue;
        }
        let mut hash = polynomial(&input[block_start..block_start + 48]);
        let power = (0..47).fold(1u64, |value, _| value.wrapping_mul(BASE));
        let threshold = u64::MAX - u64::MAX / target;
        let mut last = block_start;
        for p in block_start + 48..block_end {
            hash = hash
                .wrapping_sub(u64::from(input[p - 48]).wrapping_mul(power))
                .wrapping_mul(BASE)
                .wrapping_add(u64::from(input[p]));
            if hash > threshold && p - last >= min_match {
                result.push((last, p));
                last = p;
            }
        }
        if last < block_end {
            result.push((last, block_end));
        }
    }
    result
}

fn m2_oracle(
    input: &[u8],
    block_size: usize,
    min_match: usize,
    target: u64,
) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    for block_start in (0..input.len()).step_by(block_size) {
        let block_end = (block_start + block_size).min(input.len());
        let mut predict = [0u8; 256];
        let mut previous = 0u8;
        let mut hash = 0u32;
        let threshold = u32::MAX - u32::MAX / target as u32;
        let mut last = block_start;
        for (p, &c) in input.iter().enumerate().take(block_end).skip(block_start) {
            let multiplier = if c != predict[previous as usize] {
                271_828_182
            } else {
                314_159_265
            };
            hash = hash.wrapping_add(u32::from(c) + 1).wrapping_mul(multiplier);
            predict[previous as usize] = c;
            previous = c;
            if hash > threshold && p - last >= min_match {
                result.push((last, p));
                last = p;
                predict = [0; 256];
                previous = 0;
                hash = 0;
            }
        }
        if last < block_end {
            result.push((last, block_end));
        }
    }
    result
}

fn digest(bytes: &[u8]) -> [u8; 16] {
    let hash = blake3::hash(bytes);
    hash.as_bytes()[..16].try_into().unwrap()
}

fn oracle_candidates(
    input: &[u8],
    chunks: &[(usize, usize)],
    min_match: usize,
    max_distance: Option<u64>,
) -> Vec<MatchCandidate> {
    let mut history = Vec::<(usize, usize, [u8; 16])>::new();
    let mut result = Vec::new();
    for &(start, end) in chunks {
        let bytes = &input[start..end];
        let length = end - start;
        let mut matches = history
            .iter()
            .filter(|&&(source, source_end, key)| {
                source_end - source == length
                    && source < start
                    && max_distance.is_none_or(|distance| (start - source) as u64 <= distance)
                    && key == digest(bytes)
                    && input[source..source_end] == *bytes
            })
            .map(|&(source, _, _)| source)
            .collect::<Vec<_>>();
        matches.sort_unstable_by(|a, b| b.cmp(a));
        for source in matches {
            if length >= min_match {
                let ordinal = result.len() as u64;
                result.push(MatchCandidate {
                    src: source as u64,
                    dst: start as u64,
                    len: length as u64,
                    insertion_ordinal: ordinal,
                });
            }
        }
        history.push((start, end, digest(bytes)));
    }
    result
}

fn patterned_input(blocks: usize) -> Vec<u8> {
    let block: Vec<u8> = (0..1024)
        .map(|index| ((index * 17 + index / 11) & 0xff) as u8)
        .collect();
    block.repeat(blocks)
}

fn production(input: &[u8], method: Method, max_distance: Option<u64>) -> Vec<MatchCandidate> {
    let mut config = config(method, Layout::Index, Checksum::Xxh3);
    config.max_distance = max_distance;
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    match method {
        Method::M1RollingCdc => find_matches_m1(Cursor::new(input), &config, &context),
        Method::M2Order1Cdc => find_matches_m2(Cursor::new(input), &config, &context),
        _ => unreachable!(),
    }
    .unwrap()
    .as_slice()
    .to_vec()
}

#[test]
fn independent_boundary_oracles_cover_edge_lengths_and_threshold_hits() {
    let mut state = 0x1234_5678_9abc_def0u64;
    for length in [0usize, 1, 47, 48, 49, 80, 1024, 2048] {
        let mut input = vec![0u8; length];
        for byte in &mut input {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            *byte = (state >> 56) as u8;
        }
        let expected_m1 = m1_oracle(&input, 1024, 32, 32);
        let expected_m2 = m2_oracle(&input, 1024, 32, 32);
        assert!(expected_m1.windows(2).all(|pair| pair[0].1 <= pair[1].0));
        assert!(expected_m2.windows(2).all(|pair| pair[0].1 <= pair[1].0));
        assert!(expected_m1.iter().all(|&(start, end)| start < end));
        assert!(expected_m2.iter().all(|&(start, end)| start < end));
    }
    let input = patterned_input(8);
    let expected_m1 = m1_oracle(&input, 1024, 32, 32);
    let expected_m2 = m2_oracle(&input, 1024, 32, 32);
    assert!(expected_m1.len() > 8, "m1 oracle did not hit threshold");
    assert!(expected_m2.len() > 8, "m2 oracle did not hit threshold");
    assert_ne!(expected_m1, expected_m2, "m1 and m2 must remain distinct");
}

#[test]
fn independent_candidate_oracle_matches_m1_and_m2_order_and_distance() {
    let input = patterned_input(8);
    for (method, chunks) in [
        (Method::M1RollingCdc, m1_oracle(&input, 1024, 32, 32)),
        (Method::M2Order1Cdc, m2_oracle(&input, 1024, 32, 32)),
    ] {
        let expected = oracle_candidates(&input, &chunks, 32, None);
        assert_eq!(production(&input, method, None), expected, "{method:?}");
        assert!(!expected.is_empty());
        let boundary_distance = expected[0].dst - expected[0].src;
        let mut expected_at_distance = expected
            .iter()
            .filter(|candidate| candidate.dst - candidate.src <= boundary_distance)
            .copied()
            .collect::<Vec<_>>();
        for (ordinal, candidate) in expected_at_distance.iter_mut().enumerate() {
            candidate.insertion_ordinal = ordinal as u64;
        }
        assert_eq!(
            production(&input, method, Some(boundary_distance)),
            expected_at_distance
        );
        assert!(production(&input, method, Some(boundary_distance - 1)).len() < expected.len());
    }
}

#[test]
fn ordinary_m1_m2_round_trip_with_identical_semantic_ir_across_layouts_and_checksums() {
    let input = patterned_input(8);
    for method in [Method::M1RollingCdc, Method::M2Order1Cdc] {
        let mut expected_ir = None;
        let mut expected_stats = None;
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                let config = config(method, layout, checksum);
                let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
                let mut archive = Vec::new();
                let stats =
                    compress_with_context(Cursor::new(&input), &mut archive, &config, &context)
                        .unwrap();
                assert!(stats.semantic_match_count > 0);
                let ir = inspect_matches(archive.as_slice()).unwrap_or_else(|error| {
                    eprintln!("failed {method:?} {layout:?} {checksum:?}: {error}");
                    eprintln!("stats: {stats:?}");
                    panic!("inspect failed")
                });
                let signature = ir.as_slice().to_vec();
                if let Some(expected) = &expected_ir {
                    assert_eq!(expected, &signature, "{method:?} {layout:?} {checksum:?}");
                } else {
                    expected_ir = Some(signature);
                }
                let semantic = (
                    stats.semantic_match_count,
                    stats.covered_bytes,
                    stats.literal_bytes,
                );
                if let Some(expected) = expected_stats {
                    assert_eq!(expected, semantic);
                } else {
                    expected_stats = Some(semantic);
                }
                let mut output = Vec::new();
                srep::decompress(archive.as_slice(), &mut output).unwrap();
                assert_eq!(output, input);
            }
        }
    }
}

#[test]
fn cdc_memory_exhaustion_is_explicit_and_does_not_publish_output() {
    let input = patterned_input(8);
    for method in [Method::M1RollingCdc, Method::M2Order1Cdc] {
        let mut config = config(method, Layout::Index, Checksum::Xxh3);
        config.resources.memory = 1;
        let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
        let mut archive = Vec::new();
        let error = compress_with_context(Cursor::new(&input), &mut archive, &config, &context)
            .unwrap_err();
        assert_eq!(error.kind(), srep::ErrorKind::MemoryBudgetExceeded);
        assert!(archive.is_empty());
        assert_eq!(context.memory.current(), 0);
        assert!(context.memory.high_water() <= context.memory.limit());
    }
}
