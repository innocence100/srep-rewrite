use std::io::Cursor;

use srep::{
    Checksum, CompressionConfig, ErrorKind, Layout, Match, MatchCandidate, ResourceConfig,
    compress_with_candidates, decompress, decompress_with_resources, normalize_matches,
};

fn record_offsets(archive: &[u8]) -> Vec<(u8, usize, usize)> {
    let header = srep::format::parse_archive_header(&archive[..80]).unwrap();
    let mut cursor = 80usize;
    let mut records = Vec::new();
    while cursor + 12 <= archive.len() - 64 {
        let kind = archive[cursor];
        let payload_len =
            u64::from_le_bytes(archive[cursor + 4..cursor + 12].try_into().unwrap()) as usize;
        records.push((kind, cursor, payload_len));
        cursor += 12 + payload_len + header.checksum.width();
    }
    records
}

fn refresh_record_checksum(archive: &mut [u8], offset: usize, payload_len: usize) {
    let header = srep::format::parse_archive_header(&archive[..80]).unwrap();
    let frame: [u8; 12] = archive[offset..offset + 12].try_into().unwrap();
    let payload = &archive[offset + 12..offset + 12 + payload_len];
    let checksum = srep::checksum::record_checksum(header.checksum, &frame, payload);
    archive[offset + 12 + payload_len..offset + 12 + payload_len + checksum.len()]
        .copy_from_slice(&checksum);
}

fn candidate(src: u64, dst: u64, len: u64, insertion_ordinal: u64) -> MatchCandidate {
    MatchCandidate {
        src,
        dst,
        len,
        insertion_ordinal,
    }
}

#[test]
fn normalizer_uses_global_weighted_schedule_and_canonical_origin_ids() {
    let candidates = [
        candidate(0, 30, 40, 0),
        candidate(30, 70, 40, 1),
        candidate(0, 30, 81, 2),
    ];
    let normalized = normalize_matches(candidates, 200, 2).unwrap();
    assert_eq!(
        normalized.matches,
        vec![Match {
            src: 0,
            dst: 30,
            len: 81,
            origin_match_id: 0,
        }]
    );
    assert_eq!(normalized.covered_bytes, 81);
    assert_eq!(normalized.literal_bytes, 119);
}

#[test]
fn normalizer_is_permutation_deterministic_and_deduplicates_ordinals() {
    let forward = [
        candidate(0, 40, 30, 9),
        candidate(1, 40, 30, 3),
        candidate(80, 120, 26, 2),
        candidate(50, 80, 26, 1),
    ];
    let reverse = forward.into_iter().rev().collect::<Vec<_>>();
    let a = normalize_matches(forward, 200, 2).unwrap();
    let b = normalize_matches(reverse, 200, 2).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.matches[0].src, 0);
}

#[test]
fn normalizer_retains_minimum_ordinal_for_interleaved_duplicate_triples() {
    let candidates = [
        candidate(0, 16, 32, 9),
        candidate(0, 16, 32, 4),
        candidate(0, 16, 32, 7),
    ];
    let normalized = normalize_matches(candidates, 64, 2).unwrap();
    assert_eq!(normalized.matches.len(), 1);
    assert_eq!(normalized.matches[0].src, 0);
    assert_eq!(normalized.matches[0].dst, 16);
    assert_eq!(normalized.matches[0].len, 32);
}

#[test]
fn candidate_encoder_round_trips_all_layouts_with_same_ir() {
    let input = b"0123456789abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz";
    let candidates = [candidate(0, 36, 36, 0)];
    let mut semantic = None;
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            ..CompressionConfig::default()
        };
        let mut archive = Vec::new();
        let stats = compress_with_candidates(Cursor::new(input), &mut archive, &config, candidates)
            .unwrap();
        assert_eq!(stats.semantic_match_count, 1);
        assert_eq!(stats.covered_bytes, 36);
        let mut decoded = Vec::new();
        decompress(archive.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, input);
        let current = (
            stats.semantic_match_count,
            stats.covered_bytes,
            stats.literal_bytes,
            decoded,
        );
        if let Some(previous) = &semantic {
            assert_eq!(previous, &current);
        } else {
            semantic = Some(current);
        }
    }
}

