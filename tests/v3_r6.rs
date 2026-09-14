use std::fs;
use std::io::{Cursor, Read, Write};
use std::process::Command;

use srep::{
    Checksum, CompressionConfig, ErrorKind, Layout, MatchCandidate, Method, ResourceConfig,
};

fn resources(name: &str, memory: u64, temp_limit: u64) -> ResourceConfig {
    ResourceConfig {
        memory,
        temp_limit,
        temp_dir: std::env::temp_dir().join(format!("srep-v3-r6-{name}-{}", std::process::id())),
        ..ResourceConfig::default()
    }
}

fn reseal_encoded_digest(archive: &mut [u8], checksum: Checksum) {
    let width = checksum.width();
    let tail_start = archive.len() - srep::format_v3::tail_len(checksum);
    let mut digest = srep::format_v3::GlobalDigest::encoded(checksum);
    digest.update(&archive[..tail_start]);
    digest.update(&archive[tail_start..tail_start + srep::format_v3::TAIL_PREFIX_LEN + width]);
    archive[tail_start + srep::format_v3::TAIL_PREFIX_LEN + width..]
        .copy_from_slice(&digest.finalize());
}

fn encode_uleb(value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    srep::format_v3::encode_uleb128_into(value, &mut bytes).unwrap();
    bytes
}

fn manual_archive(
    layout: Layout,
    checksum: Checksum,
    plaintext: &[u8],
    body: &[u8],
    index: &[u8],
) -> Vec<u8> {
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = layout;
    config.checksum = checksum;
    config.block_size = 1024;
    config.min_match = 32;
    config.resources = resources("manual-writer", 1024 * 1024, 1024 * 1024);
    let header =
        srep::format_v3::ArchiveHeader::from_config(&config, plaintext.len() as u64).unwrap();
    let header_bytes = srep::format_v3::encode_archive_header(&header).unwrap();
    let body_end = srep::format_v3::HEADER_LEN as u64 + body.len() as u64;
    let index_offset = if layout.is_index() { body_end } else { 0 };
    let tail_start = body_end + index.len() as u64;
    let archive_length = tail_start + srep::format_v3::tail_len(checksum) as u64;
    let mut tail = srep::format_v3::encode_archive_tail(
        &srep::format_v3::ArchiveTail {
            archive_length,
            index_offset,
            index_total_length: index.len() as u64,
            body_end,
            plaintext_digest: srep::format_v3::plaintext_digest(checksum, plaintext),
            encoded_digest: vec![0; checksum.width()],
        },
        checksum,
    )
    .unwrap();
    let mut encoded = srep::format_v3::GlobalDigest::encoded(checksum);
    encoded.update(&header_bytes);
    encoded.update(body);
    encoded.update(index);
    encoded.update(&tail[..srep::format_v3::TAIL_PREFIX_LEN + checksum.width()]);
    tail[srep::format_v3::TAIL_PREFIX_LEN + checksum.width()..]
        .copy_from_slice(&encoded.finalize());
    let mut archive = header_bytes.to_vec();
    archive.extend_from_slice(body);
    archive.extend_from_slice(index);
    archive.extend_from_slice(&tail);
    archive
}

fn future_body(plaintext: &[u8], origins: &[(u64, u64, u64)]) -> Vec<u8> {
    let mut body = Vec::new();
    for block in 0..plaintext.len().div_ceil(1024) {
        let start = (block * 1024) as u64;
        let end = ((block + 1) * 1024).min(plaintext.len()) as u64;
        let block_origins = origins
            .iter()
            .copied()
            .filter(|(src, _, _)| *src / 1024 == block as u64)
            .collect::<Vec<_>>();
        body.extend_from_slice(&encode_uleb(block_origins.len() as u64));
        for (src, dst, len) in block_origins {
            body.extend_from_slice(&encode_uleb(src - start));
            body.extend_from_slice(&encode_uleb(dst - src));
            body.extend_from_slice(&encode_uleb(len));
        }
        for position in start..end {
            if !origins
                .iter()
                .any(|(_, dst, len)| *dst <= position && position < *dst + *len)
            {
                body.push(plaintext[position as usize]);
            }
        }
        body.extend_from_slice(&srep::format_v3::crc32c_le(
            &plaintext[start as usize..end as usize],
        ));
    }
    body
}

fn future_archive_from_origins(
    plaintext: &[u8],
    origins: &[(u64, u64, u64)],
    checksum: Checksum,
) -> Vec<u8> {
    manual_archive(
        Layout::Future,
        checksum,
        plaintext,
        &future_body(plaintext, origins),
        &[],
    )
}

