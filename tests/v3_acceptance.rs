use std::io::{Cursor, Read};

use srep::{
    Checksum, CompressionConfig, Layout, MatchCandidate, Method, ResourceConfig, compress,
    decompress, inspect, inspect_matches, verify,
};

fn manual_literal_archive(layout: Layout, checksum: Checksum, plaintext: &[u8]) -> Vec<u8> {
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = layout;
    config.checksum = checksum;
    config.block_size = 1024;
    config.min_match = 32;
    config.target_chunk = Some(4096);
    let header =
        srep::format_v3::ArchiveHeader::from_config(&config, plaintext.len() as u64).unwrap();
    let header_bytes = srep::format_v3::encode_archive_header(&header).unwrap();
    let mut body = Vec::new();
    for block in 0..header.block_count().unwrap() {
        let start = (block * header.block_size) as usize;
        let length =
            srep::format_v3::block_len_at(header.uncompressed_length, header.block_size, block)
                .unwrap() as usize;
        match layout {
            Layout::Index => {}
            Layout::Future => body.push(0),
            Layout::Io => {
                body.push(srep::format_v3::IO_LITERAL_TAG);
                srep::format_v3::encode_uleb128_into(length as u64, &mut body).unwrap();
            }
        }
        body.extend_from_slice(&plaintext[start..start + length]);
        body.extend_from_slice(&srep::format_v3::crc32c_le(
            &plaintext[start..start + length],
        ));
    }
    let index = if layout.is_index() {
        vec![0]
    } else {
        Vec::new()
    };
    let body_end = 80 + body.len() as u64;
    let index_offset = if layout.is_index() { body_end } else { 0 };
    let tail_start = body_end + index.len() as u64;
    let archive_length = tail_start + srep::format_v3::tail_len(checksum) as u64;
    let plain_digest = srep::format_v3::plaintext_digest(checksum, plaintext);
    let mut encoded = srep::format_v3::GlobalDigest::encoded(checksum);
    encoded.update(&header_bytes);
    encoded.update(&body);
    encoded.update(&index);
    let mut tail = srep::format_v3::encode_archive_tail(
        &srep::format_v3::ArchiveTail {
            archive_length,
            index_offset,
            index_total_length: index.len() as u64,
            body_end,
            plaintext_digest: plain_digest.clone(),
            encoded_digest: vec![0; checksum.width()],
        },
        checksum,
    )
    .unwrap();
    encoded.update(&tail[..srep::format_v3::TAIL_PREFIX_LEN + checksum.width()]);
    tail[srep::format_v3::TAIL_PREFIX_LEN + checksum.width()..]
        .copy_from_slice(&encoded.finalize());
    let mut archive = header_bytes.to_vec();
    archive.extend_from_slice(&body);
    archive.extend_from_slice(&index);
    archive.extend_from_slice(&tail);
    assert_eq!(archive.len() as u64, archive_length);
    archive
}

