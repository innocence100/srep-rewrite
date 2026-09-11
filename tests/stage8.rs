use std::fs::{self, OpenOptions};
use std::io::{Cursor, Seek, SeekFrom, Write};

use srep::{
    CandidateIndex, CompressionConfig, ErrorKind, HybridCandidateIndex, IndexEntry, Method,
    ResourceContext,
};

// The ignored acceptance test is run manually with:
// cargo test --release --test stage8 ignored_m1_finder_discovers_a_match_across_256_mib_history -- --ignored --nocapture

fn key(value: u64) -> [u8; 8] {
    value.to_le_bytes()
}

fn entry(position: u64, ordinal: u64) -> IndexEntry {
    IndexEntry::new(0, &key(9), position, ordinal, &[]).unwrap()
}

#[test]
fn candidate_index_handles_positions_beyond_256_mib_without_truncation() {
    let context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    let mut index = HybridCandidateIndex::with_memtable_bytes(&context, 1).unwrap();
    let position = 256 * 1024 * 1024 + 17;
    index.insert(entry(position, 4)).unwrap();
    let mut observed = Vec::new();
    index
        .for_each_candidate(0, &key(9), position + 1, 0, &mut |value| {
            observed.push(value);
            Ok(())
        })
        .unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].position, position);
}

#[test]
fn corrupted_visible_run_is_reported_and_does_not_emit_candidates() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut index = HybridCandidateIndex::with_memtable_bytes_in(
        &context,
        temp_dir.path(),
        srep::candidate_index::INDEX_RECORD_LEN as u64,
    )
    .unwrap();
    index.insert(entry(10, 0)).unwrap();
    index.finish_epoch().unwrap();
    let path = index.run_paths().pop().unwrap();
    let mut file = OpenOptions::new().write(true).open(&path).unwrap();
    let length = file.metadata().unwrap().len();
    file.seek(SeekFrom::Start(length - 1)).unwrap();
    file.write_all(&[0]).unwrap();
    file.sync_all().unwrap();

    let mut observed = Vec::new();
    let error = index
        .for_each_candidate(0, &key(9), 11, 0, &mut |value| {
            observed.push(value);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::CorruptIndex);
    assert!(observed.is_empty());
}

#[test]
fn compaction_keeps_run_count_bounded_and_releases_all_resources() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut index =
        HybridCandidateIndex::with_memtable_bytes_in(&context, temp_dir.path(), 1).unwrap();
    for position in 0..17 {
        index.insert(entry(position * 2, position)).unwrap();
        index.finish_epoch().unwrap();
    }
    assert!(index.run_count() <= 16);
    assert!(context.temp.current() > 0);
    let mut observed = Vec::new();
    index
        .for_each_candidate(0, &key(9), 40, 0, &mut |value| {
            observed.push(value.position);
            Ok(())
        })
        .unwrap();
    assert_eq!(observed.len(), 17);
    drop(index);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert!(fs::read_dir(temp_dir.path()).unwrap().next().is_none());
}

#[test]
fn more_than_sixteen_runs_preserve_all_entries_and_callback_failure_cleans_query_runs() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut index =
        HybridCandidateIndex::with_memtable_bytes_in(&context, temp_dir.path(), 1).unwrap();
    for position in 0..33u64 {
        index.insert(entry(position * 2, position)).unwrap();
        index.finish_epoch().unwrap();
    }
    let run_bytes = context.temp.current();
    let mut observed = Vec::new();
    index
        .for_each_candidate(0, &key(9), 100, 0, &mut |value| {
            observed.push(value.position);
            Ok(())
        })
        .unwrap();
    assert_eq!(observed.len(), 33);
    assert_eq!(context.temp.current(), run_bytes);

    let error = index
        .for_each_candidate(0, &key(9), 100, 0, &mut |_| {
            Err(srep::Error::invalid_match("stop callback"))
        })
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidMatch);
    assert_eq!(context.temp.current(), run_bytes);
}