fn explicit_candidate_archive(
    method: Method,
    layout: Layout,
    checksum: Checksum,
    input: &[u8],
    candidate: MatchCandidate,
    max_distance: Option<u64>,
    rep_overlay: Option<srep::RepConfig>,
) -> (Vec<u8>, CompressionConfig) {
    let mut config = CompressionConfig::for_method(method);
    config.layout = layout;
    config.checksum = checksum;
    config.block_size = 1024;
    config.min_match = 32;
    config.max_distance = max_distance;
    config.rep_overlay = rep_overlay;
    config.resources = resources("explicit-writer", 1024 * 1024, 1024 * 1024);
    let mut archive = Vec::new();
    srep::compress_with_candidates(Cursor::new(input), &mut archive, &config, [candidate]).unwrap();
    (archive, config)
}

struct FailingReader {
    inner: Cursor<Vec<u8>>,
    remaining: usize,
}

impl Read for FailingReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::other("injected reader failure"));
        }
        let amount = self.remaining.min(output.len());
        let read = self.inner.read(&mut output[..amount])?;
        self.remaining -= read;
        Ok(read)
    }
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("injected writer failure"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn assert_temp_dir_empty(path: &std::path::Path) {
    assert!(path.exists());
    assert_eq!(
        fs::read_dir(path).unwrap().count(),
        0,
        "owned temp file leaked"
    );
    fs::remove_dir_all(path).unwrap();
}

struct OneByteReader {
    inner: Cursor<Vec<u8>>,
}

impl Read for OneByteReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        self.inner.read(&mut output[..1])
    }
}

#[derive(Default)]
struct RecordingWriter {
    calls: usize,
    bytes: usize,
}

impl Write for RecordingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.calls += 1;
        self.bytes += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn input_with_origins(length: usize, origins: &[(u64, u64, u64)]) -> Vec<u8> {
    let mut input = vec![0u8; length];
    for (index, byte) in input.iter_mut().enumerate() {
        *byte = (index as u32).wrapping_mul(31) as u8;
    }
    for &(src, dst, len) in origins {
        for offset in 0..len as usize {
            input[dst as usize + offset] =
                input[src as usize + (offset as u64 % (dst - src)) as usize];
        }
    }
    input
}

fn all_x_with_origins(length: usize, origins: &[(u64, u64, u64)]) -> Vec<u8> {
    let mut input = vec![b'X'; length];
    for &(src, dst, len) in origins {
        for offset in 0..len as usize {
            input[dst as usize + offset] =
                input[src as usize + (offset as u64 % (dst - src)) as usize];
        }
    }
    input
}

fn future_archive(memory: u64, temp_limit: u64, name: &str) -> (Vec<u8>, ResourceConfig) {
    let mut input = vec![0u8; 4096];
    for (index, byte) in input.iter_mut().enumerate() {
        *byte = (index as u32).wrapping_mul(17) as u8;
    }
    let candidate = MatchCandidate {
        src: 0,
        dst: 2048,
        len: 512,
        insertion_ordinal: 0,
    };
    for offset in 0..candidate.len as usize {
        input[candidate.dst as usize + offset] = input[offset];
    }
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = Layout::Future;
    config.block_size = 1024;
    config.resources = resources("writer", 1024 * 1024, 64 * 1024);
    let mut archive = Vec::new();
    srep::compress_with_candidates(Cursor::new(input), &mut archive, &config, [candidate]).unwrap();
    (archive, resources(name, memory, temp_limit))
}

#[test]
fn future_spill_reserves_heap_and_pending_buffer_before_period_choice() {
    let (archive, resource_config) = future_archive(1024, 64 * 1024, "threshold");
    let context = srep::ResourceContext::with_resources(&resource_config).unwrap();
    let mut output = Vec::new();
    srep::decompress_with_context(
        Cursor::new(&archive),
        &mut output,
        &resource_config,
        &context,
    )
    .unwrap();
    assert_eq!(output.len(), 4096);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert!(context.memory.high_water() >= 512);
}