#[test]
fn overlapping_match_crosses_destination_blocks_for_all_layouts_and_checksums() {
    let input = b"abc".repeat(500);
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let config = CompressionConfig {
                layout,
                checksum,
                block_size: 1024,
                min_match: 2,
                seed_size: Some(2),
                ..CompressionConfig::default()
            };
            let mut archive = Vec::new();
            let stats = compress_with_candidates(
                &input[..],
                &mut archive,
                &config,
                [candidate(0, 999, 300, 0)],
            )
            .unwrap();
            assert_eq!(stats.covered_bytes, 300);
            let mut output = Vec::new();
            let decoded_stats = decompress(&archive[..], &mut output).unwrap();
            assert_eq!(decoded_stats.original_size, input.len() as u64);
            assert_eq!(
                output.len(),
                input.len(),
                "layout={layout:?} checksum={checksum:?}"
            );
            let mismatch = output.iter().zip(&input).position(|(a, b)| *a != *b);
            assert_eq!(
                output, input,
                "layout={layout:?} checksum={checksum:?}, first mismatch={mismatch:?}"
            );
        }
    }
}

#[test]
fn short_candidates_are_omitted_even_when_minimum_is_larger() {
    let normalized = normalize_matches([candidate(0, 30, 25, 0)], 100, 512).unwrap();
    assert!(normalized.matches.is_empty());
    assert_eq!(normalized.literal_bytes, 100);
}

#[test]
fn unverified_candidate_is_rejected_before_archive_output() {
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    let error = compress_with_candidates(
        b"abcdefgh".as_slice(),
        &mut archive,
        &config,
        [candidate(0, 4, 26, 0)],
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidMatch);
    assert!(archive.is_empty());
}

#[test]
fn inspect_matches_exposes_the_same_canonical_ir_for_each_layout() {
    let input = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let expected = vec![Match {
        src: 0,
        dst: 32,
        len: 32,
        origin_match_id: 0,
    }];
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            ..CompressionConfig::default()
        };
        let mut archive = Vec::new();
        compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)])
            .unwrap();
        assert_eq!(srep::inspect_matches(&archive[..]).unwrap(), expected);
    }
}

#[test]
fn candidate_archive_decodes_from_nonseekable_input_with_bounded_resources() {
    let input = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let config = CompressionConfig {
        layout: Layout::Future,
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 16, 16, 0)]).unwrap();
    let resources = ResourceConfig {
        memory: 1,
        temp_limit: 128 * 1024,
        ..ResourceConfig::default()
    };
    let mut output = Vec::new();
    decompress_with_resources(Cursor::new(archive), &mut output, &resources).unwrap();
    assert_eq!(output, input);
}

#[test]
fn valid_match_archive_can_decode_with_one_byte_memory_using_temp_storage() {
    let input = b"0123456789abcdef".repeat(16);
    let config = CompressionConfig {
        layout: Layout::Index,
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(
        &input[..],
        &mut archive,
        &config,
        [candidate(0, 16, 240, 0)],
    )
    .unwrap();
    let resources = ResourceConfig {
        memory: 1,
        temp_limit: 128 * 1024,
        ..ResourceConfig::default()
    };
    let mut output = Vec::new();
    let error =
        decompress_with_resources(Cursor::new(archive), &mut output, &resources).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded);
}

#[test]
fn candidate_resource_budget_fails_before_collecting_unaccounted_candidates() {
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        resources: ResourceConfig {
            memory: 1,
            ..ResourceConfig::default()
        },
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    let error = compress_with_candidates(
        b"0123456789abcdef0123456789abcdef".as_slice(),
        &mut archive,
        &config,
        [candidate(0, 32, 32, 0)],
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded);
    assert!(archive.is_empty());
}

#[test]
fn representation_metadata_mutation_fails_the_datablock_checksum() {
    let input = b"0123456789abcdef".repeat(4);
    for layout in [Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            ..CompressionConfig::default()
        };
        let mut archive = Vec::new();
        compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)])
            .unwrap();
        let data_payload_start = 80 + 92 + 92 + 12;
        let mutations = match layout {
            Layout::Future => vec![
                (data_payload_start + 48 + 8, 16u64),
                (data_payload_start + 48 + 32, 16u64),
            ],
            Layout::Io => vec![(data_payload_start + 48 + 48 + 16, 16u64)],
            Layout::Index => unreachable!(),
        };
        for (mutation, value) in mutations {
            archive[mutation..mutation + 8].copy_from_slice(&value.to_le_bytes());
        }
        let error = srep::verify(&archive[..]).unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::ChecksumMismatch,
            "{layout:?}: {error}"
        );
    }
}