#[test]
fn failed_seventeenth_run_keeps_prior_paths_and_entries_unchanged() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut index = HybridCandidateIndex::with_memtable_bytes_in(
        &context,
        temp_dir.path(),
        srep::candidate_index::INDEX_RECORD_LEN as u64,
    )
    .unwrap();
    for position in 0..16u64 {
        index.insert(entry(position * 2, position)).unwrap();
        index.finish_epoch().unwrap();
    }
    let paths = index.run_paths();
    let before_temp = context.temp.current();
    assert_eq!(index.run_count(), 16);
    let run_size =
        (srep::candidate_index::INDEX_HEADER_LEN + srep::candidate_index::INDEX_RECORD_LEN) as u64;
    let _held = context
        .temp
        .reserve(context.temp.limit() - before_temp - run_size)
        .unwrap();
    index.insert(entry(32, 16)).unwrap();
    let error = index.finish_epoch().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TempBudgetExceeded);
    assert_eq!(index.run_paths(), paths);
    assert_eq!(index.run_count(), 16);
    assert_eq!(context.temp.current(), context.temp.limit() - run_size);
}

#[test]
fn failed_seventeenth_insert_is_atomic_and_retryable() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut index =
        HybridCandidateIndex::with_memtable_bytes_in(&context, temp_dir.path(), 1).unwrap();
    for position in 0..16u64 {
        index.insert(entry(position * 2, position)).unwrap();
        index.finish_epoch().unwrap();
    }
    assert_eq!(index.run_count(), 16);

    let before_paths = index.run_paths();
    let before_files = fs::read_dir(temp_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    let before_generation = index.next_generation();
    let before_spills = context.candidate_index_spill_count();
    let before_memory = context.memory.current();
    let before_temp = context.temp.current();
    let run_size =
        (srep::candidate_index::INDEX_HEADER_LEN + srep::candidate_index::INDEX_RECORD_LEN) as u64;
    let held = context
        .temp
        .reserve(context.temp.limit() - before_temp - run_size * 2)
        .unwrap();
    let held_current = context.temp.current();
    let error = index.insert(entry(32, 16)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TempBudgetExceeded);
    assert_eq!(before_generation, 32);
    assert_eq!(before_temp, run_size * 16);
    assert_eq!(before_spills, 16);
    assert_eq!(index.run_paths(), before_paths);
    assert_eq!(index.run_count(), 16);
    assert_eq!(index.next_generation(), before_generation);
    assert_eq!(index.memtable_entries(), &[]);
    assert_eq!(context.candidate_index_spill_count(), before_spills);
    assert_eq!(context.memory.current(), before_memory);
    assert_eq!(context.temp.current(), held_current);
    assert!(context.temp.high_water() > held_current);
    let after_failure_files = fs::read_dir(temp_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(after_failure_files, before_files);

    drop(held);
    let mut observed = Vec::new();
    index
        .for_each_candidate(0, &key(9), 33, 0, &mut |value| {
            observed.push(value.position);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        observed,
        (0..16)
            .rev()
            .map(|position| position * 2)
            .collect::<Vec<_>>()
    );

    index.insert(entry(32, 16)).unwrap();
    assert!(index.memtable_entries().is_empty());
    assert!(index.run_count() <= 16);
    assert_eq!(index.next_generation(), before_generation + 34);
    let mut retried = Vec::new();
    index
        .for_each_candidate(0, &key(9), 33, 0, &mut |value| {
            retried.push((value.position, value.insertion_ordinal));
            Ok(())
        })
        .unwrap();
    assert_eq!(retried.len(), 17);
    assert_eq!(
        retried
            .iter()
            .filter(|&&(position, _)| position == 32)
            .count(),
        1
    );
    assert_eq!(context.candidate_index_spill_count(), before_spills + 1);
    drop(index);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert!(temp_dir.path().read_dir().unwrap().next().is_none());
}

#[test]
fn full_memory_insert_checkpoints_memtable_and_incoming_together() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut index = HybridCandidateIndex::with_memtable_bytes_in(
        &context,
        temp_dir.path(),
        2 * std::mem::size_of::<IndexEntry>() as u64,
    )
    .unwrap();

    index.insert(entry(10, 7)).unwrap();
    assert_eq!(index.memtable_entries(), &[entry(10, 7)]);
    index.insert(entry(20, 3)).unwrap();
    assert_eq!(index.memtable_entries(), &[entry(20, 3), entry(10, 7)]);
    index.insert(entry(30, 1)).unwrap();

    assert!(index.memtable_entries().is_empty());
    let mut observed = Vec::new();
    index
        .for_each_candidate(0, &key(9), 31, 0, &mut |value| {
            observed.push((value.position, value.insertion_ordinal));
            Ok(())
        })
        .unwrap();
    assert_eq!(observed, vec![(30, 1), (20, 3), (10, 7)]);
    assert_eq!(context.candidate_index_spill_count(), 1);
    drop(index);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert!(temp_dir.path().read_dir().unwrap().next().is_none());
}

#[test]
fn three_source_checkpoint_canonicalizes_runs_memtable_and_incoming_against_ram() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut index = HybridCandidateIndex::with_memtable_bytes_in(
        &context,
        temp_dir.path(),
        2 * std::mem::size_of::<IndexEntry>() as u64,
    )
    .unwrap();
    let oracle_budget = srep::MemoryBudget::new(64 * 1024);
    let mut oracle = srep::RamCandidateIndex::new(&oracle_budget).unwrap();

    for position in 0..16u64 {
        let value = entry(position * 2, position);
        index.insert(value).unwrap();
        index.finish_epoch().unwrap();
        oracle.insert(value).unwrap();
        oracle.finish_epoch().unwrap();
    }
    let old_paths = index.run_paths();
    let before_generation = index.next_generation();
    let before_spills = context.candidate_index_spill_count();
    let run_duplicate = entry(10, 40);
    let memtable_distinct = entry(200, 41);
    let incoming = entry(12, 2);
    index.insert(run_duplicate).unwrap();
    index.insert(memtable_distinct).unwrap();
    oracle.insert(run_duplicate).unwrap();
    oracle.insert(memtable_distinct).unwrap();
    assert_eq!(index.run_count(), 16);
    assert_eq!(
        index.memtable_entries(),
        &[memtable_distinct, run_duplicate]
    );

    index.insert(incoming).unwrap();
    oracle.insert(incoming).unwrap();

    assert!(index.memtable_entries().is_empty());
    assert_eq!(index.run_count(), 2);
    assert_eq!(index.next_generation(), before_generation + 19);
    assert_eq!(context.candidate_index_spill_count(), before_spills + 1);
    assert_eq!(index.run_paths()[0], old_paths[15]);
    assert!(
        !index.run_paths()[1..]
            .iter()
            .any(|path| old_paths[..15].contains(path))
    );

    let mut expected = Vec::new();
    oracle
        .for_each_candidate(0, &key(9), 301, 0, &mut |value| {
            expected.push((value.position, value.insertion_ordinal));
            Ok(())
        })
        .unwrap();
    let mut actual = Vec::new();
    index
        .for_each_candidate(0, &key(9), 301, 0, &mut |value| {
            actual.push((value.position, value.insertion_ordinal));
            Ok(())
        })
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 17);
    assert_eq!(
        actual
            .iter()
            .filter(|&&(position, _)| position == 10)
            .count(),
        1
    );
    assert_eq!(
        actual.iter().find(|&&(position, _)| position == 10),
        Some(&(10, 5))
    );
    assert_eq!(
        actual.iter().find(|&&(position, _)| position == 12),
        Some(&(12, 2))
    );

    let visible_paths = index.run_paths();
    let visible_bytes = visible_paths
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum::<u64>();
    let files = fs::read_dir(temp_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), visible_paths.len());
    assert!(files.iter().all(|path| visible_paths.contains(path)));
    assert_eq!(context.temp.current(), visible_bytes);
    assert_eq!(context.memory.current(), 0);
    drop(oracle);
    drop(index);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert!(temp_dir.path().read_dir().unwrap().next().is_none());
}