#[test]
fn future_spill_reports_bookkeeping_failure_without_leaking_resources() {
    let (archive, resource_config) = future_archive(512, 64 * 1024, "bookkeeping-failure");
    let context = srep::ResourceContext::with_resources(&resource_config).unwrap();
    let error = srep::decompress_with_context(
        Cursor::new(&archive),
        Vec::new(),
        &resource_config,
        &context,
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
}

#[test]
fn malformed_index_tail_is_rejected_without_large_block_loop() {
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = Layout::Index;
    config.checksum = Checksum::Xxh3;
    config.block_size = 1024;
    config.resources = resources("max-wire-tail", 64 * 1024, 64 * 1024);
    config.resources.output_limit = (1u64 << 63) - 1;
    let mut archive = Vec::new();
    srep::compress_with_candidates(
        Cursor::new(vec![0x5a; 1024]),
        &mut archive,
        &config,
        std::iter::empty(),
    )
    .unwrap();
    archive.truncate(archive.len() - srep::format_v3::tail_len(Checksum::Xxh3));
    let mut header = archive[..srep::format_v3::HEADER_LEN].to_vec();
    header[72..80].copy_from_slice(&((1u64 << 63) - 1).to_le_bytes());
    archive[..srep::format_v3::HEADER_LEN].copy_from_slice(&header);
    let started = std::time::Instant::now();
    let error = srep::verify_with_resources(&archive[..], &config.resources).unwrap_err();
    let elapsed = started.elapsed();
    eprintln!(
        "MAX_WIRE tiny-tail classification elapsed_ms={}",
        elapsed.as_secs_f64() * 1000.0
    );
    assert!(elapsed < std::time::Duration::from_secs(1));
    assert_eq!(error.kind(), ErrorKind::CorruptRecord);
}

#[test]
fn huge_future_register_count_reads_first_register_before_allocating_count_storage() {
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = Layout::Future;
    config.block_size = 1024;
    config.resources = resources("huge-future-count", 1024 * 1024, 64 * 1024);
    let header = srep::format_v3::ArchiveHeader::from_config(&config, 32_000_000).unwrap();
    let header_bytes = srep::format_v3::encode_archive_header(&header).unwrap();
    let mut archive = header_bytes.to_vec();
    archive.extend_from_slice(&encode_uleb(1_000_000));
    let started = std::time::Instant::now();
    let error = srep::verify_with_resources(&archive[..], &config.resources).unwrap_err();
    let elapsed = started.elapsed();
    eprintln!(
        "huge Future count natural EOF elapsed_ms={}",
        elapsed.as_secs_f64() * 1000.0
    );
    assert!(elapsed < std::time::Duration::from_secs(1));
    assert_eq!(error.kind(), ErrorKind::TruncatedArchive);
}

#[test]
fn tail_prefix_and_full_tail_errors_are_distinct_for_all_layouts_and_hashes() {
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
            config.layout = layout;
            config.checksum = checksum;
            config.block_size = 1024;
            config.resources = resources("tail-matrix", 128 * 1024, 128 * 1024);
            let mut archive = Vec::new();
            srep::compress_with_candidates(
                Cursor::new(vec![0x41; 1025]),
                &mut archive,
                &config,
                std::iter::empty(),
            )
            .unwrap();
            let tail_len = srep::format_v3::tail_len(checksum);
            let partial = archive[..archive.len() - tail_len + 3].to_vec();
            let partial_error = srep::verify_with_resources(&partial[..], &config.resources)
                .unwrap_err()
                .kind();
            let expected_partial = if layout == Layout::Index {
                ErrorKind::CorruptRecord
            } else {
                ErrorKind::TruncatedArchive
            };
            assert_eq!(
                partial_error, expected_partial,
                "partial {layout:?}/{checksum:?}"
            );
            let mut malformed = archive;
            let tail_start = malformed.len() - tail_len;
            malformed[tail_start] ^= 1;
            assert_eq!(
                srep::verify_with_resources(&malformed[..], &config.resources)
                    .unwrap_err()
                    .kind(),
                ErrorKind::CorruptRecord,
                "malformed {layout:?}/{checksum:?}"
            );
        }
    }
}

#[test]
fn decoder_enforces_inclusive_global_distance_cap_for_all_sequential_layouts_and_hashes() {
    let input = input_with_origins(4096, &[(0, 1024, 64)]);
    let candidate = MatchCandidate {
        src: 0,
        dst: 1024,
        len: 64,
        insertion_ordinal: 0,
    };
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
            config.layout = layout;
            config.checksum = checksum;
            config.block_size = 1024;
            config.max_distance = Some(1024);
            config.resources = resources("distance-sequential", 256 * 1024, 256 * 1024);
            let mut archive = Vec::new();
            srep::compress_with_candidates(Cursor::new(&input), &mut archive, &config, [candidate])
                .unwrap();
            let mut restored = Vec::new();
            srep::decompress_with_resources(&archive[..], &mut restored, &config.resources)
                .unwrap();
            assert_eq!(restored, input);

            let tail_start = archive.len() - srep::format_v3::tail_len(checksum);
            archive[48..56].copy_from_slice(&1023u64.to_le_bytes());
            reseal_encoded_digest(&mut archive, checksum);
            let error = srep::verify_with_resources(&archive[..], &config.resources).unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::InvalidMatch,
                "{layout:?}/{checksum:?}"
            );
            assert_eq!(
                tail_start,
                archive.len() - srep::format_v3::tail_len(checksum)
            );
        }
    }
}

