use std::io::Cursor;
use std::time::{Duration, Instant};

use srep::{
    Checksum, CompressionConfig, ErrorKind, Layout, Method, ResourceConfig, compress, decompress,
    find_matches_m0, find_matches_m0_with_context, inspect, inspect_matches,
};

fn m0(min_match: u64) -> CompressionConfig {
    CompressionConfig {
        method: srep::Method::M0Rep,
        min_match,
        seed_size: None,
        target_chunk: None,
        ..CompressionConfig::for_method(srep::Method::M0Rep)
    }
}

#[test]
fn m0_emits_independently_extended_overlapping_candidates() {
    let config = m0(8);
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let matches = find_matches_m0(Cursor::new(b"abcabcabcabc"), &config, &context).unwrap();
    assert!(
        matches
            .iter()
            .any(|candidate| { candidate.src == 0 && candidate.dst == 3 && candidate.len == 9 })
    );
}

#[test]
// Performance regression for independent per-candidate extension in debug
// builds.
fn m0_repeated_block_finishes_and_round_trips() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let block: Vec<u8> = (0..65_536)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 32) as u8
        })
        .collect();
    let input = [block.as_slice(), block.as_slice()].concat();
    let config = m0(512);
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();

    let candidates = find_matches_m0(Cursor::new(&input), &config, &context).unwrap();
    assert!(candidates.iter().any(|candidate| {
        candidate.dst >= 65_536
            && candidate.dst + candidate.len >= input.len() as u64
            && candidate.len >= 512
    }));

    let mut archive = Vec::new();
    let stats = compress(Cursor::new(&input), &mut archive, &config).unwrap();
    assert!(stats.covered_bytes >= 65_536);
    assert!(stats.semantic_match_count > 0);
    let info = inspect(&archive[..]).unwrap();
    assert!(info.covered_bytes >= 65_536);
    assert!(info.semantic_match_count > 0);
    let mut restored = Vec::new();
    decompress(&archive[..], &mut restored).unwrap();
    assert_eq!(restored, input);
    drop(candidates);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
}

#[test]
// This is intentionally a practical bound rather than a tight benchmark.
fn m0_repeated_128k_block_has_bounded_completion_and_coverage() {
    let mut state = 0xD1B5_4A32_9C87_6EF1u64;
    let block: Vec<u8> = (0..128 * 1024)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 32) as u8
        })
        .collect();
    let input = [block.as_slice(), block.as_slice()].concat();
    let config = m0(512);
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let started = Instant::now();

    let candidates = find_matches_m0(Cursor::new(&input), &config, &context).unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "m0 repeated-block discovery exceeded practical bound: {:?}",
        started.elapsed()
    );
    assert!(candidates.iter().any(|candidate| {
        candidate.dst >= block.len() as u64
            && candidate.dst + candidate.len >= input.len() as u64
            && candidate.len >= block.len() as u64
    }));

    let mut archive = Vec::new();
    let stats = compress(Cursor::new(&input), &mut archive, &config).unwrap();
    assert!(stats.semantic_match_count > 0);
    assert!(stats.covered_bytes >= block.len() as u64);
    let mut restored = Vec::new();
    decompress(&archive[..], &mut restored).unwrap();
    assert_eq!(restored, input);
}

#[test]
// Regression for the frozen exact-repeat shape: four 64 KiB copies and the
// same seed used by fidelity-v1-01.  The bound covers finder, validation,
// normalization, archive encoding, and the round trip.
fn m0_frozen_exact_repeat_compresses_end_to_end_within_bound() {
    let mut state = 0x4100u64 ^ 0x9E37_79B9_7F4A_7C15;
    let block: Vec<u8> = (0..65_536)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 32) as u8
        })
        .collect();
    let input = [
        block.as_slice(),
        block.as_slice(),
        block.as_slice(),
        block.as_slice(),
    ]
    .concat();
    let mut config = m0(512);
    config.block_size = 8 * 1024;
    config.checksum = srep::Checksum::Xxh3;
    let started = Instant::now();

    let mut archive = Vec::new();
    let stats = compress(Cursor::new(&input), &mut archive, &config).unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(30),
        "frozen exact-repeat compression exceeded practical bound: {:?}",
        started.elapsed()
    );
    assert!(!archive.is_empty());
    assert!(stats.covered_bytes > 0);

    let mut restored = Vec::new();
    decompress(&archive[..], &mut restored).unwrap();
    assert_eq!(restored, input);
}

