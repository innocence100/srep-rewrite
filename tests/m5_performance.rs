use std::io::Cursor;
use std::time::Instant;

use srep::{
    CandidateIndex, CompressionConfig, HybridCandidateIndex, IndexEntry, MatchCandidate, Method,
    ResourceConfig, ResourceContext, find_matches_m5, m5_seed_size,
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

fn far_distance_like(size: usize, prefix: usize) -> Vec<u8> {
    let mut data = vec![0u8; size];
    for index in 0..prefix {
        data[index] = (index.wrapping_mul(17) + 3) as u8;
        data[size - prefix + index] = data[index];
    }
    for (index, slot) in data.iter_mut().enumerate().take(size - prefix).skip(prefix) {
        *slot = (index.wrapping_mul(31) + 11) as u8;
    }
    data
}

#[test]
fn far_distance_root_cause_is_independent_extension_not_interval_state() {
    let input = far_distance_like(24 * 1024, 4 * 1024);
    let minimum = 512usize;
    let started = Instant::now();
    let produced = find_matches_m5(Cursor::new(&input), &config(minimum as u64), &{
        let cfg = config(minimum as u64);
        ResourceContext::with_resources(&cfg.resources).unwrap()
    })
    .unwrap()
    .as_slice()
    .to_vec();
    let elapsed = started.elapsed();
    let expected = m5_oracle(&input, minimum);
    assert_eq!(produced, expected);
    assert!(
        produced.iter().any(|candidate| {
            candidate.src == 0
                && candidate.dst == (input.len() - 4 * 1024) as u64
                && candidate.len >= 4 * 1024
        }),
        "missing independent far-distance candidate"
    );
    assert!(
        elapsed.as_secs() < 30,
        "independent exact search exceeded 30s: {elapsed:?}"
    );
}

#[test]
fn polynomial_collision_still_requires_exact_compare() {
    let first = b"abcdWXYZ".to_vec();
    let second = b"efghABCD".to_vec();
    let mut input = first.clone();
    input.extend_from_slice(&second);
    assert_ne!(first, second);
    let produced = find_matches_m5(Cursor::new(&input), &config(8), &{
        let cfg = config(8);
        ResourceContext::with_resources(&cfg.resources).unwrap()
    })
    .unwrap();
    assert!(produced.is_empty());
    let equal = b"abcdWXYZ".repeat(2);
    let matched = find_matches_m5(Cursor::new(&equal), &config(8), &{
        let cfg = config(8);
        ResourceContext::with_resources(&cfg.resources).unwrap()
    })
    .unwrap();
    assert!(
        matched
            .iter()
            .any(|candidate| candidate.src == 0 && candidate.dst == 8)
    );
}

#[test]
fn far_distance_2mib_finder_completes_without_oracle() {
    let prefix = 256 * 1024;
    let data = far_distance_like(2 * 1024 * 1024, prefix);
    let mut cfg = config(512);
    cfg.resources.memory = 256 * 1024 * 1024;
    cfg.resources.temp_limit = 1024 * 1024 * 1024;
    let context = ResourceContext::with_resources(&cfg.resources).unwrap();
    let started = Instant::now();
    let produced = find_matches_m5(Cursor::new(&data), &cfg, &context).unwrap();
    let elapsed = started.elapsed();
    assert!(
        produced.iter().any(|candidate| {
            candidate.src == 0
                && candidate.dst == (data.len() - prefix) as u64
                && candidate.len >= prefix as u64
        }),
        "missing independent 2MiB far-distance candidate"
    );
    assert!(
        elapsed.as_secs() < 180,
        "2MiB far-distance exceeded 180s: {elapsed:?}"
    );
    eprintln!(
        "2MiB far-distance m5 finder: {elapsed:?} candidates={}",
        produced.len()
    );
}

#[test]
fn spilled_index_query_matches_ram_and_keeps_temp_budget() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut hybrid =
        HybridCandidateIndex::with_memtable_bytes_in(&context, temp_dir.path(), 1).unwrap();
    let ram_budget = srep::MemoryBudget::new(64 * 1024);
    let mut ram = srep::RamCandidateIndex::new(&ram_budget).unwrap();
    let key = 9u64.to_le_bytes();
    for position in 0..10u64 {
        let value = IndexEntry::new(0, &key, position * 4, position, &[]).unwrap();
        hybrid.insert(value).unwrap();
        hybrid.finish_epoch().unwrap();
        ram.insert(value).unwrap();
    }
    let before_temp = context.temp.current();
    let mut expected = Vec::new();
    ram.for_each_candidate(0, &key, 50, 0, &mut |value| {
        expected.push(value);
        Ok(())
    })
    .unwrap();
    let mut actual = Vec::new();
    hybrid
        .for_each_candidate(0, &key, 50, 0, &mut |value| {
            actual.push(value);
            Ok(())
        })
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(context.temp.current(), before_temp);
}
