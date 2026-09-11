use std::io::Cursor;

use srep::{
    Checksum, CompressionConfig, ErrorKind, Layout, ResourceConfig,
    compress_with_candidates_with_context, compress_with_context, find_matches_m0, inspect_matches,
};

fn m0_config(layout: Layout, checksum: Checksum) -> CompressionConfig {
    CompressionConfig {
        layout,
        checksum,
        block_size: 1024,
        min_match: 32,
        max_distance: Some(128),
        resources: ResourceConfig {
            memory: 256 * 1024 * 1024,
            temp_limit: 64 * 1024 * 1024,
            ..ResourceConfig::default()
        },
        ..CompressionConfig::for_method(srep::Method::M0Rep)
    }
}

fn repeated_input() -> Vec<u8> {
    b"0123456789abcdef".repeat(16)
}

#[test]
fn production_src_has_no_stable_sort_calls() {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut stable_calls = Vec::new();
    for entry in walkdir(&source_root) {
        let source = std::fs::read_to_string(&entry).unwrap();
        for (line_number, line) in source.lines().enumerate() {
            if line.contains(".sort_by(") || line.contains(".sort_by_key(") {
                stable_calls.push(format!("{}:{}", entry.display(), line_number + 1));
            }
        }
    }
    assert!(
        stable_calls.is_empty(),
        "stable production sorts: {stable_calls:?}"
    );
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn ordinary_m0_resource_limit_is_exact_and_failure_keeps_output_empty() {
    let input = repeated_input();
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let mut generous = m0_config(layout, Checksum::Xxh3);
        generous.resources.memory = 1024 * 1024;
        let generous_context = srep::ResourceContext::with_resources(&generous.resources).unwrap();
        let mut generous_archive = Vec::new();
        let generous_stats = compress_with_context(
            Cursor::new(&input),
            &mut generous_archive,
            &generous,
            &generous_context,
        )
        .unwrap();
        let generous_high_water = generous_context.memory.high_water();
        eprintln!("ordinary m0 {layout:?}/Xxh3 generous high-water={generous_high_water}");
        assert!(generous_stats.semantic_match_count > 0);
        assert_eq!(generous_context.memory.current(), 0);
        assert_eq!(generous_context.temp.current(), 0);

        let mut generous_exact = generous.clone();
        generous_exact.resources.memory = generous_high_water;
        let generous_exact_context =
            srep::ResourceContext::with_resources(&generous_exact.resources).unwrap();
        let mut generous_exact_archive = Vec::new();
        let generous_exact_stats = compress_with_context(
            Cursor::new(&input),
            &mut generous_exact_archive,
            &generous_exact,
            &generous_exact_context,
        )
        .unwrap();
        assert_eq!(generous_exact_stats, generous_stats);
        assert_eq!(generous_exact_archive, generous_archive);
        assert!(generous_exact_context.memory.high_water() <= generous_high_water);
        assert_eq!(generous_exact_context.memory.current(), 0);

        let mut generous_one_below = generous.clone();
        generous_one_below.resources.memory = generous_high_water - 1;
        let generous_one_below_context =
            srep::ResourceContext::with_resources(&generous_one_below.resources).unwrap();
        let mut generous_one_below_archive = Vec::new();
        let generous_one_below_result = compress_with_context(
            Cursor::new(&input),
            &mut generous_one_below_archive,
            &generous_one_below,
            &generous_one_below_context,
        );
        eprintln!(
            "ordinary m0 {layout:?}/Xxh3 generous one-byte-below={} result={}",
            generous_high_water - 1,
            if generous_one_below_result.is_ok() {
                "success"
            } else {
                "memory-failure"
            }
        );
        if let Err(error) = generous_one_below_result {
            assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded);
            assert!(generous_one_below_archive.is_empty());
        } else {
            assert_eq!(generous_one_below_archive, generous_archive);
        }
        assert_eq!(generous_one_below_context.memory.current(), 0);
        assert!(generous_one_below_context.memory.high_water() < generous_high_water);

        let mut upper = 1024 * 1024;
        loop {
            let mut config = m0_config(layout, Checksum::Xxh3);
            config.resources.memory = upper;
            let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
            let mut archive = Vec::new();
            if compress_with_context(Cursor::new(&input), &mut archive, &config, &context).is_ok() {
                break;
            }
            upper *= 2;
        }

        let mut lower = 1;
        while lower < upper {
            let middle = lower + (upper - lower) / 2;
            let mut config = m0_config(layout, Checksum::Xxh3);
            config.resources.memory = middle;
            let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
            let mut archive = Vec::new();
            if compress_with_context(Cursor::new(&input), &mut archive, &config, &context).is_ok() {
                upper = middle;
            } else {
                lower = middle + 1;
            }
        }
        let threshold = lower;
        let mut calibration = m0_config(layout, Checksum::Xxh3);
        calibration.resources.memory = threshold;
        let calibration_context =
            srep::ResourceContext::with_resources(&calibration.resources).unwrap();
        let mut calibration_archive = Vec::new();
        let calibration_stats = compress_with_context(
            Cursor::new(&input),
            &mut calibration_archive,
            &calibration,
            &calibration_context,
        )
        .unwrap();
        assert!(calibration_stats.semantic_match_count > 0);
        assert_eq!(calibration_stats, generous_stats);
        assert_eq!(calibration_archive, generous_archive);
        let memory_limit = calibration_context.memory.high_water();
        eprintln!(
            "ordinary m0 {layout:?}/Xxh3 threshold={threshold} memory high-water={memory_limit}"
        );
        assert!(memory_limit > 0);
        assert_eq!(calibration_context.memory.current(), 0);
        assert_eq!(calibration_context.temp.current(), 0);

        let exact = calibration.clone();
        let exact_context = srep::ResourceContext::with_resources(&exact.resources).unwrap();
        let mut exact_archive = Vec::new();
        let exact_stats = compress_with_context(
            Cursor::new(&input),
            &mut exact_archive,
            &exact,
            &exact_context,
        )
        .unwrap();
        assert_eq!(exact_stats, calibration_stats);
        assert_eq!(exact_archive, calibration_archive);
        assert!(exact_context.memory.high_water() <= memory_limit);
        assert_eq!(exact_context.memory.current(), 0);

        assert_eq!(threshold, memory_limit);

        let mut tight = exact.clone();
        tight.resources.memory = threshold - 1;
        let tight_context = srep::ResourceContext::with_resources(&tight.resources).unwrap();
        let mut tight_archive = Vec::new();
        let error = compress_with_context(
            Cursor::new(&input),
            &mut tight_archive,
            &tight,
            &tight_context,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded);
        assert!(tight_archive.is_empty());
        assert_eq!(tight_context.memory.current(), 0);
        assert!(tight_context.memory.high_water() <= tight.resources.memory);
    }
}