#[test]
fn decoder_accepts_base_matches_farther_than_rep_overlay_distance() {
    let input = all_x_with_origins(4096, &[(0, 1024, 64)]);
    let candidate = MatchCandidate {
        src: 0,
        dst: 1024,
        len: 64,
        insertion_ordinal: 0,
    };
    for method in [
        Method::M3FixedDigest,
        Method::M4Reread,
        Method::M5Exhaustive,
    ] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            for layout in [Layout::Index, Layout::Future, Layout::Io] {
                let (archive, config) = explicit_candidate_archive(
                    method,
                    layout,
                    checksum,
                    &input,
                    candidate,
                    Some(1024),
                    Some(srep::RepConfig {
                        distance: 64,
                        min_match: 32,
                    }),
                );
                let inspected =
                    srep::inspect_matches_with_resources(&archive[..], &config.resources).unwrap();
                assert_eq!(inspected.as_slice()[0].src, 0);
                assert_eq!(inspected.as_slice()[0].dst, 1024);
                let mut output = Vec::new();
                srep::decompress_with_resources(&archive[..], &mut output, &config.resources)
                    .unwrap();
                assert_eq!(output, input);
            }
        }
    }
}

#[test]
fn future_budget_thresholds_are_monotonic_and_always_released() {
    for memory in [511, 512] {
        let (archive, resource_config) = future_archive(memory, 64 * 1024, "threshold-fail");
        let context = srep::ResourceContext::with_resources(&resource_config).unwrap();
        let error = srep::decompress_with_context(
            Cursor::new(&archive),
            Vec::new(),
            &resource_config,
            &context,
        )
        .unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::MemoryBudgetExceeded,
            "memory={memory}"
        );
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }
    let (archive, resource_config) = future_archive(1024, 64 * 1024, "threshold-success");
    let context = srep::ResourceContext::with_resources(&resource_config).unwrap();
    let mut output = Vec::new();
    srep::decompress_with_context(
        Cursor::new(&archive),
        &mut output,
        &resource_config,
        &context,
    )
    .unwrap();
    assert_eq!(output.len(), 4096);
    assert!(context.memory.high_water() <= resource_config.memory);
    assert!(context.temp.high_water() <= resource_config.temp_limit);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
}

#[test]
fn many_simultaneous_spilled_future_periods_are_accounted_and_released() {
    let origins = (0..8u64)
        .map(|index| (index * 1024, 16_384 + index * 4096, 4096))
        .collect::<Vec<_>>();
    let input = input_with_origins(16_384 + 8 * 4096 + 4096, &origins);
    let archive = future_archive_from_origins(&input, &origins, Checksum::Blake3);
    let low_resources = resources("many-spills", 32 * 1024, 2 * 1024 * 1024);
    let context = srep::ResourceContext::with_resources(&low_resources).unwrap();
    let inspected = srep::inspect_matches_with_resources(&archive[..], &low_resources).unwrap();
    assert_eq!(inspected.as_slice().len(), origins.len());
    assert!(
        inspected
            .as_slice()
            .iter()
            .zip(origins.iter())
            .all(|(actual, expected)| (actual.src, actual.dst, actual.len) == *expected)
    );
    let mut output = Vec::new();
    srep::decompress_with_context(Cursor::new(&archive), &mut output, &low_resources, &context)
        .unwrap_or_else(|error| {
            panic!(
                "{error}; memory current={} high={} temp current={} high={}",
                context.memory.current(),
                context.memory.high_water(),
                context.temp.current(),
                context.temp.high_water()
            )
        });
    assert_eq!(output, input);
    assert!(context.memory.high_water() <= low_resources.memory);
    assert!(context.temp.high_water() > 8 * 4096);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert_temp_dir_empty(&low_resources.temp_dir);

    let high_resources = resources("many-spills-high", 128 * 1024, 2 * 1024 * 1024);
    let high_context = srep::ResourceContext::with_resources(&high_resources).unwrap();
    let mut high_output = Vec::new();
    srep::decompress_with_context(
        Cursor::new(&archive),
        &mut high_output,
        &high_resources,
        &high_context,
    )
    .unwrap();
    assert_eq!(high_output, output);
    assert_eq!(high_context.memory.current(), 0);
    assert_eq!(high_context.temp.current(), 0);
    assert_temp_dir_empty(&high_resources.temp_dir);
}

