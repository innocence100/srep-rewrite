use std::io::Cursor;
use std::time::Instant;

use srep::{
    Checksum, CompressionConfig, Layout, Method, ResourceConfig, ResourceContext,
    compress_with_candidates, compress_with_context, decompress, find_matches_m3, find_matches_m4,
    inspect_matches, normalize_matches,
};

const SAMPLE03_SHA256: &str = "52f13e80d570d4e8fb9924d8de7ee234f81968fd79b9742cb17d834f66f4bec5";

fn default_fixed_config(method: Method, memory: u64) -> CompressionConfig {
    let resources = ResourceConfig {
        memory,
        temp_limit: memory,
        ..ResourceConfig::default()
    };
    let mut config = CompressionConfig::for_method(method);
    config.layout = Layout::Index;
    config.checksum = Checksum::Xxh3;
    config.block_size = 8 * 1024;
    config.min_match = 512;
    config.seed_size = Some(512);
    config.resources = resources;
    config
}

fn deterministic_bytes(size: usize, seed: u64) -> Vec<u8> {
    let mut state = seed ^ 0x9E3779B97F4A7C15;
    let mut result = Vec::with_capacity(size);
    for _ in 0..size {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        result.push(((state >> 32) & 0xFF) as u8);
    }
    result
}

fn frozen_sample03_bytes() -> Vec<u8> {
    let seed = 16642u64;
    let block = deterministic_bytes(32768, seed);
    let inserted = {
        let mut value = b"INSERTED-REGION-v2\0".to_vec();
        value.extend_from_slice(&deterministic_bytes(1237, seed + 1));
        value
    };
    let deleted = 911usize;
    let source = block.repeat(4);
    let mut shifted = source[..2 * block.len() - deleted].to_vec();
    shifted.extend_from_slice(&inserted);
    shifted.extend_from_slice(&source[2 * block.len()..]);
    let mut data = source.clone();
    data.extend_from_slice(&shifted);
    data.extend_from_slice(&source);
    data.truncate(524288);
    data.resize(524288, 0);
    data
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unique_with_plants(size: usize, seed: usize) -> Vec<u8> {
    let mut input = deterministic_bytes(size, 0xC0FFEE);
    for byte in input.iter_mut() {
        if *byte == 0 {
            *byte = 1;
        }
    }
    let mut pattern = Vec::with_capacity(seed);
    let mut state = 0x51ED_u64;
    for _ in 0..seed {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        pattern.push(((state >> 24) as u8) | 1);
    }
    input[..seed].copy_from_slice(&pattern);
    input[seed..2 * seed].copy_from_slice(&pattern);
    input
}

fn prefix_key(bytes: &[u8]) -> [u8; 8] {
    let mut key = [0u8; 8];
    let copied = bytes.len().min(8);
    key[..copied].copy_from_slice(&bytes[..copied]);
    key
}

fn seed_lookup(input: &[u8], seed: usize) -> std::collections::HashMap<[u8; 8], Vec<usize>> {
    let mut by_seed = std::collections::HashMap::<[u8; 8], Vec<usize>>::new();
    for source in (0..=input.len().saturating_sub(seed)).step_by(seed) {
        by_seed
            .entry(prefix_key(&input[source..source + seed]))
            .or_default()
            .push(source);
    }
    by_seed
}

fn m3_oracle(input: &[u8], seed: usize, minimum: usize) -> Vec<srep::MatchCandidate> {
    let by_seed = seed_lookup(input, seed);
    let mut result = Vec::new();
    for target in 0..=input.len().saturating_sub(seed) {
        let Some(sources) = by_seed.get(&prefix_key(&input[target..target + seed])) else {
            continue;
        };
        for &source_start in sources.iter().rev() {
            if source_start >= target
                || input[source_start..source_start + seed] != input[target..target + seed]
            {
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
                result.push(srep::MatchCandidate {
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

fn m4_oracle(input: &[u8], seed: usize, minimum: usize) -> Vec<srep::MatchCandidate> {
    let by_seed = seed_lookup(input, seed);
    let mut result = Vec::new();
    for target in 0..=input.len().saturating_sub(seed) {
        let Some(sources) = by_seed.get(&prefix_key(&input[target..target + seed])) else {
            continue;
        };
        for &source_start in sources.iter().rev() {
            if source_start >= target
                || input[source_start..source_start + seed] != input[target..target + seed]
            {
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
                result.push(srep::MatchCandidate {
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

fn assert_triples_ordinals_ir_archive(
    input: &[u8],
    method: Method,
    config: &CompressionConfig,
    context: &ResourceContext,
    produced: &[srep::MatchCandidate],
    expected: &[srep::MatchCandidate],
) {
    assert_eq!(produced, expected, "{method:?} triples+ordinals");
    let ir = normalize_matches(
        produced.iter().copied(),
        input.len() as u64,
        config.min_match,
    )
    .unwrap();
    let mut compact_archive = Vec::new();
    let compact_stats =
        compress_with_context(Cursor::new(input), &mut compact_archive, config, context).unwrap();
    let mut full_archive = Vec::new();
    compress_with_candidates(
        Cursor::new(input),
        &mut full_archive,
        config,
        produced.iter().copied(),
    )
    .unwrap();
    assert_eq!(compact_archive, full_archive, "{method:?} archive bytes");
    let inspected = inspect_matches(compact_archive.as_slice()).unwrap();
    assert_eq!(inspected.as_slice(), ir.as_slice(), "{method:?} IR");
    assert_eq!(compact_stats.semantic_match_count, ir.len() as u64);
    assert_eq!(compact_stats.covered_bytes, ir.covered_bytes);
    let mut restored = Vec::new();
    decompress(compact_archive.as_slice(), &mut restored).unwrap();
    assert_eq!(restored, input);
}

#[test]
fn m3_full_compression_regression_uses_real_archive_path() {
    let mut input = Vec::with_capacity(256 * 1024);
    for block in 0..512u32 {
        let mut chunk = [0u8; 512];
        for (index, byte) in chunk.iter_mut().enumerate() {
            *byte = (block.wrapping_mul(17) as usize + index * 29) as u8;
        }
        input.extend_from_slice(&chunk);
    }
    let repeated = input[..64 * 1024].to_vec();
    input.extend_from_slice(&repeated);
    let config = default_fixed_config(Method::M3FixedDigest, 256 * 1024 * 1024);
    let context = ResourceContext::with_resources(&config.resources).unwrap();
    let mut archive = Vec::new();
    let stats = compress_with_context(Cursor::new(&input), &mut archive, &config, &context)
        .expect("full m3 compression should succeed");
    assert!(stats.semantic_match_count > 0);
    let mut restored = Vec::new();
    decompress(archive.as_slice(), &mut restored).unwrap();
    assert_eq!(restored, input);
}

#[test]
fn m3_compact_codec_path_matches_full_ir_and_archive_for_trailing_zeros() {
    let mut input = deterministic_bytes(2048, 7);
    input.resize(8192, 0);
    let mut config = default_fixed_config(Method::M3FixedDigest, 64 * 1024 * 1024);
    config.min_match = 64;
    config.seed_size = Some(64);
    config.block_size = 1024;
    let context = ResourceContext::with_resources(&config.resources).unwrap();
    let full = find_matches_m3(Cursor::new(&input), &config, &context).unwrap();
    assert!(
        full.len() > 64,
        "trailing zeros must produce many unique m3 candidates, got {}",
        full.len()
    );
    let unique_triples = {
        let mut keys = full
            .iter()
            .map(|candidate| (candidate.src, candidate.dst, candidate.len))
            .collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        keys.len()
    };
    assert_eq!(unique_triples, full.len(), "m3 trailing-zero set is unique");
    let full_ir =
        normalize_matches(full.iter().copied(), input.len() as u64, config.min_match).unwrap();
    let mut compact_archive = Vec::new();
    let compact_stats =
        compress_with_context(Cursor::new(&input), &mut compact_archive, &config, &context)
            .unwrap();
    let mut full_archive = Vec::new();
    compress_with_candidates(
        Cursor::new(&input),
        &mut full_archive,
        &config,
        full.iter().copied(),
    )
    .unwrap();
    assert_eq!(compact_archive, full_archive);
    let inspected = inspect_matches(compact_archive.as_slice()).unwrap();
    assert_eq!(inspected.as_slice(), full_ir.as_slice());
    assert_eq!(compact_stats.semantic_match_count, full_ir.len() as u64);
    assert_eq!(compact_stats.covered_bytes, full_ir.covered_bytes);
    let mut restored = Vec::new();
    decompress(compact_archive.as_slice(), &mut restored).unwrap();
    assert_eq!(restored, input);
}

#[test]
fn acceleration_threshold_matches_independent_bytewise_oracle() {
    let seed = 16 * 1024usize;
    let above = unique_with_plants(256 * 1024, seed);
    assert_eq!(above.len(), 256 * 1024);
    for method in [Method::M3FixedDigest, Method::M4Reread] {
        let mut config = default_fixed_config(method, 256 * 1024 * 1024);
        config.min_match = seed as u64;
        config.seed_size = Some(seed as u64);
        config.block_size = 1024;
        let context = ResourceContext::with_resources(&config.resources).unwrap();
        let produced = match method {
            Method::M3FixedDigest => find_matches_m3(Cursor::new(&above), &config, &context),
            Method::M4Reread => find_matches_m4(Cursor::new(&above), &config, &context),
            _ => unreachable!(),
        }
        .unwrap();
        let expected = match method {
            Method::M3FixedDigest => m3_oracle(&above, seed, seed),
            Method::M4Reread => m4_oracle(&above, seed, seed),
            _ => unreachable!(),
        };
        assert_eq!(
            produced.as_slice(),
            expected.as_slice(),
            "{method:?} above-threshold independent oracle"
        );
        assert_triples_ordinals_ir_archive(
            &above,
            method,
            &config,
            &context,
            produced.as_slice(),
            &expected,
        );
        drop(produced);
        assert_eq!(context.memory.current(), 0, "{method:?} above released");
        assert_eq!(context.temp.current(), 0, "{method:?} above temp released");
    }
}

#[test]
fn m3_frozen_sample03_completes_within_default_budget() {
    let input = frozen_sample03_bytes();
    assert_eq!(input.len(), 524288);
    assert_eq!(sha256_hex(&input), SAMPLE03_SHA256);
    let config = default_fixed_config(Method::M3FixedDigest, 256 * 1024 * 1024);
    let context = ResourceContext::with_resources(&config.resources).unwrap();
    let started = Instant::now();
    let mut archive = Vec::new();
    let stats = compress_with_context(Cursor::new(&input), &mut archive, &config, &context)
        .expect("frozen sample03 m3 must fit the default 256MiB budget");
    let elapsed = started.elapsed();
    eprintln!(
        "sample03 m3 compress matches={} covered={} literals={} archive={} time={elapsed:?} mem_hw={} checksum={}",
        stats.semantic_match_count,
        stats.covered_bytes,
        stats.literal_bytes,
        archive.len(),
        context.memory.high_water(),
        sha256_hex(&archive)
    );
    assert!(
        elapsed.as_secs() < 120,
        "sample03 m3 took {elapsed:?}, exceeding the 120s probe"
    );
    assert!(stats.semantic_match_count > 0);
    assert!(stats.covered_bytes > 0);
    let inspected = inspect_matches(archive.as_slice()).unwrap();
    assert_eq!(inspected.len() as u64, stats.semantic_match_count);
    let mut restored = Vec::new();
    decompress(archive.as_slice(), &mut restored).unwrap();
    assert_eq!(restored, input);
    assert!(context.memory.high_water() <= context.memory.limit());
}

#[test]
fn m4_frozen_sample03_completes_within_default_budget() {
    let input = frozen_sample03_bytes();
    assert_eq!(input.len(), 524288);
    assert_eq!(sha256_hex(&input), SAMPLE03_SHA256);
    let config = default_fixed_config(Method::M4Reread, 256 * 1024 * 1024);
    let context = ResourceContext::with_resources(&config.resources).unwrap();
    let started = Instant::now();
    let mut archive = Vec::new();
    let stats = compress_with_context(Cursor::new(&input), &mut archive, &config, &context)
        .expect("frozen sample03 m4 must fit the default 256MiB budget");
    let elapsed = started.elapsed();
    eprintln!(
        "sample03 m4 compress matches={} covered={} literals={} archive={} time={elapsed:?} mem_hw={} checksum={}",
        stats.semantic_match_count,
        stats.covered_bytes,
        stats.literal_bytes,
        archive.len(),
        context.memory.high_water(),
        sha256_hex(&archive)
    );
    assert!(
        elapsed.as_secs() < 180,
        "sample03 m4 took {elapsed:?}, exceeding the 180s probe"
    );
    assert!(stats.semantic_match_count > 0);
    assert!(stats.covered_bytes > 0);
    let inspected = inspect_matches(archive.as_slice()).unwrap();
    assert_eq!(inspected.len() as u64, stats.semantic_match_count);
    let mut restored = Vec::new();
    decompress(archive.as_slice(), &mut restored).unwrap();
    assert_eq!(restored, input);
    assert!(context.memory.high_water() <= context.memory.limit());
    drop(archive);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
}