fn manual_index_scaling_archive(checksum: Checksum, length: usize) -> Vec<u8> {
    let plaintext = vec![0x5au8; length];
    let matches = (1..(length / 32))
        .map(|index| srep::Match {
            src: 0,
            dst: (index * 32) as u64,
            len: 32,
            origin_match_id: (index - 1) as u64,
        })
        .collect::<Vec<_>>();
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = Layout::Index;
    config.checksum = checksum;
    config.block_size = 1024;
    config.min_match = 32;
    config.target_chunk = Some(4096);
    let header = srep::format_v3::ArchiveHeader::from_config(&config, length as u64).unwrap();
    let header_bytes = srep::format_v3::encode_archive_header(&header).unwrap();
    let mut body = Vec::new();
    let mut match_cursor = 0usize;
    for block in 0..header.block_count().unwrap() {
        let start = (block * header.block_size) as usize;
        let block_len =
            srep::format_v3::block_len_at(header.uncompressed_length, header.block_size, block)
                .unwrap() as usize;
        for offset in 0..block_len {
            let position = start + offset;
            while match_cursor < matches.len()
                && matches[match_cursor].dst + matches[match_cursor].len <= position as u64
            {
                match_cursor += 1;
            }
            if match_cursor == matches.len() || (position as u64) < matches[match_cursor].dst {
                body.push(plaintext[position]);
            }
        }
        body.extend_from_slice(&srep::format_v3::crc32c_le(
            &plaintext[start..start + block_len],
        ));
    }
    let index = srep::format_v3::encode_compact_index(&matches).unwrap();
    let body_end = 80 + body.len() as u64;
    let tail_start = body_end + index.len() as u64;
    let archive_length = tail_start + srep::format_v3::tail_len(checksum) as u64;
    let plain_digest = srep::format_v3::plaintext_digest(checksum, &plaintext);
    let mut encoded = srep::format_v3::GlobalDigest::encoded(checksum);
    encoded.update(&header_bytes);
    encoded.update(&body);
    encoded.update(&index);
    let mut tail = srep::format_v3::encode_archive_tail(
        &srep::format_v3::ArchiveTail {
            archive_length,
            index_offset: body_end,
            index_total_length: index.len() as u64,
            body_end,
            plaintext_digest: plain_digest.clone(),
            encoded_digest: vec![0; checksum.width()],
        },
        checksum,
    )
    .unwrap();
    encoded.update(&tail[..srep::format_v3::TAIL_PREFIX_LEN + checksum.width()]);
    tail[srep::format_v3::TAIL_PREFIX_LEN + checksum.width()..]
        .copy_from_slice(&encoded.finalize());
    let mut archive = header_bytes.to_vec();
    archive.extend_from_slice(&body);
    archive.extend_from_slice(&index);
    archive.extend_from_slice(&tail);
    archive
}

fn config(layout: Layout, checksum: Checksum) -> CompressionConfig {
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = layout;
    config.checksum = checksum;
    config.block_size = 1024;
    config.resources = ResourceConfig {
        temp_dir: std::env::temp_dir().join("srep-v3-acceptance"),
        ..ResourceConfig::default()
    };
    config
}

#[test]
fn public_compress_emits_v3_and_all_layouts_round_trip() {
    let input = b"v3-public-api".repeat(500);
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let config = config(layout, checksum);
            let mut archive = Vec::new();
            let stats = compress(Cursor::new(&input), &mut archive, &config).unwrap();
            assert_eq!(&archive[..8], b"SREPNG3\0");
            assert!(verify(Cursor::new(&archive)).is_ok());
            let info = inspect(Cursor::new(&archive)).unwrap();
            assert_eq!(info.version, 3);
            assert_eq!(info.layout, Some(layout));
            assert_eq!(info.checksum, Some(checksum));
            let matches = inspect_matches(Cursor::new(&archive)).unwrap();
            assert_eq!(matches.as_slice().len() as u64, stats.semantic_match_count);
            let mut restored = Vec::new();
            let decoded = decompress(Cursor::new(&archive), &mut restored).unwrap();
            assert_eq!(decoded.original_size, input.len() as u64);
            assert_eq!(restored, input);
        }
    }
}

#[test]
fn public_writer_emits_v3_and_remains_readable() {
    let config = config(Layout::Index, Checksum::Xxh3);
    let mut archive = Vec::new();
    srep::compress(b"v3 writer compatibility".as_slice(), &mut archive, &config).unwrap();
    assert_eq!(&archive[..8], b"SREPNG3\0");
    let mut output = Vec::new();
    decompress(Cursor::new(&archive), &mut output).unwrap();
    assert_eq!(output, b"v3 writer compatibility");
}

#[test]
fn public_v3_compression_routes_every_finder_method() {
    let input = b"0123456789abcdef".repeat(256);
    for method in [
        Method::M0Rep,
        Method::M1RollingCdc,
        Method::M2Order1Cdc,
        Method::M3FixedDigest,
        Method::M4Reread,
        Method::M5Exhaustive,
    ] {
        let mut config = CompressionConfig::for_method(method);
        config.block_size = 1024;
        config.min_match = 16;
        if matches!(method, Method::M3FixedDigest | Method::M4Reread) {
            config.seed_size = Some(16);
        }
        if matches!(method, Method::M1RollingCdc | Method::M2Order1Cdc) {
            config.target_chunk = Some(4096);
        }
        config.resources = ResourceConfig {
            temp_dir: std::env::temp_dir().join("srep-v3-acceptance-methods"),
            ..ResourceConfig::default()
        };
        let mut archive = Vec::new();
        let stats = compress(Cursor::new(&input), &mut archive, &config).unwrap();
        assert_eq!(archive[..8], srep::format_v3::NG_V3_MAGIC);
        assert_eq!(stats.method, Some(method));
        let mut output = Vec::new();
        decompress(Cursor::new(&archive), &mut output).unwrap();
        assert_eq!(output, input);
    }
}