#[test]
fn reader_input_output_and_temp_failures_release_shared_ledgers() {
    let (archive, base_resources) = future_archive(64 * 1024, 64 * 1024, "io-failures");

    let read_resources = resources("read-failure", 64 * 1024, 64 * 1024);
    let read_context = srep::ResourceContext::with_resources(&read_resources).unwrap();
    let error = srep::decompress_with_context(
        FailingReader {
            inner: Cursor::new(archive.clone()),
            remaining: srep::format_v3::HEADER_LEN,
        },
        Vec::new(),
        &read_resources,
        &read_context,
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InputIo);
    assert_eq!(read_context.memory.current(), 0);
    assert_eq!(read_context.temp.current(), 0);
    assert_temp_dir_empty(&read_resources.temp_dir);

    let output_resources = resources("output-failure", 64 * 1024, 64 * 1024);
    let output_context = srep::ResourceContext::with_resources(&output_resources).unwrap();
    let error = srep::decompress_with_context(
        Cursor::new(archive.clone()),
        FailingWriter,
        &output_resources,
        &output_context,
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::OutputIo);
    assert_eq!(output_context.memory.current(), 0);
    assert_eq!(output_context.temp.current(), 0);
    assert_temp_dir_empty(&output_resources.temp_dir);

    let temp_resources = ResourceConfig {
        temp_limit: 1,
        temp_dir: std::env::temp_dir()
            .join(format!("srep-v3-r6-temp-failure-{}", std::process::id())),
        ..base_resources
    };
    let temp_context = srep::ResourceContext::with_resources(&temp_resources).unwrap();
    let error = srep::decompress_with_context(
        Cursor::new(archive),
        Vec::new(),
        &temp_resources,
        &temp_context,
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TempBudgetExceeded);
    assert_eq!(temp_context.memory.current(), 0);
    assert_eq!(temp_context.temp.current(), 0);
    assert_temp_dir_empty(&temp_resources.temp_dir);
}

#[test]
fn injected_input_failures_map_to_input_io_at_header_body_and_tail_boundaries() {
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
            config.layout = layout;
            config.checksum = checksum;
            config.block_size = 1024;
            config.resources = resources("injected-reader-writer", 1024 * 1024, 1024 * 1024);
            let mut archive = Vec::new();
            srep::compress_with_candidates(
                Cursor::new(vec![b'X'; 2048]),
                &mut archive,
                &config,
                std::iter::empty(),
            )
            .unwrap();
            let cut_points = [
                srep::format_v3::HEADER_LEN,
                srep::format_v3::HEADER_LEN + 1,
                archive.len() - srep::format_v3::tail_len(checksum) + 1,
            ];
            for (point, cut) in cut_points.into_iter().enumerate() {
                let reader_resources = resources(
                    &format!("injected-reader-{layout:?}-{checksum:?}-{point}"),
                    256 * 1024,
                    256 * 1024,
                );
                let context = srep::ResourceContext::with_resources(&reader_resources).unwrap();
                let error = srep::decompress_with_context(
                    FailingReader {
                        inner: Cursor::new(archive.clone()),
                        remaining: cut,
                    },
                    Vec::new(),
                    &reader_resources,
                    &context,
                )
                .unwrap_err();
                assert_eq!(
                    error.kind(),
                    ErrorKind::InputIo,
                    "{layout:?}/{checksum:?}/{point}"
                );
                assert_eq!(context.memory.current(), 0);
                assert_eq!(context.temp.current(), 0);
                assert_temp_dir_empty(&reader_resources.temp_dir);
            }
        }
    }
}

#[test]
fn malformed_v3_grammars_keep_record_and_match_error_kinds() {
    for checksum in [Checksum::Xxh3, Checksum::Blake3] {
        let literal = b"xyz";
        let zero_length = manual_archive(
            Layout::Io,
            checksum,
            literal,
            &[srep::format_v3::IO_LITERAL_TAG, 0],
            &[],
        );
        assert_eq!(
            srep::verify_with_resources(
                &zero_length[..],
                &resources("grammar-zero", 64 * 1024, 64 * 1024)
            )
            .unwrap_err()
            .kind(),
            ErrorKind::CorruptRecord,
            "zero length/{checksum:?}"
        );

        let adjacent = manual_archive(
            Layout::Io,
            checksum,
            literal,
            &[0, 1, b'x', 0, 1, b'y'],
            &[],
        );
        assert_eq!(
            srep::verify_with_resources(
                &adjacent[..],
                &resources("grammar-adjacent", 64 * 1024, 64 * 1024)
            )
            .unwrap_err()
            .kind(),
            ErrorKind::CorruptRecord,
            "adjacent literals/{checksum:?}"
        );

        let overrun = manual_archive(
            Layout::Io,
            checksum,
            &vec![b'x'; 1025],
            &[0, 0x81, 0x08],
            &[],
        );
        assert_eq!(
            srep::verify_with_resources(
                &overrun[..],
                &resources("grammar-overrun", 64 * 1024, 64 * 1024)
            )
            .unwrap_err()
            .kind(),
            ErrorKind::CorruptRecord,
            "literal overrun/{checksum:?}"
        );

        let outside = manual_archive(
            Layout::Future,
            checksum,
            &vec![b'x'; 1024],
            &[1, 0x80, 0x08, 64],
            &[],
        );
        assert_eq!(
            srep::verify_with_resources(
                &outside[..],
                &resources("grammar-source", 64 * 1024, 64 * 1024)
            )
            .unwrap_err()
            .kind(),
            ErrorKind::InvalidMatch,
            "source outside block/{checksum:?}"
        );

        let overlap = manual_archive(
            Layout::Future,
            checksum,
            &vec![b'x'; 2200],
            &[2, 0, 0xE8, 0x07, 0xE8, 0x07, 1, 0xE8, 0x07, 64],
            &[],
        );
        assert_eq!(
            srep::verify_with_resources(
                &overlap[..],
                &resources("grammar-overlap", 64 * 1024, 64 * 1024)
            )
            .unwrap_err()
            .kind(),
            ErrorKind::InvalidMatch,
            "overlap/{checksum:?}"
        );

        let overflow = manual_archive(Layout::Future, checksum, b"x", &[0xFF; 9], &[]);
        assert_eq!(
            srep::verify_with_resources(
                &overflow[..],
                &resources("grammar-uleb", 64 * 1024, 64 * 1024)
            )
            .unwrap_err()
            .kind(),
            ErrorKind::CorruptRecord,
            "ULEB overflow/{checksum:?}"
        );

        let zero_distance = manual_archive(
            Layout::Future,
            checksum,
            &vec![b'x'; 1024],
            &[1, 0, 0, 64],
            &[],
        );
        assert_eq!(
            srep::verify_with_resources(
                &zero_distance[..],
                &resources("grammar-zero-distance", 64 * 1024, 64 * 1024)
            )
            .unwrap_err()
            .kind(),
            ErrorKind::InvalidMatch,
            "zero distance/{checksum:?}"
        );
    }
}