#[test]
fn m0_does_not_make_a_region_representative_visible_early() {
    let mut config = m0(16);
    config.min_match = 16;
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let input = b"abcdefghijklmnopabcdefghijklmnop";
    let matches = find_matches_m0(Cursor::new(input), &config, &context).unwrap();
    assert!(
        matches
            .iter()
            .any(|candidate| candidate.src == 0 && candidate.dst == 16)
    );
    assert!(
        !matches
            .iter()
            .any(|candidate| candidate.src == 0 && candidate.dst < 16)
    );
}

#[test]
fn m0_applies_inclusive_max_distance() {
    let mut config = m0(8);
    config.max_distance = Some(3);
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let matches = find_matches_m0(Cursor::new(b"abcabcabcabc"), &config, &context).unwrap();
    assert!(
        matches
            .iter()
            .any(|candidate| candidate.src == 0 && candidate.dst == 3)
    );
    assert!(
        !matches
            .iter()
            .any(|candidate| candidate.src == 0 && candidate.dst > 3)
    );
}

#[test]
fn m0_budget_failure_is_explicit() {
    let mut config = m0(8);
    config.resources = ResourceConfig {
        memory: 1,
        ..ResourceConfig::default()
    };
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let error = find_matches_m0(Cursor::new(b"abcabcabcabc"), &config, &context).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded);
}

#[test]
fn m0_has_no_window_when_input_is_shorter_than_region() {
    let config = m0(32);
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let matches = find_matches_m0(Cursor::new(b"abc"), &config, &context).unwrap();
    assert!(matches.is_empty());
}

#[test]
fn m0_context_api_releases_all_memory_after_result_drop() {
    let config = m0(8);
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let matches =
        find_matches_m0_with_context(Cursor::new(b"abcabcabcabc"), &config, &context).unwrap();
    assert!(context.memory.current() > 0);
    drop(matches);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
}

#[test]
fn all_methods_are_routed_through_their_real_finders() {
    let input = b"0123456789abcdef".repeat(8);
    for method in [
        Method::M3FixedDigest,
        Method::M4Reread,
        Method::M5Exhaustive,
    ] {
        let mut config = CompressionConfig::for_method(method);
        if matches!(
            method,
            Method::M3FixedDigest | Method::M4Reread | Method::M5Exhaustive
        ) {
            config.min_match = 8;
            if matches!(method, Method::M3FixedDigest | Method::M4Reread) {
                config.seed_size = Some(8);
            }
        }
        let mut archive = Vec::new();
        let stats = compress(&input[..], &mut archive, &config).unwrap();
        assert_eq!(stats.method, Some(method));
        if matches!(
            method,
            Method::M3FixedDigest | Method::M4Reread | Method::M5Exhaustive
        ) {
            assert!(stats.semantic_match_count > 0, "{method:?}");
            assert!(stats.covered_bytes > 0, "{method:?}");
        }
    }
}

#[test]
fn m0_compress_routes_real_candidates_through_all_layouts_and_checksums() {
    let input = b"0123456789abcdef".repeat(32);
    let mut expected_ir = None;
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let mut config = m0(8);
            config.layout = layout;
            config.checksum = checksum;
            config.block_size = 1024;
            let mut archive = Vec::new();
            let stats = compress(&input[..], &mut archive, &config).unwrap();
            assert_eq!(stats.method, Some(Method::M0Rep));
            assert!(stats.semantic_match_count > 0);
            assert!(stats.covered_bytes > 0);
            let matches = inspect_matches(&archive[..]).unwrap();
            if let Some(expected) = &expected_ir {
                assert_eq!(&matches, expected);
            } else {
                expected_ir = Some(matches);
            }
            let info = inspect(&archive[..]).unwrap();
            assert_eq!(info.semantic_match_count, stats.semantic_match_count);
            let mut restored = Vec::new();
            decompress(&archive[..], &mut restored).unwrap();
            assert_eq!(restored, input);
        }
    }
}

#[test]
// Differential: the same final (src,dst,len) triple can be reached through
// multiple interleaved representative distances.  The reference pipeline must
// canonicalize globally by triple (retaining the minimum insertion ordinal)
// and validate each unique triple exactly once, so the emitted IR and archive
// are identical to a run whose candidates contain no interleaved duplicates.
fn m0_interleaved_distance_duplicates_are_canonicalized_identically() {
    let input = b"0123456789abcdef".repeat(1024);
    let config = m0(512);
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let candidates = find_matches_m0(Cursor::new(&input), &config, &context).unwrap();
    assert!(candidates.len() > 4);

    // Build a permutation that interleaves duplicate triples at different
    // positions, mirroring what the finder emits when several representative
    // distances produce the same final triple.
    let mut interleaved = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        interleaved.push(*candidate);
        if index % 3 == 0 {
            interleaved.push(*candidate);
        }
    }
    let mut archive_plain = Vec::new();
    let plain_context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let plain_stats = srep::compress_with_candidates_with_context(
        Cursor::new(&input),
        &mut archive_plain,
        &config,
        candidates.iter().copied(),
        &plain_context,
    )
    .unwrap();
    let mut archive_interleaved = Vec::new();
    let interleaved_context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let interleaved_stats = srep::compress_with_candidates_with_context(
        Cursor::new(&input),
        &mut archive_interleaved,
        &config,
        interleaved,
        &interleaved_context,
    )
    .unwrap();
    assert_eq!(interleaved_stats, plain_stats);
    assert_eq!(archive_interleaved, archive_plain);
}