#[test]
fn public_v3_candidate_api_persists_the_same_ir_in_each_layout() {
    let mut input = b"candidate-api-v3-".repeat(80);
    let source = input[..64].to_vec();
    input.extend_from_slice(&source);
    let candidate = MatchCandidate {
        src: 0,
        dst: 1360,
        len: 64,
        insertion_ordinal: 0,
    };
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
        config.layout = layout;
        config.block_size = 1024;
        config.min_match = 32;
        config.target_chunk = Some(4096);
        config.resources = ResourceConfig {
            temp_dir: std::env::temp_dir().join("srep-v3-acceptance-candidates"),
            ..ResourceConfig::default()
        };
        let mut archive = Vec::new();
        let stats =
            srep::compress_with_candidates(Cursor::new(&input), &mut archive, &config, [candidate])
                .unwrap();
        assert_eq!(stats.semantic_match_count, 1);
        let inspected = inspect_matches(Cursor::new(&archive)).unwrap();
        assert_eq!(inspected.as_slice()[0].dst, candidate.dst);
        let mut output = Vec::new();
        decompress(Cursor::new(&archive), &mut output).unwrap();
        assert_eq!(output, input);
    }
}

#[test]
fn candidate_api_keeps_shorter_overlap_when_it_unlocks_following_gain() {
    let input = vec![0u8; 260];
    let candidates = [
        MatchCandidate {
            src: 0,
            dst: 100,
            len: 60,
            insertion_ordinal: 0,
        },
        MatchCandidate {
            src: 0,
            dst: 100,
            len: 100,
            insertion_ordinal: 1,
        },
        MatchCandidate {
            src: 0,
            dst: 160,
            len: 100,
            insertion_ordinal: 2,
        },
    ];
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = Layout::Index;
    config.block_size = 1024;
    config.min_match = 32;
    let mut archive = Vec::new();
    srep::compress_with_candidates(Cursor::new(&input), &mut archive, &config, candidates).unwrap();
    let inspected = inspect_matches(Cursor::new(&archive)).unwrap();
    assert_eq!(
        inspected
            .iter()
            .map(|item| (item.dst, item.len))
            .collect::<Vec<_>>(),
        vec![(100, 60), (160, 100)]
    );
}

#[test]
fn non_seekable_v3_input_is_decoded_without_input_spooling_for_sequential_layout() {
    struct OneByteReader {
        input: Cursor<Vec<u8>>,
    }
    impl Read for OneByteReader {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if output.is_empty() {
                return Ok(0);
            }
            self.input.read(&mut output[..1])
        }
    }

    let input = b"streaming-v3".repeat(300);
    let config = config(Layout::Future, Checksum::Xxh3);
    let mut archive = Vec::new();
    compress(Cursor::new(&input), &mut archive, &config).unwrap();
    let resources = ResourceConfig {
        temp_dir: config.resources.temp_dir.clone(),
        temp_limit: archive.len() as u64 + 4096,
        ..config.resources.clone()
    };
    let context = srep::ResourceContext::with_resources(&resources).unwrap();
    let mut output = Vec::new();
    srep::decompress_with_context(
        OneByteReader {
            input: Cursor::new(archive),
        },
        &mut output,
        &resources,
        &context,
    )
    .unwrap();
    assert_eq!(output, input);
}

#[test]
fn unknown_v3_header_version_is_not_treated_as_v2_or_legacy() {
    let mut header = vec![0u8; srep::format_v3::HEADER_LEN];
    header[..8].copy_from_slice(&srep::format_v3::NG_V3_MAGIC);
    header[8] = 4;
    let error = verify(Cursor::new(header)).unwrap_err();
    assert_eq!(error.kind(), srep::ErrorKind::UnsupportedVersion);
}