#[test]
fn duplicate_future_registers_are_rejected_for_both_checksums_without_output_or_ledger_leaks() {
    let plaintext = input_with_origins(200, &[(0, 100, 32), (0, 100, 32)]);
    let mut runs = 0;
    for checksum in [Checksum::Xxh3, Checksum::Blake3] {
        let archive =
            future_archive_from_origins(&plaintext, &[(0, 100, 32), (0, 100, 32)], checksum);
        let config = resources(
            &format!("duplicate-future-{checksum:?}"),
            256 * 1024,
            256 * 1024,
        );
        let context = srep::ResourceContext::with_resources(&config).unwrap();
        let mut output = Vec::new();
        let error = srep::decompress_with_context(
            OneByteReader {
                inner: Cursor::new(archive),
            },
            &mut output,
            &config,
            &context,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidMatch, "{checksum:?}");
        assert!(output.is_empty());
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
        assert_temp_dir_empty(&config.temp_dir);
        runs += 1;
    }
    assert_eq!(runs, 2);
}

#[test]
fn nonseekable_tail_error_matrix_covers_empty_and_abc_for_all_layouts_and_checksums() {
    #[derive(Clone, Copy)]
    enum Mutation {
        ShortHeader,
        CompleteTailRemoval,
        PartialTailRemoval,
        BadTailMagic,
        BadTailVersion,
        TrailingByte,
    }

    let mutations = [
        Mutation::ShortHeader,
        Mutation::CompleteTailRemoval,
        Mutation::PartialTailRemoval,
        Mutation::BadTailMagic,
        Mutation::BadTailVersion,
        Mutation::TrailingByte,
    ];
    let mut cases = 0;
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            for plaintext in [b"".as_slice(), b"abc".as_slice()] {
                let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
                config.layout = layout;
                config.checksum = checksum;
                config.block_size = 1024;
                config.resources = resources(
                    &format!("tail-matrix-writer-{layout:?}-{checksum:?}"),
                    256 * 1024,
                    256 * 1024,
                );
                let mut original = Vec::new();
                srep::compress_with_candidates(
                    Cursor::new(plaintext),
                    &mut original,
                    &config,
                    std::iter::empty(),
                )
                .unwrap();
                for (mutation_index, mutation) in mutations.iter().copied().enumerate() {
                    let mut archive = original.clone();
                    let tail_len = srep::format_v3::tail_len(checksum);
                    match mutation {
                        Mutation::ShortHeader => archive.truncate(40),
                        Mutation::CompleteTailRemoval => archive.truncate(archive.len() - tail_len),
                        Mutation::PartialTailRemoval => archive.truncate(archive.len() - 1),
                        Mutation::BadTailMagic => {
                            let tail_start = archive.len() - tail_len;
                            archive[tail_start] ^= 1;
                        }
                        Mutation::BadTailVersion => {
                            let tail_start = archive.len() - tail_len;
                            archive[tail_start + 8] ^= 1;
                        }
                        Mutation::TrailingByte => archive.push(0),
                    }
                    let reader_resources = resources(
                        &format!(
                            "tail-matrix-reader-{layout:?}-{checksum:?}-{plaintext:?}-{mutation_index}"
                        ),
                        256 * 1024,
                        256 * 1024,
                    );
                    fs::create_dir_all(&reader_resources.temp_dir).unwrap();
                    let context = srep::ResourceContext::with_resources(&reader_resources).unwrap();
                    let physical_len = archive.len();
                    let mut output = Vec::new();
                    let error = srep::decompress_with_context(
                        OneByteReader {
                            inner: Cursor::new(archive),
                        },
                        &mut output,
                        &reader_resources,
                        &context,
                    )
                    .unwrap_err();
                    let expected = match mutation {
                        Mutation::ShortHeader => ErrorKind::TruncatedArchive,
                        Mutation::CompleteTailRemoval if layout == Layout::Index => {
                            if physical_len < tail_len {
                                ErrorKind::TruncatedArchive
                            } else {
                                ErrorKind::CorruptRecord
                            }
                        }
                        Mutation::CompleteTailRemoval => ErrorKind::TruncatedArchive,
                        Mutation::PartialTailRemoval if layout == Layout::Index => {
                            ErrorKind::CorruptRecord
                        }
                        Mutation::PartialTailRemoval => ErrorKind::TruncatedArchive,
                        Mutation::BadTailMagic
                        | Mutation::BadTailVersion
                        | Mutation::TrailingByte => ErrorKind::CorruptRecord,
                    };
                    assert_eq!(
                        error.kind(),
                        expected,
                        "{layout:?}/{checksum:?}/{plaintext:?}/mutation={mutation_index}"
                    );
                    assert!(output.is_empty());
                    assert_eq!(context.memory.current(), 0);
                    assert_eq!(context.temp.current(), 0);
                    assert_temp_dir_empty(&reader_resources.temp_dir);
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 3 * 2 * 2 * 6);
}

#[test]
fn tail_removal_matrix_distinguishes_literal_and_reference_index_bodies() {
    let literal = vec![0x42; 1025];
    let reference = input_with_origins(3300, &[(0, 1000, 2200)]);
    let candidate = MatchCandidate {
        src: 0,
        dst: 1000,
        len: 2200,
        insertion_ordinal: 0,
    };
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let mut literal_config = CompressionConfig::for_method(Method::M1RollingCdc);
            literal_config.layout = layout;
            literal_config.checksum = checksum;
            literal_config.block_size = 1024;
            literal_config.resources = resources("tail-removal-literal", 256 * 1024, 256 * 1024);
            let mut literal_archive = Vec::new();
            srep::compress_with_candidates(
                Cursor::new(&literal),
                &mut literal_archive,
                &literal_config,
                std::iter::empty(),
            )
            .unwrap();
            literal_archive.truncate(literal_archive.len() - srep::format_v3::tail_len(checksum));
            assert_eq!(
                srep::verify_with_resources(&literal_archive[..], &literal_config.resources)
                    .unwrap_err()
                    .kind(),
                if layout == Layout::Index {
                    ErrorKind::CorruptRecord
                } else {
                    ErrorKind::TruncatedArchive
                },
                "literal removal {layout:?}/{checksum:?}"
            );

            let mut reference_config = literal_config.clone();
            reference_config.resources =
                resources("tail-removal-reference", 256 * 1024, 256 * 1024);
            let mut reference_archive = Vec::new();
            srep::compress_with_candidates(
                Cursor::new(&reference),
                &mut reference_archive,
                &reference_config,
                [candidate],
            )
            .unwrap();
            reference_archive
                .truncate(reference_archive.len() - srep::format_v3::tail_len(checksum));
            let expected = if layout == Layout::Index {
                ErrorKind::CorruptRecord
            } else {
                ErrorKind::TruncatedArchive
            };
            assert_eq!(
                srep::verify_with_resources(&reference_archive[..], &reference_config.resources)
                    .unwrap_err()
                    .kind(),
                expected,
                "reference removal {layout:?}/{checksum:?}"
            );
        }
    }
}