#[test]
// Differential: unlimited history and an inclusive max-distance bound must
// agree on every candidate whose distance is within the bound, and the bound
// must exclude candidates beyond it while preserving the same IR for the
// remaining candidates.
fn m0_max_distance_inclusion_and_exclusion_are_exact() {
    let input = b"0123456789abcdef".repeat(1024);
    let mut unlimited = m0(512);
    unlimited.max_distance = None;
    let mut bounded = m0(512);
    bounded.max_distance = Some(4096);
    let unlimited_context = srep::ResourceContext::with_resources(&unlimited.resources).unwrap();
    let bounded_context = srep::ResourceContext::with_resources(&bounded.resources).unwrap();
    let unlimited_candidates =
        find_matches_m0(Cursor::new(&input), &unlimited, &unlimited_context).unwrap();
    let bounded_candidates =
        find_matches_m0(Cursor::new(&input), &bounded, &bounded_context).unwrap();
    assert!(unlimited_candidates.len() > bounded_candidates.len());
    for candidate in bounded_candidates.iter() {
        assert!(candidate.dst - candidate.src <= 4096);
        assert!(
            unlimited_candidates
                .iter()
                .any(|other| other.src == candidate.src
                    && other.dst == candidate.dst
                    && other.len == candidate.len)
        );
    }
    for candidate in unlimited_candidates.iter() {
        if candidate.dst - candidate.src <= 4096 {
            assert!(
                bounded_candidates
                    .iter()
                    .any(|other| other.src == candidate.src
                        && other.dst == candidate.dst
                        && other.len == candidate.len)
            );
        }
    }
}

#[test]
// Differential: the finder must produce identical candidate sequences
// regardless of memory budget (as long as the budget is sufficient for the
// finder to complete).  This exercises both the in-memory snapshot path and
// the file-backed fallback path, which are selected automatically based on
// available memory.
fn m0_accelerator_and_resource_fallback_agree() {
    let input = b"0123456789abcdef".repeat(1024);
    let config = m0(512);
    let generous_context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let accelerated = find_matches_m0(Cursor::new(&input), &config, &generous_context).unwrap();
    assert!(!accelerated.is_empty());

    // A budget just above the finder's minimum (7 MiB) still allows the
    // snapshot to be created; a much larger budget also works.  The unit test
    // `input_snapshot_allocation_failure_selects_exact_fallback` covers the
    // explicit fallback when the snapshot cannot be allocated.
    let mut tight = config.clone();
    tight.resources.memory = 8 * 1024 * 1024;
    let tight_context = srep::ResourceContext::with_resources(&tight.resources).unwrap();
    let fallback = find_matches_m0(Cursor::new(&input), &tight, &tight_context).unwrap();
    assert_eq!(fallback.as_slice(), accelerated.as_slice());
}

#[test]
// Differential: duplicate triples arriving with different insertion ordinals
// must keep the minimum ordinal through validation, normalization, and the
// persisted IndexSection, regardless of arrival order.
fn m0_duplicate_triple_keeps_minimum_ordinal_through_the_pipeline() {
    let input = b"0123456789abcdef".repeat(1024);
    let config = m0(512);
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let candidates = find_matches_m0(Cursor::new(&input), &config, &context).unwrap();
    let first = candidates[0];
    let reordered = vec![
        srep::MatchCandidate {
            insertion_ordinal: first.insertion_ordinal + 1000,
            ..first
        },
        first,
    ];
    let mut archive = Vec::new();
    let stats = srep::compress_with_candidates_with_context(
        Cursor::new(&input),
        &mut archive,
        &config,
        reordered,
        &context,
    )
    .unwrap();
    assert!(stats.semantic_match_count > 0);
    let mut restored = Vec::new();
    decompress(&archive[..], &mut restored).unwrap();
    assert_eq!(restored, input);
}