#[test]
fn every_finder_has_identical_candidates_and_archives_when_spilled() {
    let input: Vec<u8> = (0..128)
        .map(|index| b"stage8-corpus-"[index % 14])
        .collect();
    let methods = [
        Method::M0Rep,
        Method::M1RollingCdc,
        Method::M2Order1Cdc,
        Method::M3FixedDigest,
        Method::M4Reread,
        Method::M5Exhaustive,
    ];
    for method in methods {
        let method_input: Vec<u8> = if matches!(method, Method::M1RollingCdc | Method::M2Order1Cdc)
        {
            (0..4096)
                .map(|index| b"stage8-corpus-"[index % 14])
                .collect()
        } else {
            input.clone()
        };
        let mut config = CompressionConfig::for_method(method);
        config.block_size = 1024;
        config.min_match = match method {
            Method::M1RollingCdc | Method::M2Order1Cdc => 32,
            _ => 8,
        };
        if matches!(method, Method::M3FixedDigest | Method::M4Reread) {
            config.seed_size = Some(8);
        }
        if matches!(method, Method::M1RollingCdc | Method::M2Order1Cdc) {
            config.target_chunk = Some(64);
        }
        config.validate().unwrap();

        let normal_resources = config.resources.clone();
        let normal_context = ResourceContext::with_resources(&normal_resources).unwrap();
        let spill_dir = tempfile::tempdir().unwrap();
        let mut spill_resources = normal_resources.clone();
        spill_resources.temp_dir = spill_dir.path().to_path_buf();
        let mut spill_context = ResourceContext::with_resources(&spill_resources).unwrap();
        spill_context.candidate_index_memtable_bytes = Some(1);

        let normal = find(method, &method_input, &config, &normal_context);
        let spilled = find(method, &method_input, &config, &spill_context);
        assert_eq!(spilled, normal, "candidate mismatch for {}", method.name());
        assert!(
            spill_context.candidate_index_spill_count() > 0,
            "{} did not exercise a committed spill",
            method.name()
        );

        for layout in [srep::Layout::Index, srep::Layout::Future, srep::Layout::Io] {
            let mut archive_config = config.clone();
            archive_config.layout = layout;
            archive_config.resources = normal_resources.clone();
            let mut normal_archive = Vec::new();
            srep::compress_with_candidates_with_context(
                Cursor::new(&method_input),
                &mut normal_archive,
                &archive_config,
                normal.iter().copied(),
                &normal_context,
            )
            .unwrap();
            archive_config.resources = spill_resources.clone();
            let mut spilled_archive = Vec::new();
            srep::compress_with_candidates_with_context(
                Cursor::new(&method_input),
                &mut spilled_archive,
                &archive_config,
                spilled.iter().copied(),
                &spill_context,
            )
            .unwrap();
            assert_eq!(
                spilled_archive,
                normal_archive,
                "archive mismatch for {} {:?}",
                method.name(),
                layout
            );
        }
    }
}