#[test]
fn future_decoder_releases_completed_periods_before_later_registers() {
    let mut input = vec![0u8; 9 * 1024];
    for (index, byte) in input.iter_mut().enumerate() {
        *byte = (index as u32).wrapping_mul(31) as u8;
    }
    let mut candidates = Vec::new();
    for block in 0..8u64 {
        let src = block * 1024;
        let dst = src + 512;
        for offset in 0..512 {
            input[(dst + offset) as usize] = input[(src + offset) as usize];
        }
        candidates.push(MatchCandidate {
            src,
            dst,
            len: 512,
            insertion_ordinal: block,
        });
    }
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = Layout::Future;
    config.block_size = 1024;
    config.min_match = 32;
    config.target_chunk = Some(4096);
    config.resources = ResourceConfig {
        memory: 128 * 1024,
        temp_limit: 2 * 1024 * 1024,
        temp_dir: std::env::temp_dir().join("srep-v3-future-resource"),
        ..ResourceConfig::default()
    };
    let mut archive = Vec::new();
    srep::compress_with_candidates(Cursor::new(&input), &mut archive, &config, candidates).unwrap();
    let resources = config.resources.clone();
    let context = srep::ResourceContext::with_resources(&resources).unwrap();
    let mut restored = Vec::new();
    srep::decompress_with_context(Cursor::new(&archive), &mut restored, &resources, &context)
        .unwrap_or_else(|error| {
            panic!(
                "{error}; memory current={} high={} temp current={} high={}",
                context.memory.current(),
                context.memory.high_water(),
                context.temp.current(),
                context.temp.high_water()
            )
        });
    assert_eq!(restored, input);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
}

#[test]
fn future_decoder_uses_bounded_working_memory_for_many_blocks() {
    let block_count = 128u64;
    let mut input = vec![0u8; (block_count as usize + 1) * 1024];
    for (index, byte) in input.iter_mut().enumerate() {
        *byte = (index as u32).wrapping_mul(17) as u8;
    }
    let mut candidates = Vec::new();
    for block in 0..block_count {
        let src = block * 1024;
        let dst = src + 512;
        for offset in 0..512 {
            input[(dst + offset) as usize] = input[(src + offset) as usize];
        }
        candidates.push(MatchCandidate {
            src,
            dst,
            len: 512,
            insertion_ordinal: block,
        });
    }
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = Layout::Future;
    config.block_size = 1024;
    config.min_match = 32;
    config.target_chunk = Some(4096);
    config.resources = ResourceConfig {
        memory: 256 * 1024,
        temp_limit: 4 * 1024 * 1024,
        temp_dir: std::env::temp_dir().join("srep-v3-future-many-blocks"),
        ..ResourceConfig::default()
    };
    let mut archive = Vec::new();
    srep::compress_with_candidates(Cursor::new(&input), &mut archive, &config, candidates).unwrap();
    let resources = ResourceConfig {
        memory: 8 * 1024,
        temp_limit: 4 * 1024 * 1024,
        temp_dir: config.resources.temp_dir.clone(),
        ..config.resources.clone()
    };
    let context = srep::ResourceContext::with_resources(&resources).unwrap();
    let mut restored = Vec::new();
    srep::decompress_with_context(Cursor::new(&archive), &mut restored, &resources, &context)
        .unwrap_or_else(|error| {
            panic!(
                "{error}; memory current={} high={} temp current={} high={}",
                context.memory.current(),
                context.memory.high_water(),
                context.temp.current(),
                context.temp.high_water()
            )
        });
    assert_eq!(restored, input);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
}

#[test]
fn v3_truncated_tails_are_truncated_but_present_bad_tails_are_corrupt() {
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let config = config(layout, Checksum::Xxh3);
        let mut archive = Vec::new();
        compress(Cursor::new(b"abc"), &mut archive, &config).unwrap();
        for amount in [1usize, 76] {
            let truncated = &archive[..archive.len() - amount];
            let error = verify(Cursor::new(truncated)).unwrap_err();
            assert_eq!(
                error.kind(),
                if layout == Layout::Index {
                    srep::ErrorKind::CorruptRecord
                } else {
                    srep::ErrorKind::TruncatedArchive
                },
                "{layout:?}/{amount}"
            );
        }
        let mut corrupt = archive;
        let tail_start = corrupt.len() - 76;
        corrupt[tail_start] ^= 1;
        let error = verify(Cursor::new(corrupt)).unwrap_err();
        assert_eq!(error.kind(), srep::ErrorKind::CorruptRecord, "{layout:?}");
    }
}