#[test]
fn ordinary_m0_archives_are_byte_identical_across_layouts_checksums_and_repeats() {
    let input = repeated_input();
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let config = m0_config(layout, checksum);
            let mut expected = None;
            for _ in 0..5 {
                let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
                let mut archive = Vec::new();
                compress_with_context(Cursor::new(&input), &mut archive, &config, &context)
                    .unwrap();
                assert!(!inspect_matches(archive.as_slice()).unwrap().is_empty());
                if let Some(expected) = &expected {
                    assert_eq!(
                        &archive, expected,
                        "layout={layout:?}, checksum={checksum:?}"
                    );
                } else {
                    expected = Some(archive);
                }
            }
        }
    }
}

#[test]
fn candidate_input_permutations_keep_candidates_ir_and_archives_identical() {
    let input = repeated_input();
    let mut config = m0_config(Layout::Future, Checksum::Blake3);
    config.resources.memory = 256 * 1024 * 1024;
    let finder_context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    let candidates = find_matches_m0(Cursor::new(&input), &config, &finder_context)
        .unwrap()
        .as_slice()
        .to_vec();
    assert!(candidates.len() > 4);
    let expected_candidates = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.src,
                candidate.dst,
                candidate.len,
                candidate.insertion_ordinal,
            )
        })
        .collect::<Vec<_>>();
    for _ in 0..4 {
        let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
        let repeated = find_matches_m0(Cursor::new(&input), &config, &context).unwrap();
        let signature = repeated
            .iter()
            .map(|candidate| {
                (
                    candidate.src,
                    candidate.dst,
                    candidate.len,
                    candidate.insertion_ordinal,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(signature, expected_candidates);
    }

    let permutations = [
        candidates.clone(),
        candidates.iter().copied().rev().collect(),
        {
            let mut permutation = candidates.iter().copied().enumerate().collect::<Vec<_>>();
            permutation.sort_unstable_by_key(|(index, _)| index % 3);
            permutation
                .into_iter()
                .map(|(_, candidate)| candidate)
                .collect()
        },
    ];
    let mut expected_ir = None;
    let mut expected_archive = None;
    for permutation in permutations {
        let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
        let mut archive = Vec::new();
        compress_with_candidates_with_context(
            Cursor::new(&input),
            &mut archive,
            &config,
            permutation,
            &context,
        )
        .unwrap();
        let ir = inspect_matches(archive.as_slice()).unwrap();
        if let Some(expected) = &expected_ir {
            assert_eq!(&ir, expected);
        } else {
            expected_ir = Some(ir);
        }
        if let Some(expected) = &expected_archive {
            assert_eq!(&archive, expected);
        } else {
            expected_archive = Some(archive);
        }
    }
}