#[test]
fn index_counts_are_checked_from_the_fixed_header_before_any_large_allocation() {
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let input = b"0123456789abcdef".repeat(4);
    let mut archive = Vec::new();
    compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)]).unwrap();
    let (kind, offset, payload_len) = record_offsets(&archive)
        .into_iter()
        .find(|(kind, _, _)| *kind == srep::format::RECORD_INDEX_SECTION)
        .unwrap();
    assert_eq!(kind, srep::format::RECORD_INDEX_SECTION);
    archive[offset + 12 + 8..offset + 12 + 16].copy_from_slice(&u64::MAX.to_le_bytes());
    refresh_record_checksum(&mut archive, offset, payload_len);
    let error = srep::verify(&archive[..]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::CorruptIndex);
}

#[test]
fn reference_decoder_rejects_wrong_block_count_before_reading_blocks() {
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(b"literal".as_slice(), &mut archive, &config, []).unwrap();
    let (_, offset, payload_len) = record_offsets(&archive)
        .into_iter()
        .find(|(kind, _, _)| *kind == srep::format::RECORD_LAYOUT_METADATA)
        .unwrap();
    archive[offset + 12 + 16..offset + 12 + 24].copy_from_slice(&2u64.to_le_bytes());
    refresh_record_checksum(&mut archive, offset, payload_len);
    assert_eq!(
        srep::verify(&archive[..]).unwrap_err().kind(),
        ErrorKind::CorruptRecord
    );
}

#[test]
fn checksum_valid_directory_field_mutations_are_rejected_as_corrupt_index() {
    let input = b"0123456789abcdef".repeat(4);
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)]).unwrap();
    let (_, directory_offset, directory_len) = record_offsets(&archive)
        .into_iter()
        .find(|(kind, _, _)| *kind == srep::format::RECORD_BLOCK_DIRECTORY)
        .unwrap();
    for field in [24usize, 32, 40, 48, 56] {
        let mut mutated = archive.clone();
        let value = if field == 48 { 1u64 } else { u64::MAX };
        let begin = directory_offset + 12 + 16 + field;
        mutated[begin..begin + 8].copy_from_slice(&value.to_le_bytes());
        refresh_record_checksum(&mut mutated, directory_offset, directory_len);
        let error = srep::verify(&mutated[..]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::CorruptIndex, "field {field}");
    }
}

#[test]
fn stage3_archive_byte_mutations_never_panic() {
    let input = b"0123456789abcdef".repeat(4);
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            ..CompressionConfig::default()
        };
        let mut archive = Vec::new();
        compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)])
            .unwrap();
        for index in 0..archive.len() {
            let mut mutated = archive.clone();
            mutated[index] ^= 1;
            let result = std::panic::catch_unwind(|| srep::verify(&mutated[..]));
            assert!(
                result.is_ok(),
                "mutation panic at {layout:?} offset {index}"
            );
        }
    }
}

#[test]
fn impossible_datablock_counts_are_structural_before_memory_budget() {
    let input = b"literal";
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            ..CompressionConfig::default()
        };
        let mut archive = Vec::new();
        compress_with_candidates(&input[..], &mut archive, &config, []).unwrap();
        let (_, offset, payload_len) = record_offsets(&archive)
            .into_iter()
            .find(|(kind, _, _)| *kind == srep::format::RECORD_DATA_BLOCK)
            .unwrap();
        let count_offset = offset + 12 + 32;
        archive[count_offset..count_offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        refresh_record_checksum(&mut archive, offset, payload_len);
        let resources = ResourceConfig {
            memory: 1,
            ..ResourceConfig::default()
        };
        let error = srep::verify_with_resources(&archive[..], &resources).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::CorruptRecord, "{layout:?}");
    }
}