#[test]
fn independent_manual_literal_archives_cover_wire_matrix() {
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            for plaintext in [
                Vec::new(),
                b"abc".to_vec(),
                (0..1025).map(|n| n as u8).collect(),
            ] {
                let archive = manual_literal_archive(layout, checksum, &plaintext);
                let info = inspect(Cursor::new(&archive)).unwrap();
                assert_eq!(info.version, 3);
                assert_eq!(info.layout, Some(layout));
                assert_eq!(info.checksum, Some(checksum));
                assert_eq!(info.original_size, plaintext.len() as u64);
                assert_eq!(info.semantic_match_count, 0);
                let mut output = Vec::new();
                decompress(Cursor::new(&archive), &mut output).unwrap();
                assert_eq!(output, plaintext);
            }
        }
    }
}

#[test]
fn independent_manual_archive_corruptions_keep_structural_error_kinds() {
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let archive = manual_literal_archive(layout, checksum, b"abc");
            let tail_start = archive.len() - srep::format_v3::tail_len(checksum);
            let mut tail_version = archive.clone();
            tail_version[tail_start + 8] = 4;
            assert_eq!(
                verify(Cursor::new(tail_version)).unwrap_err().kind(),
                srep::ErrorKind::CorruptRecord
            );
            let mut header_version = archive.clone();
            header_version[8] = 4;
            assert_eq!(
                verify(Cursor::new(header_version)).unwrap_err().kind(),
                srep::ErrorKind::UnsupportedVersion
            );
            let mut trailing = archive.clone();
            trailing.push(0);
            assert_eq!(
                verify(Cursor::new(trailing)).unwrap_err().kind(),
                srep::ErrorKind::CorruptRecord
            );
            let mut crc = archive.clone();
            let crc_position = match layout {
                Layout::Index => 80 + 3,
                Layout::Future => 80 + 1 + 3,
                Layout::Io => 80 + 1 + 1 + 3,
            };
            crc[crc_position] ^= 1;
            assert_eq!(
                verify(Cursor::new(crc)).unwrap_err().kind(),
                srep::ErrorKind::ChecksumMismatch
            );
        }
    }
}

#[test]
fn manual_archives_isolate_global_digest_and_crc_failures() {
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let archive = manual_literal_archive(layout, checksum, b"abc");
            let mut target_chunk = archive.clone();
            target_chunk[40..48].copy_from_slice(&8192u64.to_le_bytes());
            assert_eq!(
                verify(Cursor::new(target_chunk)).unwrap_err().kind(),
                srep::ErrorKind::ChecksumMismatch,
                "{layout:?}/{checksum:?} header mutation"
            );

            let tail_start = archive.len() - srep::format_v3::tail_len(checksum);
            let mut plaintext_digest = archive.clone();
            plaintext_digest[tail_start + 44] ^= 1;
            assert_eq!(
                verify(Cursor::new(plaintext_digest)).unwrap_err().kind(),
                srep::ErrorKind::ChecksumMismatch,
                "{layout:?}/{checksum:?} plaintext digest"
            );
            let mut encoded_digest = archive.clone();
            let encoded_start = tail_start + 44 + checksum.width();
            encoded_digest[encoded_start] ^= 1;
            assert_eq!(
                verify(Cursor::new(encoded_digest)).unwrap_err().kind(),
                srep::ErrorKind::ChecksumMismatch,
                "{layout:?}/{checksum:?} encoded digest"
            );
            let mut crc = archive;
            let crc_position = match layout {
                Layout::Index => 83,
                Layout::Future => 84,
                Layout::Io => 85,
            };
            crc[crc_position] ^= 1;
            assert_eq!(
                verify(Cursor::new(crc)).unwrap_err().kind(),
                srep::ErrorKind::ChecksumMismatch,
                "{layout:?}/{checksum:?} crc"
            );
        }
    }
}

