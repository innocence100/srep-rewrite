use std::io::Cursor;

use srep::{CompressionConfig, MatchCandidate, ResourceConfig, find_matches_m0};

const BASE: u64 = 153_191;

fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |value, &byte| {
        value.wrapping_mul(BASE).wrapping_add(byte as u64)
    })
}

fn oracle(input: &[u8], min_match: usize, max_distance: Option<u64>) -> Vec<MatchCandidate> {
    let region = (min_match / 8).max(1);
    let eligible = if input.len() < region {
        0
    } else {
        input.len() - region + 1
    };
    let mut representatives = Vec::new();
    for region_start in (0..eligible).step_by(region) {
        let region_end = (region_start + region).min(eligible);
        let position = (region_start..region_end)
            .max_by_key(|&position| {
                (
                    hash(&input[position..position + region]),
                    usize::MAX - position,
                )
            })
            .unwrap();
        representatives.push((position, hash(&input[position..position + region])));
    }

    let mut output = Vec::new();
    let mut ordinal = 0u64;
    for target in 0..eligible {
        for &(source, key) in representatives.iter().rev().filter(|&&(source, _)| {
            source < target && (source / region * region + region).min(eligible) <= target
        }) {
            if key != hash(&input[target..target + region])
                || input[source..source + region] != input[target..target + region]
            {
                continue;
            }
            let distance = target - source;
            let mut backward = 0usize;
            while backward < source
                && backward < target
                && input[source - backward - 1] == input[target - backward - 1]
            {
                backward += 1;
            }
            let src = source - backward;
            let dst = target - backward;
            let mut length = backward + region;
            while dst + length < input.len()
                && input[dst + length] == input[src + length % distance]
            {
                length += 1;
            }
            let distance_u64 = distance as u64;
            if max_distance.is_none_or(|limit| distance_u64 <= limit) && length >= min_match {
                output.push(MatchCandidate {
                    src: src as u64,
                    dst: dst as u64,
                    len: length as u64,
                    insertion_ordinal: ordinal,
                });
                ordinal += 1;
            }
        }
    }
    output
}

fn production(input: &[u8], min_match: u64, max_distance: Option<u64>) -> Vec<MatchCandidate> {
    let mut config = CompressionConfig::for_method(srep::Method::M0Rep);
    config.min_match = min_match;
    config.max_distance = max_distance;
    config.resources = ResourceConfig {
        memory: 64 * 1024 * 1024,
        ..ResourceConfig::default()
    };
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    find_matches_m0(Cursor::new(input), &config, &context)
        .unwrap()
        .as_slice()
        .to_vec()
}

fn assert_oracle(input: &[u8], min_match: u64, max_distance: Option<u64>) {
    assert_eq!(
        production(input, min_match, max_distance),
        oracle(input, min_match as usize, max_distance),
        "len={}, min_match={min_match}, max_distance={max_distance:?}",
        input.len()
    );
}

#[test]
fn oracle_matches_tied_and_final_partial_regions() {
    assert_oracle(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 16, None);
    assert_oracle(b"abcdefghijklmnopabcdefghijklmnop", 16, None);
    assert_oracle(b"012345678901234567890123456789012345", 16, None);
}

#[test]
fn oracle_golden_sequence_has_contiguous_target_ordinals() {
    let actual = production(b"0123456701234567", 8, None);
    let expected = vec![
        (0, 8, 8, 0),
        (0, 8, 8, 1),
        (0, 8, 8, 2),
        (0, 8, 8, 3),
        (0, 8, 8, 4),
        (0, 8, 8, 5),
        (0, 8, 8, 6),
        (0, 8, 8, 7),
    ];
    assert_eq!(
        actual
            .iter()
            .map(|item| (item.src, item.dst, item.len, item.insertion_ordinal))
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn oracle_matches_multiple_representatives_and_overlap() {
    assert_oracle(b"abcdEFGHabcdIJKLabcdEFGHabcdIJKL", 8, None);
    assert_oracle(b"abcabcabcabcabcabc", 8, None);
}

#[test]
fn oracle_matches_inclusive_and_excluded_distance_boundaries() {
    let input = b"abcdefghabcdefghabcdefgh";
    assert_oracle(input, 8, Some(8));
    assert_oracle(input, 8, Some(7));
}

#[test]
fn oracle_matches_no_candidate_and_fixed_random_cases() {
    assert_oracle(b"no repeated windows here", 8, None);
    let mut state = 0x1234_5678_9abc_def0u64;
    for length in 0..48usize {
        let mut input = vec![0u8; length];
        for byte in &mut input {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            *byte = (state >> 56) as u8;
        }
        assert_oracle(&input, 2 + (length as u64 % 24), Some(16));
    }
}