#[test]
fn future_installs_new_source_block_register_before_carried_delivery() {
    let origins = [(0, 1000, 1000), (1024, 2048, 64)];
    let input = all_x_with_origins(3300, &origins);
    for checksum in [Checksum::Xxh3, Checksum::Blake3] {
        let archive = future_archive_from_origins(&input, &origins, checksum);
        let resources = resources("future-source-block", 128 * 1024, 512 * 1024);
        let inspected = srep::inspect_matches_with_resources(&archive[..], &resources).unwrap();
        assert_eq!(inspected.as_slice().len(), 2);
        assert_eq!(
            inspected
                .as_slice()
                .iter()
                .map(|item| (item.src, item.dst, item.len))
                .collect::<Vec<_>>(),
            vec![(0, 1000, 1000), (1024, 2048, 64)]
        );
        let mut output = Vec::new();
        srep::decompress_with_resources(&archive[..], &mut output, &resources).unwrap();
        assert_eq!(output, input);
    }
}

#[test]
fn malformed_literal_match_eof_and_count_grammars_are_rejected() {
    for checksum in [Checksum::Xxh3, Checksum::Blake3] {
        let plaintext = vec![b'X'; 1025];
        let body = vec![srep::format_v3::IO_LITERAL_TAG, 0x81, 0x08];
        let literal_overrun = manual_archive(Layout::Io, checksum, &plaintext, &body, &[]);
        assert_eq!(
            srep::verify_with_resources(
                &literal_overrun[..],
                &resources("grammar-literal-overrun", 64 * 1024, 64 * 1024)
            )
            .unwrap_err()
            .kind(),
            ErrorKind::CorruptRecord,
            "literal overrun/{checksum:?}"
        );

        let mut eof_body = vec![srep::format_v3::IO_LITERAL_TAG, 0x80, 0x08];
        eof_body.extend_from_slice(&plaintext[..1024]);
        eof_body.extend_from_slice(&srep::format_v3::crc32c_le(&plaintext[..1024]));
        eof_body.extend_from_slice(&[srep::format_v3::IO_MATCH_TAG, 1, 64]);
        eof_body.extend_from_slice(&srep::format_v3::crc32c_le(b"X"));
        let match_at_global_eof = manual_archive(Layout::Io, checksum, &plaintext, &eof_body, &[]);
        assert_eq!(
            srep::verify_with_resources(
                &match_at_global_eof[..],
                &resources("grammar-match-eof", 64 * 1024, 64 * 1024)
            )
            .unwrap_err()
            .kind(),
            ErrorKind::InvalidMatch,
            "match at global EOF/{checksum:?}"
        );

        let count_overlarge =
            manual_archive(Layout::Future, checksum, &vec![b'X'; 1024], &[40], &[]);
        assert_eq!(
            srep::verify_with_resources(
                &count_overlarge[..],
                &resources("grammar-count", 64 * 1024, 64 * 1024)
            )
            .unwrap_err()
            .kind(),
            ErrorKind::CorruptRecord,
            "overlarge count/{checksum:?}"
        );
    }
}