#[test]
fn manual_v3_tail_locators_and_reserved_fields_are_strict() {
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        let checksum = Checksum::Xxh3;
        let archive = manual_literal_archive(layout, checksum, b"abc");
        let tail_start = archive.len() - srep::format_v3::tail_len(checksum);

        let mut reserved = archive.clone();
        reserved[tail_start + 9] = 1;
        assert_eq!(
            verify(Cursor::new(reserved)).unwrap_err().kind(),
            srep::ErrorKind::CorruptRecord,
            "{layout:?} tail flags"
        );

        let mut index_offset = archive.clone();
        index_offset[tail_start + 20..tail_start + 28]
            .copy_from_slice(&(if layout.is_index() { 0u64 } else { 1u64 }).to_le_bytes());
        assert_eq!(
            verify(Cursor::new(index_offset)).unwrap_err().kind(),
            srep::ErrorKind::CorruptIndex,
            "{layout:?} index offset"
        );

        let mut index_length = archive;
        index_length[tail_start + 28..tail_start + 36]
            .copy_from_slice(&(if layout.is_index() { 0u64 } else { 1u64 }).to_le_bytes());
        assert_eq!(
            verify(Cursor::new(index_length)).unwrap_err().kind(),
            srep::ErrorKind::CorruptIndex,
            "{layout:?} index length"
        );
    }
}