#[test]
fn candidate_temp_archive_and_history_share_one_budget() {
    let input = b"0123456789abcdef".repeat(8);
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            ..CompressionConfig::default()
        };
        let mut archive = Vec::new();
        compress_with_candidates(
            &input[..],
            &mut archive,
            &config,
            [candidate(0, 16, 112, 0)],
        )
        .unwrap();
        let probe_resources = ResourceConfig {
            temp_limit: 128 * 1024,
            ..ResourceConfig::default()
        };
        let probe_context = srep::ResourceContext::with_resources(&probe_resources).unwrap();
        let mut probe_output = Vec::new();
        srep::decompress_with_context(
            Cursor::new(archive.clone()),
            &mut probe_output,
            &probe_resources,
            &probe_context,
        )
        .unwrap();
        assert_eq!(probe_output, input);
        let exact = probe_context.temp.high_water();
        assert!(exact >= archive.len() as u64 + input.len() as u64);
        for temp_limit in [exact, exact - 1] {
            let resources = ResourceConfig {
                temp_limit,
                ..ResourceConfig::default()
            };
            let context = srep::ResourceContext::with_resources(&resources).unwrap();
            let mut output = Vec::new();
            let result = srep::decompress_with_context(
                Cursor::new(archive.clone()),
                &mut output,
                &resources,
                &context,
            );
            if temp_limit == exact {
                result.unwrap();
                assert_eq!(output, input);
            } else {
                assert_eq!(result.unwrap_err().kind(), ErrorKind::TempBudgetExceeded);
                assert!(output.is_empty());
            }
            assert_eq!(context.temp.current(), 0, "{layout:?}, limit={temp_limit}");
            assert!(context.temp.high_water() <= temp_limit);
        }
    }
}

#[test]
fn candidate_decoder_releases_memory_reservations_after_success() {
    let input = b"0123456789abcdef".repeat(8);
    let config = CompressionConfig {
        layout: Layout::Future,
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(
        &input[..],
        &mut archive,
        &config,
        [candidate(0, 16, 112, 0)],
    )
    .unwrap();
    let resources = ResourceConfig {
        memory: 4096,
        temp_limit: 128 * 1024,
        ..ResourceConfig::default()
    };
    let context = srep::ResourceContext::with_resources(&resources).unwrap();
    let mut output = Vec::new();
    srep::decompress_with_context(&archive[..], &mut output, &resources, &context).unwrap();
    assert_eq!(output, input);
    assert!(context.memory.high_water() <= resources.memory);
    assert_eq!(context.memory.current(), 0);
}

#[test]
fn checksum_valid_operation_count_mutations_are_rejected() {
    let input = b"0123456789abcdef".repeat(4);
    for layout in [Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            ..CompressionConfig::default()
        };
        let mut archive = Vec::new();
        compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)])
            .unwrap();
        let (_, data_offset, _) = record_offsets(&archive)
            .into_iter()
            .find(|(kind, _, _)| *kind == srep::format::RECORD_DATA_BLOCK)
            .unwrap();
        let operation_field = data_offset + 12 + 40;
        archive[operation_field..operation_field + 8].copy_from_slice(&99u64.to_le_bytes());
        let frame: [u8; 12] = archive[data_offset..data_offset + 12].try_into().unwrap();
        let payload_len = u64::from_le_bytes(frame[4..12].try_into().unwrap()) as usize;
        let payload = &archive[data_offset + 12..data_offset + 12 + payload_len];
        let header = srep::format::parse_archive_header(&archive[..80]).unwrap();
        let block =
            srep::checksum::block_checksum(header.checksum, &frame, payload, 0, 0, &input[..])
                .unwrap();
        let checksum_start = data_offset + 12 + payload_len;
        archive[checksum_start..checksum_start + block.len()].copy_from_slice(&block);
        assert_eq!(
            srep::verify(&archive[..]).unwrap_err().kind(),
            ErrorKind::CorruptRecord
        );
    }
}