fn find(
    method: Method,
    input: &[u8],
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Vec<srep::MatchCandidate> {
    let result = match method {
        Method::M0Rep => srep::find_matches_m0(Cursor::new(input), config, context),
        Method::M1RollingCdc => srep::find_matches_m1(Cursor::new(input), config, context),
        Method::M2Order1Cdc => srep::find_matches_m2(Cursor::new(input), config, context),
        Method::M3FixedDigest => srep::find_matches_m3(Cursor::new(input), config, context),
        Method::M4Reread => srep::find_matches_m4(Cursor::new(input), config, context),
        Method::M5Exhaustive => srep::find_matches_m5(Cursor::new(input), config, context),
    }
    .unwrap();
    result.iter().copied().collect()
}

#[test]
#[ignore = "manual acceptance: runs the m1 finder over a generated input larger than 256 MiB"]
fn ignored_m1_finder_discovers_a_match_across_256_mib_history() {
    let input_dir = tempfile::tempdir().unwrap();
    let input_path = input_dir.path().join("large-input.bin");
    let block_size = 1024 * 1024;
    let gap = 256 * 1024 * 1024 + block_size as u64;
    let mut pattern = vec![0u8; block_size];
    let mut state = 0x9e37_79b9u64;
    for byte in &mut pattern {
        state ^= state << 7;
        state ^= state >> 9;
        state ^= state << 8;
        *byte = state as u8;
    }
    let mut input = fs::File::create(&input_path).unwrap();
    input.write_all(&pattern).unwrap();
    let mut filler = vec![0u8; block_size];
    for block in 1..=256u64 {
        let mut value = block.wrapping_mul(0xd6e8_feb8_6659_fd93);
        for byte in &mut filler {
            value ^= value << 7;
            value ^= value >> 9;
            value ^= value << 8;
            *byte = value as u8;
        }
        input.write_all(&filler).unwrap();
    }
    assert_eq!(input.stream_position().unwrap(), gap);
    input.write_all(&pattern).unwrap();
    input.sync_all().unwrap();
    drop(input);

    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.min_match = 32;
    config.target_chunk = Some(1024 * 1024);
    config.block_size = block_size as u64;
    config.resources.memory = 128 * 1024 * 1024;
    config.resources.temp_limit = 2 * 1024 * 1024 * 1024;
    config.resources.temp_dir = input_dir.path().to_path_buf();
    let input = fs::File::open(input_path).unwrap();
    let mut context = ResourceContext::with_resources(&config.resources).unwrap();
    context.candidate_index_memtable_bytes = Some(1);
    let candidates = srep::find_matches_m1(input, &config, &context).unwrap();
    assert!(context.candidate_index_spill_count() > 0);
    assert!(candidates.iter().any(|candidate| {
        candidate.dst >= gap
            && candidate.dst - candidate.src > 256 * 1024 * 1024
            && candidate.len >= config.min_match
    }));
}