#[test]
fn late_encoded_digest_failure_is_atomic_for_library_and_cli_across_layouts_and_checksums() {
    let mut pairs = 0;
    for layout in [Layout::Index, Layout::Future, Layout::Io] {
        for checksum in [Checksum::Xxh3, Checksum::Blake3] {
            let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
            config.layout = layout;
            config.checksum = checksum;
            config.block_size = 1024;
            config.resources = resources(
                &format!("late-digest-library-{layout:?}-{checksum:?}"),
                256 * 1024,
                256 * 1024,
            );
            let mut archive = Vec::new();
            srep::compress_with_candidates(
                Cursor::new(b"abc"),
                &mut archive,
                &config,
                std::iter::empty(),
            )
            .unwrap();
            *archive.last_mut().unwrap() ^= 1;

            let context = srep::ResourceContext::with_resources(&config.resources).unwrap();
            let mut output = RecordingWriter::default();
            let error = srep::decompress_with_context(
                Cursor::new(&archive),
                &mut output,
                &config.resources,
                &context,
            )
            .unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::ChecksumMismatch,
                "{layout:?}/{checksum:?}"
            );
            assert_eq!(output.calls, 0, "{layout:?}/{checksum:?}");
            assert_eq!(output.bytes, 0, "{layout:?}/{checksum:?}");
            assert_eq!(context.memory.current(), 0);
            assert_eq!(context.temp.current(), 0);
            assert_temp_dir_empty(&config.resources.temp_dir);

            let root = std::env::temp_dir().join(format!(
                "srep-v3-r6-late-cli-{layout:?}-{checksum:?}-{}",
                std::process::id()
            ));
            let temp_dir = root.join("reader-temp");
            fs::create_dir_all(&temp_dir).unwrap();
            let archive_path = root.join("bad.srep");
            let output_path = root.join("out.bin");
            fs::write(&archive_path, archive).unwrap();
            fs::write(&output_path, b"must survive").unwrap();
            let result = Command::new(env!("CARGO_BIN_EXE_srep"))
                .args([
                    "decompress",
                    "--force",
                    "--temp-dir",
                    temp_dir.to_str().unwrap(),
                    archive_path.to_str().unwrap(),
                    output_path.to_str().unwrap(),
                ])
                .output()
                .unwrap();
            if result.status.success()
                || result.status.code() != Some(ErrorKind::ChecksumMismatch.code())
            {
                panic!(
                    "late digest CLI unexpectedly succeeded or used wrong exit code for {layout:?}/{checksum:?}: stdout={:?} stderr={:?}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            assert_eq!(fs::read(&output_path).unwrap(), b"must survive");
            assert_eq!(fs::read_dir(&temp_dir).unwrap().count(), 0);
            assert_eq!(fs::read_dir(&root).unwrap().count(), 3);
            fs::remove_dir_all(root).unwrap();
            pairs += 1;
        }
    }
    assert_eq!(pairs, 3 * 2);
}