#[test]
fn checksum_valid_index_range_mutations_are_rejected_as_corrupt_index() {
    let input = b"0123456789abcdef".repeat(4);
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)]).unwrap();
    let (_, index_offset, index_len) = record_offsets(&archive)
        .into_iter()
        .find(|(kind, _, _)| *kind == srep::format::RECORD_INDEX_SECTION)
        .unwrap();
    for value in [1u64, u64::MAX] {
        let mut mutated = archive.clone();
        let range_start = index_offset + 12 + 32 + 24;
        mutated[range_start + 8..range_start + 16].copy_from_slice(&value.to_le_bytes());
        refresh_record_checksum(&mut mutated, index_offset, index_len);
        assert_eq!(
            srep::verify(&mutated[..]).unwrap_err().kind(),
            ErrorKind::CorruptIndex
        );
    }
}

#[test]
fn checksum_valid_datablock_count_mutations_are_rejected_as_corrupt_record() {
    let input = b"0123456789abcdef".repeat(4);
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            ..CompressionConfig::default()
        };
        let mut archive = Vec::new();
        compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)])
            .unwrap();
        let (_, data_offset, payload_len) = record_offsets(&archive)
            .into_iter()
            .find(|(kind, _, _)| *kind == srep::format::RECORD_DATA_BLOCK)
            .unwrap();
        let count_offset = data_offset + 12 + 32;
        archive[count_offset..count_offset + 8].copy_from_slice(&99u64.to_le_bytes());
        refresh_record_checksum(&mut archive, data_offset, payload_len);
        assert_eq!(
            srep::verify(&archive[..]).unwrap_err().kind(),
            ErrorKind::CorruptRecord,
            "{layout:?}"
        );
    }
}

#[test]
fn checksum_valid_index_match_length_overflow_is_invalid_match_and_never_panics() {
    let input = b"0123456789abcdef".repeat(4);
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)]).unwrap();
    let (_, index_offset, index_len) = record_offsets(&archive)
        .into_iter()
        .find(|(kind, _, _)| *kind == srep::format::RECORD_INDEX_SECTION)
        .unwrap();
    let entry_len = index_offset + 12 + 32 + 16;
    archive[entry_len..entry_len + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    refresh_record_checksum(&mut archive, index_offset, index_len);
    let result = std::panic::catch_unwind(|| srep::verify(&archive[..]));
    assert!(result.is_ok());
    assert_eq!(result.unwrap().unwrap_err().kind(), ErrorKind::InvalidMatch);
}

#[test]
fn candidate_endpoint_overflow_is_rejected_for_every_layout() {
    let input = b"0123456789abcdef".repeat(4);
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            ..CompressionConfig::default()
        };
        let mut archive = Vec::new();
        let error = compress_with_candidates(
            &input[..],
            &mut archive,
            &config,
            [candidate(0, u64::MAX, 26, 0)],
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidMatch, "{layout:?}");
        assert!(archive.is_empty());
    }
}