#[test]
fn index_scaling_probe_reports_reader_timings_for_4095_8191_and_16383_matches() {
    for length in [128 * 1024usize, 256 * 1024, 512 * 1024] {
        let archive = manual_index_scaling_archive(Checksum::Xxh3, length);
        let started = std::time::Instant::now();
        verify(Cursor::new(&archive)).unwrap();
        println!(
            "index-scaling length={} matches={} archive_bytes={} elapsed_ms={}",
            length,
            length / 32 - 1,
            archive.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
}

#[test]
fn v3_wire_lengths_and_crc_known_vector_match_the_normative_examples() {
    assert_eq!(
        srep::format_v3::crc32c_le(b"123456789"),
        [0x83, 0x92, 0x06, 0xe3]
    );
    for checksum in [Checksum::Xxh3, Checksum::Blake3] {
        assert_eq!(
            manual_literal_archive(Layout::Index, checksum, b"").len(),
            if checksum == Checksum::Xxh3 { 157 } else { 189 }
        );
        assert_eq!(
            manual_literal_archive(Layout::Future, checksum, b"").len(),
            if checksum == Checksum::Xxh3 { 156 } else { 188 }
        );
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            let archive = manual_literal_archive(layout, checksum, b"abc");
            assert_eq!(inspect(Cursor::new(&archive)).unwrap().original_size, 3);
        }
    }
}

#[test]
fn v3_distance_cap_is_inclusive_and_rep_distance_does_not_cap_base_matches() {
    let mut input = vec![0u8; 5000];
    for (index, byte) in input.iter_mut().enumerate() {
        *byte = (index as u32).wrapping_mul(13) as u8;
    }
    let src = 0u64;
    let dst = 2048u64;
    let len = 512u64;
    for offset in 0..len as usize {
        input[dst as usize + offset] = input[offset];
    }
    let candidate = MatchCandidate {
        src,
        dst,
        len,
        insertion_ordinal: 0,
    };

    let mut config = CompressionConfig::for_method(Method::M3FixedDigest);
    config.layout = Layout::Index;
    config.block_size = 1024;
    config.min_match = 32;
    config.seed_size = Some(32);
    config.max_distance = Some(dst);
    config.rep_overlay = Some(srep::RepConfig {
        distance: 64,
        min_match: 32,
    });
    config.resources = ResourceConfig {
        temp_dir: std::env::temp_dir().join("srep-v3-distance-cap"),
        ..ResourceConfig::default()
    };
    let mut archive = Vec::new();
    srep::compress_with_candidates(Cursor::new(&input), &mut archive, &config, [candidate])
        .unwrap();
    let inspected = inspect_matches(Cursor::new(&archive)).unwrap();
    assert_eq!(inspected.as_slice()[0].src, src);
    assert_eq!(inspected.as_slice()[0].dst, dst);
    assert_eq!(inspected.as_slice()[0].len, len);

    let mut rejected = config.clone();
    rejected.max_distance = Some(dst - 1);
    let error =
        srep::compress_with_candidates(Cursor::new(&input), Vec::new(), &rejected, [candidate])
            .unwrap_err();
    assert_eq!(error.kind(), srep::ErrorKind::InvalidMatch);
}

#[test]
fn future_reader_installs_registers_before_carried_delivery_and_cross_block_sources() {
    let mut input = (0..3300u32)
        .map(|value| value.wrapping_mul(37) as u8)
        .collect::<Vec<_>>();
    let origins = [(0usize, 1000usize, 1100usize), (1000, 2100, 64usize)];
    for &(src, dst, len) in &origins {
        for offset in 0..len {
            input[dst + offset] = input[src + (offset % (dst - src))];
        }
    }
    let candidates = origins
        .into_iter()
        .enumerate()
        .map(|(ordinal, (src, dst, len))| MatchCandidate {
            src: src as u64,
            dst: dst as u64,
            len: len as u64,
            insertion_ordinal: ordinal as u64,
        });
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = Layout::Future;
    config.block_size = 1024;
    config.min_match = 32;
    config.target_chunk = Some(4096);
    config.resources = ResourceConfig {
        temp_dir: std::env::temp_dir().join("srep-v3-future-carry"),
        ..ResourceConfig::default()
    };
    let mut archive = Vec::new();
    srep::compress_with_candidates(Cursor::new(&input), &mut archive, &config, candidates).unwrap();
    let matches = inspect_matches(Cursor::new(&archive)).unwrap();
    assert_eq!(matches.as_slice().len(), 2);
    assert_eq!(matches.as_slice()[0].src, 0);
    assert_eq!(matches.as_slice()[1].src, 1000);
    let mut output = Vec::new();
    decompress(Cursor::new(&archive), &mut output).unwrap();
    assert_eq!(output, input);
}

#[test]
fn manual_v3_uleb_encodings_are_shortest_and_layout_specific() {
    let mut index = manual_literal_archive(Layout::Index, Checksum::Xxh3, b"abc");
    index[87] = 0x80;
    assert_eq!(
        verify(Cursor::new(index)).unwrap_err().kind(),
        srep::ErrorKind::CorruptIndex
    );

    let mut future = manual_literal_archive(Layout::Future, Checksum::Xxh3, b"abc");
    future[80] = 0x80;
    assert_eq!(
        verify(Cursor::new(future)).unwrap_err().kind(),
        srep::ErrorKind::CorruptRecord
    );

    let mut io = manual_literal_archive(Layout::Io, Checksum::Xxh3, b"abc");
    io[81] = 0x80;
    assert_eq!(
        verify(Cursor::new(io)).unwrap_err().kind(),
        srep::ErrorKind::CorruptRecord
    );
}

#[test]
fn manual_v3_uleb_overflow_is_rejected_for_index_future_and_io() {
    assert_eq!(
        srep::format_v3::decode_uleb128_index(&[0xff; 9])
            .unwrap_err()
            .kind(),
        srep::ErrorKind::CorruptIndex
    );

    let mut future = manual_literal_archive(Layout::Future, Checksum::Xxh3, b"abc");
    future[80..89].fill(0xff);
    assert_eq!(
        verify(Cursor::new(future)).unwrap_err().kind(),
        srep::ErrorKind::CorruptRecord
    );

    let mut io = manual_literal_archive(Layout::Io, Checksum::Xxh3, b"abc");
    io[81..90].fill(0xff);
    assert_eq!(
        verify(Cursor::new(io)).unwrap_err().kind(),
        srep::ErrorKind::CorruptRecord
    );
}

#[test]
fn future_reader_spills_large_periods_to_the_shared_temp_budget() {
    let mut input = vec![0u8; 10_000];
    for (index, byte) in input.iter_mut().enumerate() {
        *byte = (index as u32).wrapping_mul(29) as u8;
    }
    let src = 0u64;
    let dst = 5000u64;
    let len = 4096u64;
    for offset in 0..len as usize {
        input[dst as usize + offset] = input[offset];
    }
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = Layout::Future;
    config.block_size = 1024;
    config.min_match = 32;
    config.target_chunk = Some(4096);
    config.resources = ResourceConfig {
        memory: 1024,
        temp_limit: 2 * 1024 * 1024,
        temp_dir: std::env::temp_dir().join("srep-v3-future-period-spill"),
        ..ResourceConfig::default()
    };
    let candidate = MatchCandidate {
        src,
        dst,
        len,
        insertion_ordinal: 0,
    };
    let mut archive = Vec::new();
    srep::compress_with_candidates(Cursor::new(&input), &mut archive, &config, [candidate])
        .unwrap();
    let mut output = Vec::new();
    let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
    srep::decompress_with_context(
        Cursor::new(&archive),
        &mut output,
        &config.resources,
        &context,
    )
    .unwrap();
    assert_eq!(output, input);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
}