#[test]
fn io_origin_ending_at_block_boundary_can_close_before_next_block_literal() {
    let mut input = vec![b'x'; 1400];
    for index in 0..124 {
        input[900 + index] = b'x';
    }
    let config = CompressionConfig {
        layout: Layout::Io,
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(
        &input[..],
        &mut archive,
        &config,
        [candidate(0, 900, 124, 0)],
    )
    .unwrap();
    let mut output = Vec::new();
    let stats = decompress(&archive[..], &mut output).unwrap();
    assert_eq!(output, input);
    assert_eq!(stats.semantic_match_count, 1);
}

#[test]
fn io_origin_continues_across_a_destination_block_boundary() {
    let input = b"abc".repeat(800);
    let config = CompressionConfig {
        layout: Layout::Io,
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(
        &input[..],
        &mut archive,
        &config,
        [candidate(0, 999, 900, 0)],
    )
    .unwrap();
    let mut output = Vec::new();
    let stats = decompress(&archive[..], &mut output).unwrap();
    assert_eq!(output, input);
    assert_eq!(stats.semantic_match_count, 1);
    assert_eq!(stats.covered_bytes, 900);
}

#[test]
fn io_noncontiguous_same_origin_after_boundary_is_invalid_match() {
    let input = b"abc".repeat(800);
    let config = CompressionConfig {
        layout: Layout::Io,
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(
        &input[..],
        &mut archive,
        &config,
        [candidate(0, 999, 900, 0)],
    )
    .unwrap();
    let (_, first_data, first_len) = record_offsets(&archive)
        .into_iter()
        .find(|(kind, _, _)| *kind == srep::format::RECORD_DATA_BLOCK)
        .unwrap();
    let second_data = first_data + 12 + first_len + Checksum::Xxh3.width();
    let operation = second_data + 12 + 48;
    let operation_len =
        u32::from_le_bytes(archive[operation + 4..operation + 8].try_into().unwrap()) as usize;
    if operation_len == 40 {
        archive[operation + 16..operation + 24].copy_from_slice(&1u64.to_le_bytes());
    }
    let error = srep::verify(&archive[..]).unwrap_err();
    assert!(matches!(
        error.kind(),
        ErrorKind::InvalidMatch | ErrorKind::ChecksumMismatch
    ));
}

#[test]
fn normalization_result_keeps_memory_reservation_until_drop() {
    let budget = srep::MemoryBudget::new(4096);
    let result =
        srep::normalize_matches_with_budget([candidate(0, 32, 32, 0)], 64, 2, &budget).unwrap();
    assert!(budget.current() > 0);
    drop(result);
    assert_eq!(budget.current(), 0);
}

#[test]
fn convenience_normalizer_uses_fallible_default_budgeted_storage() {
    let result =
        normalize_matches([candidate(0, 32, 32, 0), candidate(1, 33, 31, 1)], 64, 2).unwrap();
    assert_eq!(result.matches.len(), 1);
}

#[test]
fn inspected_matches_keeps_memory_reservation_until_drop() {
    let input = b"0123456789abcdef".repeat(4);
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)]).unwrap();
    let resources = ResourceConfig {
        memory: 4096,
        temp_limit: 128 * 1024,
        ..ResourceConfig::default()
    };
    let context = srep::ResourceContext::with_resources(&resources).unwrap();
    let inspected = srep::inspect_matches_with_context(&archive[..], &resources, &context).unwrap();
    assert_eq!(inspected.as_slice().len(), 1);
    assert!(context.memory.current() > 0);
    drop(inspected);
    assert_eq!(context.memory.current(), 0);
}

#[test]
fn inspected_matches_api_keeps_owned_collection_alive() {
    let input = b"0123456789abcdef".repeat(4);
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)]).unwrap();
    let inspected = srep::inspect_matches(&archive[..]).unwrap();
    assert_eq!(inspected.as_slice().len(), 1);
    assert_eq!(inspected.as_slice()[0].dst, 32);
}

#[test]
fn checksum_valid_index_section_length_u64_max_is_invalid_match_without_panic() {
    let input = b"0123456789abcdef".repeat(4);
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(&input[..], &mut archive, &config, [candidate(0, 32, 32, 0)]).unwrap();
    let (_, offset, payload_len) = record_offsets(&archive)
        .into_iter()
        .find(|(kind, _, _)| *kind == srep::format::RECORD_INDEX_SECTION)
        .unwrap();
    archive[offset + 4..offset + 12].copy_from_slice(&u64::MAX.to_le_bytes());
    refresh_record_checksum(&mut archive, offset, payload_len);
    let result = std::panic::catch_unwind(|| srep::verify(&archive[..]));
    assert!(result.is_ok());
    assert_eq!(result.unwrap().unwrap_err().kind(), ErrorKind::InvalidMatch);
}

#[test]
fn all_layouts_reject_source_and_destination_endpoint_overflows() {
    let input = b"0123456789abcdef".repeat(4);
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            ..CompressionConfig::default()
        };
        for candidate_value in [
            candidate(u64::MAX - 1, u64::MAX - 1, 26, 0),
            candidate(0, u64::MAX, 26, 0),
        ] {
            let mut archive = Vec::new();
            let error =
                compress_with_candidates(&input[..], &mut archive, &config, [candidate_value])
                    .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidMatch, "{layout:?}");
            assert!(archive.is_empty());
        }
    }
}

#[test]
fn budgeted_vec_accounts_capacity_before_growth_and_releases_on_drop() {
    let budget = srep::MemoryBudget::new(256);
    let mut values = srep::BudgetedVec::<u64>::new(&budget).unwrap();
    assert_eq!(values.capacity(), 0);
    values.push(7).unwrap();
    assert!(values.capacity() >= values.len());
    assert!(budget.current() >= (values.capacity() * std::mem::size_of::<u64>()) as u64);
    let high_water = budget.high_water();
    assert!(high_water > 0);
    drop(values);
    assert_eq!(budget.current(), 0);
    assert_eq!(budget.high_water(), high_water);
}

#[test]
fn budgeted_vec_failed_growth_rolls_back_without_bare_escape() {
    let budget = srep::MemoryBudget::new(80);
    let mut values = srep::BudgetedVec::<u64>::new(&budget).unwrap();
    values.push(1).unwrap();
    values.push(2).unwrap();
    values.push(3).unwrap();
    values.push(4).unwrap();
    let before = values.as_slice().to_vec();
    assert!(values.push(5).is_err());
    assert_eq!(values.as_slice(), before.as_slice());
    assert_eq!(
        budget.current(),
        (values.capacity() * std::mem::size_of::<u64>()) as u64
    );
}

#[test]
fn budgeted_vec_failed_growth_preserves_elements_and_reservation() {
    let budget = srep::MemoryBudget::new(80);
    let mut values = srep::BudgetedVec::<u64>::with_capacity(2, &budget).unwrap();
    values.push(1).unwrap();
    values.push(2).unwrap();
    values.push(3).unwrap();
    values.push(4).unwrap();
    let before = values.as_slice().to_vec();
    let current = budget.current();
    assert!(values.push(5).is_err());
    assert_eq!(values.as_slice(), before.as_slice());
    assert_eq!(budget.current(), current);
}

#[test]
fn budgeted_vec_capacity_accounting_is_exact_for_zst_and_shared_budgets() {
    #[derive(Debug, PartialEq, Eq)]
    struct ZeroSized;

    let budget = srep::MemoryBudget::new(96);
    let mut zst = srep::BudgetedVec::<ZeroSized>::new(&budget).unwrap();
    for _ in 0..10_000 {
        zst.push(ZeroSized).unwrap();
    }
    assert_eq!(budget.current(), 0);

    let mut first = srep::BudgetedVec::<u64>::with_capacity(4, &budget).unwrap();
    let second = srep::BudgetedVec::<u64>::with_capacity(4, &budget).unwrap();
    assert_eq!(
        budget.current(),
        (first.capacity() + second.capacity()) as u64 * std::mem::size_of::<u64>() as u64
    );
    for value in 0..4 {
        first.push(value).unwrap();
    }
    assert!(first.push(4).is_err());
    assert_eq!(first.as_slice(), &[0, 1, 2, 3]);
    drop(second);
    first.push(4).unwrap();
    assert_eq!(first.as_slice(), &[0, 1, 2, 3, 4]);
    drop(first);
    drop(zst);
    assert_eq!(budget.current(), 0);
}

#[test]
fn budgeted_vec_resize_panic_keeps_capacity_reservation_and_elements() {
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Debug)]
    struct PanicClone {
        clones: Rc<Cell<usize>>,
    }

    impl Clone for PanicClone {
        fn clone(&self) -> Self {
            let count = self.clones.get();
            self.clones.set(count + 1);
            assert!(count < 1, "intentional clone panic");
            Self {
                clones: Rc::clone(&self.clones),
            }
        }
    }

    let budget = srep::MemoryBudget::new(256);
    let clones = Rc::new(Cell::new(0));
    let mut values = srep::BudgetedVec::with_capacity(1, &budget).unwrap();
    values
        .push(PanicClone {
            clones: Rc::clone(&clones),
        })
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = values.resize(
            10,
            PanicClone {
                clones: Rc::clone(&clones),
            },
        );
    }));
    assert!(result.is_err());
    assert_eq!(
        budget.current(),
        (values.capacity() * std::mem::size_of::<PanicClone>()) as u64
    );
    assert!(!values.is_empty());
    drop(values);
    assert_eq!(budget.current(), 0);
}

#[test]
fn budgeted_vec_resize_panic_preserves_prefix_and_exact_capacity_accounting() {
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Debug, PartialEq, Eq)]
    struct CloneProbe {
        state: Rc<Cell<u8>>,
        value: u8,
    }

    impl Clone for CloneProbe {
        fn clone(&self) -> Self {
            let count = self.state.get();
            self.state.set(count + 1);
            assert!(count < 2, "intentional clone panic");
            Self {
                state: Rc::clone(&self.state),
                value: self.value,
            }
        }
    }

    let budget = srep::MemoryBudget::new(512);
    let state = Rc::new(Cell::new(0));
    let mut values = srep::BudgetedVec::with_capacity(1, &budget).unwrap();
    values
        .push(CloneProbe {
            state: Rc::clone(&state),
            value: 7,
        })
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = values.resize(
            12,
            CloneProbe {
                state: Rc::clone(&state),
                value: 9,
            },
        );
    }));
    assert!(result.is_err());
    assert_eq!(values.first().map(|value| value.value), Some(7));
    assert_eq!(
        budget.current(),
        (values.capacity() * std::mem::size_of::<CloneProbe>()) as u64
    );
    drop(values);
    assert_eq!(budget.current(), 0);
}

#[test]
fn candidate_memory_failure_writes_no_partial_output_for_all_layouts() {
    let input = vec![b'x'; 1024];
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            resources: ResourceConfig {
                memory: 512,
                ..ResourceConfig::default()
            },
            ..CompressionConfig::default()
        };
        let mut output = Vec::new();
        let error = compress_with_candidates(
            &input[..],
            &mut output,
            &config,
            [candidate(0, 512, 512, 0)],
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded, "{layout:?}");
        assert!(output.is_empty(), "{layout:?} wrote partial output");
    }
}

#[test]
fn candidate_context_memory_failure_reports_zero_current_and_bounded_high_water() {
    let input = vec![b'x'; 1024];
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = CompressionConfig {
            layout,
            block_size: 1024,
            min_match: 2,
            seed_size: Some(2),
            resources: ResourceConfig {
                memory: 512,
                ..ResourceConfig::default()
            },
            ..CompressionConfig::default()
        };
        let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
        let mut output = Vec::new();
        let error = srep::compress_with_candidates_with_context(
            &input[..],
            &mut output,
            &config,
            [candidate(0, 512, 512, 0)],
            &context,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded, "{layout:?}");
        assert!(output.is_empty(), "{layout:?}");
        assert_eq!(context.memory.current(), 0, "{layout:?}");
        assert!(context.memory.high_water() <= 512, "{layout:?}");
    }
}

#[test]
fn checksum_valid_io_literal_length_overflow_is_corrupt_record_without_panic() {
    let input = b"0123456789abcdef".repeat(8);
    let config = CompressionConfig {
        layout: Layout::Io,
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        ..CompressionConfig::default()
    };
    let mut archive = Vec::new();
    compress_with_candidates(
        &input[..],
        &mut archive,
        &config,
        [candidate(0, 16, 112, 0)],
    )
    .unwrap();
    let (_, offset, payload_len) = record_offsets(&archive)
        .into_iter()
        .find(|(kind, _, _)| *kind == srep::format::RECORD_DATA_BLOCK)
        .unwrap();
    let literal_len = offset + 12 + 48 + 8;
    archive[literal_len..literal_len + 8].copy_from_slice(&(u64::MAX - 15).to_le_bytes());
    let frame: [u8; 12] = archive[offset..offset + 12].try_into().unwrap();
    let payload = &archive[offset + 12..offset + 12 + payload_len];
    let digest =
        srep::checksum::block_checksum(config.checksum, &frame, payload, 0, 0, &input[..]).unwrap();
    let checksum_start = offset + 12 + payload_len;
    archive[checksum_start..checksum_start + digest.len()].copy_from_slice(&digest);
    let result = std::panic::catch_unwind(|| srep::verify(&archive[..]));
    assert!(result.is_ok());
    assert_eq!(
        result.unwrap().unwrap_err().kind(),
        ErrorKind::CorruptRecord
    );
}

#[test]
fn large_candidate_plan_fails_before_output_when_plan_memory_is_insufficient() {
    let input = vec![b'x'; 1024 * 1024];
    let config = CompressionConfig {
        block_size: 1024,
        min_match: 2,
        seed_size: Some(2),
        resources: ResourceConfig {
            memory: 512,
            ..ResourceConfig::default()
        },
        ..CompressionConfig::default()
    };
    let mut output = Vec::new();
    let error = compress_with_candidates(&input[..], &mut output, &config, []).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded);
    assert!(output.is_empty());
}
