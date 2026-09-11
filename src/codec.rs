use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tempfile::{Builder, NamedTempFile};

use crate::checksum::{
    archive_digest_start, block_digest_start, record_digest_start, verify_bytes,
};
use crate::config::{Checksum, CompressionConfig, Layout, Method, ResourceConfig};
use crate::dispatch::{ArchiveKind, read_and_classify};
use crate::error::{Error, Result};
use crate::format::{
    self, ArchiveHeader, BlockDirectoryEntry, DATA_BLOCK_HEADER_LEN, DataBlockHeader, FRAME_LEN,
    HEADER_LEN, RECORD_ARCHIVE_SUMMARY, RECORD_BLOCK_DIRECTORY, RECORD_DATA_BLOCK,
    RECORD_INDEX_SECTION, RECORD_LAYOUT_METADATA, RECORD_METHOD_PARAMETERS, TRAILER_LEN, Trailer,
    parse_archive_header, parse_archive_summary, parse_frame, parse_layout_metadata,
    parse_method_parameters, parse_trailer, read_digest, read_record_frame, record_total_len,
    verify_ordinary_checksum,
};
use crate::format::{IO_LITERAL_HEADER_LEN, LITERAL_RUN_HEADER_LEN};
use crate::resource::{Reservation, ResourceContext};

const IO_BUFFER_SIZE: usize = 512;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompressionStats {
    pub original_size: u64,
    pub archive_size: u64,
    pub payload_size: u64,
    pub block_count: u64,
    pub compressed_blocks: u64,
    pub reference_count: u64,
    pub semantic_match_count: u64,
    pub covered_bytes: u64,
    pub literal_bytes: u64,
    pub method: Option<Method>,
    pub layout: Option<Layout>,
    pub checksum: Option<Checksum>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchiveInfo {
    pub version: u8,
    pub method: Option<Method>,
    pub layout: Option<Layout>,
    pub checksum: Option<Checksum>,
    pub legacy_layout: Option<String>,
    pub legacy_checksum: Option<String>,
    pub legacy_base_len: Option<u64>,
    pub block_size: Option<u64>,
    pub min_match: Option<u64>,
    pub original_size: u64,
    pub payload_size: u64,
    pub block_count: u64,
    pub semantic_match_count: u64,
    pub covered_bytes: u64,
    pub literal_bytes: u64,
}

pub fn compress<R: Read, W: Write>(
    input: R,
    output: W,
    config: &CompressionConfig,
) -> Result<CompressionStats> {
    config.validate()?;
    let context = ResourceContext::with_resources(&config.resources)?;
    compress_with_context(input, output, config, &context)
}

pub fn compress_with_candidates<
    R: Read,
    W: Write,
    I: IntoIterator<Item = crate::match_ir::MatchCandidate>,
>(
    input: R,
    output: W,
    config: &CompressionConfig,
    candidates: I,
) -> Result<CompressionStats> {
    config.validate()?;
    let context = ResourceContext::with_resources(&config.resources)?;
    compress_with_candidates_with_context(input, output, config, candidates, &context)
}

pub fn compress_with_candidates_with_context<
    R: Read,
    W: Write,
    I: IntoIterator<Item = crate::match_ir::MatchCandidate>,
>(
    input: R,
    output: W,
    config: &CompressionConfig,
    candidates: I,
    context: &ResourceContext,
) -> Result<CompressionStats> {
    crate::reference::compress_with_candidates(input, output, config, candidates, context)
}

pub fn compress_with_context<R: Read, W: Write>(
    input: R,
    output: W,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<CompressionStats> {
    config.validate()?;
    let spool = spool_input(input, &config.resources, context)?;
    let candidates = match config.method {
        Method::M0Rep => {
            crate::match_finder::m0::find_matches_m0_spooled_compact(&spool, config, context)?
        }
        Method::M1RollingCdc => crate::match_finder::cdc::find_matches_spooled(
            &spool,
            config,
            context,
            crate::match_finder::cdc::CdcMethod::M1,
        )?,
        Method::M2Order1Cdc => crate::match_finder::cdc::find_matches_spooled(
            &spool,
            config,
            context,
            crate::match_finder::cdc::CdcMethod::M2,
        )?,
        Method::M3FixedDigest | Method::M4Reread => {
            let candidates = crate::match_finder::fixed::find_matches_spooled(
                &spool,
                config,
                context,
                config.method,
            )?;
            return crate::reference::compress_spooled_with_candidates(
                spool, config, candidates, context, output,
            );
        }
        Method::M5Exhaustive => {
            let candidates =
                crate::match_finder::m5::find_matches_spooled(&spool, config, context)?;
            return crate::reference::compress_spooled_with_candidates(
                spool, config, candidates, context, output,
            );
        }
    };
    crate::reference::compress_spooled_with_candidates(spool, config, candidates, context, output)
}

pub fn decompress<R: Read, W: Write>(input: R, output: W) -> Result<CompressionStats> {
    let resources = ResourceConfig::default();
    let context = ResourceContext::with_resources(&resources).expect("default resources valid");
    decompress_with_context(input, output, &resources, &context)
}

pub fn decompress_with_resources<R: Read, W: Write>(
    input: R,
    output: W,
    resources: &ResourceConfig,
) -> Result<CompressionStats> {
    validate_resources(resources)?;
    let context = ResourceContext::with_resources(resources)?;
    decompress_with_context(input, output, resources, &context)
}

pub fn decompress_with_context<R: Read, W: Write>(
    input: R,
    output: W,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<CompressionStats> {
    validate_resources(resources)?;
    decode(input, output, resources, context, true)
}

pub fn verify<R: Read>(input: R) -> Result<CompressionStats> {
    let resources = ResourceConfig::default();
    let context = ResourceContext::with_resources(&resources).expect("default resources valid");
    decode(input, std::io::sink(), &resources, &context, false)
}

pub fn verify_with_resources<R: Read>(
    input: R,
    resources: &ResourceConfig,
) -> Result<CompressionStats> {
    let context = ResourceContext::with_resources(resources)?;
    decode(input, std::io::sink(), resources, &context, false)
}

pub fn inspect<R: Read>(input: R) -> Result<ArchiveInfo> {
    inspect_archive(input)
}

pub fn inspect_with_resources<R: Read>(
    input: R,
    resources: &ResourceConfig,
) -> Result<ArchiveInfo> {
    inspect_archive_with_resources(input, resources)
}

pub fn inspect_matches<R: Read>(input: R) -> Result<crate::match_ir::InspectedMatches> {
    inspect_matches_with_resources(input, &ResourceConfig::default())
}

pub fn inspect_matches_with_resources<R: Read>(
    input: R,
    resources: &ResourceConfig,
) -> Result<crate::match_ir::InspectedMatches> {
    validate_resources(resources)?;
    let context = ResourceContext::with_resources(resources)?;
    inspect_matches_with_context(input, resources, &context)
}

pub fn inspect_matches_with_context<R: Read>(
    mut input: R,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<crate::match_ir::InspectedMatches> {
    validate_resources(resources)?;
    let (kind, header_bytes) = read_and_classify(&mut input)?;
    match kind {
        ArchiveKind::NgV2 => {
            crate::reference::inspect_matches(input, header_bytes, resources, context)
        }
        ArchiveKind::PrototypeV1 => Err(Error::unsupported_version(
            "experimental SREP-NG v1 is not supported",
        )),
        ArchiveKind::Legacy => Err(Error::unsupported_version(
            "match inspection is only defined for SREP-NG v2",
        )),
    }
}

fn decode<R: Read, W: Write>(
    mut input: R,
    mut output: W,
    resources: &ResourceConfig,
    context: &ResourceContext,
    write: bool,
) -> Result<CompressionStats> {
    validate_resources(resources)?;
    let (kind, header_bytes) = read_and_classify(&mut input)?;
    match kind {
        ArchiveKind::NgV2 => {
            parse_archive_header(&header_bytes)?;
            if write {
                let mut spool = TempSpool::new(resources, context)?;
                let result = decode_v2(
                    &mut input,
                    &mut spool,
                    header_bytes,
                    resources,
                    context,
                    true,
                );
                let stats = match result {
                    Ok(stats) => stats,
                    Err(error) => return Err(spool.take_budget_error().unwrap_or(error)),
                };
                spool.rewind()?;
                copy_spool(&mut spool.file, &mut output)?;
                Ok(stats)
            } else {
                decode_v2(
                    &mut input,
                    &mut output,
                    header_bytes,
                    resources,
                    context,
                    false,
                )
            }
        }
        ArchiveKind::PrototypeV1 => Err(Error::unsupported_version(
            "experimental SREP-NG v1 is not supported",
        )),
        ArchiveKind::Legacy => {
            let mut staged = if write {
                Some(TempSpool::new(resources, context)?)
            } else {
                None
            };
            let result = if let Some(ref mut spool) = staged {
                crate::legacy::decode(header_bytes, &mut input, spool, resources, context, true)?
            } else {
                crate::legacy::decode(
                    header_bytes,
                    &mut input,
                    &mut std::io::sink(),
                    resources,
                    context,
                    false,
                )?
            };
            if let Some(mut spool) = staged {
                spool.rewind()?;
                copy_spool(&mut spool.file, &mut output)?;
            }
            Ok(CompressionStats {
                original_size: result.original_size,
                archive_size: result.archive_size,
                payload_size: result.archive_size.saturating_sub(result.header_size),
                block_count: result.block_count,
                compressed_blocks: result.block_count,
                reference_count: 0,
                semantic_match_count: result.match_count,
                covered_bytes: result.covered_bytes,
                literal_bytes: result.literal_bytes,
                method: None,
                layout: None,
                checksum: None,
            })
        }
    }
}

fn decode_v2<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    header_bytes: Vec<u8>,
    resources: &ResourceConfig,
    context: &ResourceContext,
    write: bool,
) -> Result<CompressionStats> {
    let header = parse_archive_header(&header_bytes)?;
    let checksum = header.checksum;
    let (method_frame, method_payload, method_digest) = read_fixed_record::<_, 64>(
        input,
        RECORD_METHOD_PARAMETERS,
        checksum,
        StructuralError::Header,
    )?;
    let _ = parse_method_parameters(&method_payload, &header)?;
    verify_ordinary_checksum(
        checksum,
        &method_frame,
        &method_payload,
        &method_digest[..checksum.width()],
    )?;
    let (layout_frame, layout_payload, layout_digest) = read_fixed_record::<_, 64>(
        input,
        RECORD_LAYOUT_METADATA,
        checksum,
        StructuralError::Record,
    )?;
    let meta = parse_layout_metadata(&layout_payload, &header)?;
    verify_ordinary_checksum(
        checksum,
        &layout_frame,
        &layout_payload,
        &layout_digest[..checksum.width()],
    )?;
    if meta.uncompressed_len > resources.output_limit {
        return Err(Error::output_limit(
            "archive uncompressed length exceeds output limit",
        ));
    }
    if meta.block_count != expected_block_count(meta.uncompressed_len, header.block_size)? {
        return Err(Error::corrupt_record(
            "block_count does not match uncompressed length and block size",
        ));
    }
    if meta.semantic_match_count != 0 || meta.covered_bytes != 0 {
        let mut prefix = Vec::new();
        prefix.extend_from_slice(&method_frame);
        prefix.extend_from_slice(&method_payload);
        prefix.extend_from_slice(&method_digest[..checksum.width()]);
        prefix.extend_from_slice(&layout_frame);
        prefix.extend_from_slice(&layout_payload);
        prefix.extend_from_slice(&layout_digest[..checksum.width()]);
        let mut replay = std::io::Cursor::new(prefix).chain(input);
        return crate::reference::decode_with_candidates(
            &mut replay,
            output,
            header_bytes,
            resources,
            context,
            write,
        );
    }
    if meta.literal_bytes != meta.uncompressed_len {
        return Err(Error::corrupt_record(
            "literal-only archive must have literal_bytes equal to uncompressed_len",
        ));
    }

    let mut offset = HEADER_LEN as u64;
    offset = add(offset, record_len(&method_frame, checksum)?)?;
    offset = add(offset, record_len(&layout_frame, checksum)?)?;
    let mut directory = if header.layout.is_index() {
        let expected = format::directory_payload_len(meta.block_count)?;
        let (frame, directory) = read_directory_record(
            input,
            checksum,
            expected,
            meta.block_count,
            resources,
            context,
        )?;
        offset = add(offset, record_len(&frame, checksum)?)?;
        Some(directory)
    } else {
        None
    };

    let mut semantic_digest =
        archive_digest_start(checksum, &header_bytes, &method_payload, &layout_payload);
    let mut total_data_record_bytes = 0u64;
    let mut dst = 0u64;
    for block_id in 0..meta.block_count {
        let expected_len = meta
            .uncompressed_len
            .checked_sub(dst)
            .ok_or_else(|| Error::corrupt_record("block destination exceeds uncompressed_len"))?
            .min(header.block_size);
        let frame = read_record_frame(input)?;
        let (record_type, payload_len) = parse_frame(&frame)?;
        if record_type != RECORD_DATA_BLOCK {
            return Err(Error::corrupt_record("expected DataBlock record"));
        }
        let (block_header, total) = read_datablock(
            input,
            output,
            write,
            checksum,
            &frame,
            payload_len,
            &header,
            block_id,
            dst,
            expected_len,
            &mut semantic_digest,
            resources,
            context,
        )?;
        if let Some(directory) = directory.as_mut() {
            let entry = directory.entry(block_id)?;
            validate_directory_entry(&entry, &block_header, offset, total, 1)?;
        }
        total_data_record_bytes = add(total_data_record_bytes, total)?;
        offset = add(offset, total)?;
        dst = add(dst, expected_len)?;
    }
    if dst != meta.uncompressed_len {
        return Err(Error::corrupt_record(
            "reconstructed length does not match LayoutMetadata",
        ));
    }

    let body_end = offset;
    let (index_offset, index_total) = if header.layout.is_index() {
        let index_offset = offset;
        let expected = format::index_section_payload_len(0, meta.block_count)?;
        let frame = read_index_record(input, checksum, expected, meta.block_count)?;
        let total = record_len(&frame, checksum)?;
        offset = add(offset, total)?;
        (index_offset, total)
    } else {
        (0, 0)
    };

    let summary_offset = offset;
    let expected_summary = format::SUMMARY_PREFIX_LEN + checksum.width();
    let (summary_frame, summary_buffer, summary_digest) = read_fixed_record::<_, 104>(
        input,
        RECORD_ARCHIVE_SUMMARY,
        checksum,
        StructuralError::Record,
    )?;
    let summary_payload = &summary_buffer[..expected_summary];
    let (declared_data_bytes, declared_index_bytes, _, digest) =
        parse_archive_summary(summary_payload, &header, &meta)?;
    verify_ordinary_checksum(
        checksum,
        &summary_frame,
        summary_payload,
        &summary_digest[..checksum.width()],
    )?;
    if declared_data_bytes != total_data_record_bytes {
        return Err(Error::corrupt_record(
            "ArchiveSummary total_data_record_bytes mismatch",
        ));
    }
    if let Some(directory) = directory.as_mut() {
        if declared_index_bytes != index_total {
            return Err(Error::corrupt_index(
                "ArchiveSummary index_section_total_record_bytes mismatch",
            ));
        }
        if directory.total_data_bytes()? != total_data_record_bytes {
            return Err(Error::corrupt_index(
                "BlockDirectory data_record_total_len sum mismatch",
            ));
        }
    }
    verify_bytes(
        checksum,
        &semantic_digest.finalize(),
        &digest[..checksum.width()],
        "ArchiveSummary semantic digest mismatch",
    )?;
    let summary_total = record_len(&summary_frame, checksum)?;
    offset = add(offset, summary_total)?;
    let mut trailer_bytes = [0u8; TRAILER_LEN];
    input
        .read_exact(&mut trailer_bytes)
        .map_err(|error| Error::map_eof(error, "truncated trailer"))?;
    let file_len = add(offset, TRAILER_LEN as u64)?;
    let trailer = parse_trailer(&trailer_bytes, header.layout, file_len)?;
    validate_trailer(
        &trailer,
        summary_offset,
        summary_total,
        index_offset,
        index_total,
        body_end,
        header.layout,
        meta.block_count,
    )?;
    let mut trailing = [0u8; 1];
    match input.read(&mut trailing) {
        Ok(0) => {}
        Ok(_) => return Err(Error::corrupt_record("trailing data after trailer")),
        Err(error) => return Err(Error::input_io(error)),
    }
    Ok(CompressionStats {
        original_size: meta.uncompressed_len,
        archive_size: file_len,
        payload_size: total_data_record_bytes,
        block_count: meta.block_count,
        compressed_blocks: 0,
        reference_count: 0,
        semantic_match_count: 0,
        covered_bytes: 0,
        literal_bytes: meta.literal_bytes,
        method: Some(header.method),
        layout: Some(header.layout),
        checksum: Some(header.checksum),
    })
}

fn inspect_from_stats(stats: CompressionStats, header: &ArchiveHeader) -> ArchiveInfo {
    ArchiveInfo {
        version: 2,
        method: Some(header.method),
        layout: Some(header.layout),
        checksum: Some(header.checksum),
        legacy_layout: None,
        legacy_checksum: None,
        legacy_base_len: None,
        block_size: Some(header.block_size),
        min_match: Some(header.min_match),
        original_size: stats.original_size,
        payload_size: stats.payload_size,
        block_count: stats.block_count,
        semantic_match_count: stats.semantic_match_count,
        covered_bytes: stats.covered_bytes,
        literal_bytes: stats.literal_bytes,
    }
}

pub fn inspect_header<R: Read>(input: R) -> Result<ArchiveInfo> {
    inspect_header_with_resources(input, &ResourceConfig::default())
}

fn inspect_header_with_resources<R: Read>(
    mut input: R,
    resources: &ResourceConfig,
) -> Result<ArchiveInfo> {
    let stats_and_header = {
        let (kind, header_bytes) = read_and_classify(&mut input)?;
        if kind == ArchiveKind::Legacy {
            let context = ResourceContext::with_resources(resources)?;
            let result = crate::legacy::decode(
                header_bytes,
                &mut input,
                &mut std::io::sink(),
                resources,
                &context,
                false,
            )?;
            return Ok(ArchiveInfo {
                version: result.version,
                method: None,
                layout: None,
                checksum: None,
                legacy_layout: Some(result.layout.name().to_owned()),
                legacy_checksum: Some(result.checksum.name().to_owned()),
                legacy_base_len: Some(result.base_len as u64),
                block_size: None,
                min_match: None,
                original_size: result.original_size,
                payload_size: result.archive_size.saturating_sub(result.header_size),
                block_count: result.block_count,
                semantic_match_count: result.match_count,
                covered_bytes: result.covered_bytes,
                literal_bytes: result.literal_bytes,
            });
        }
        if kind != ArchiveKind::NgV2 {
            return Err(Error::unsupported_version(
                "inspect requires a SREP-NG v2 archive",
            ));
        }
        let header = parse_archive_header(&header_bytes)?;
        let context = ResourceContext::with_resources(resources)?;
        let stats = decode_v2(
            &mut input,
            &mut std::io::sink(),
            header_bytes.clone(),
            resources,
            &context,
            false,
        )?;
        inspect_from_stats(stats, &header)
    };
    Ok(stats_and_header)
}

struct DirectorySpool {
    file: File,
    _temp: NamedTempFile,
    _reservation: Reservation,
    block_count: u64,
}

impl DirectorySpool {
    fn entry(&mut self, block_id: u64) -> Result<BlockDirectoryEntry> {
        if block_id >= self.block_count {
            return Err(Error::corrupt_index("missing BlockDirectory entry"));
        }
        let offset = (format::BLOCK_DIRECTORY_ENTRY_LEN as u64)
            .checked_mul(block_id)
            .ok_or_else(|| Error::corrupt_index("BlockDirectory entry offset overflows"))?;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(Error::temp_storage)?;
        let mut bytes = [0u8; format::BLOCK_DIRECTORY_ENTRY_LEN];
        self.file
            .read_exact(&mut bytes)
            .map_err(Error::temp_storage)?;
        Ok(BlockDirectoryEntry {
            block_id: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            dst_start: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            uncompressed_len: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            data_record_offset: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            data_record_total_len: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            literal_run_count: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
            first_starting_match_index: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
            starting_match_count: u64::from_le_bytes(bytes[56..64].try_into().unwrap()),
        })
    }

    fn total_data_bytes(&mut self) -> Result<u64> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(Error::temp_storage)?;
        let mut sum = 0u64;
        let mut bytes = [0u8; format::BLOCK_DIRECTORY_ENTRY_LEN];
        for _ in 0..self.block_count {
            self.file
                .read_exact(&mut bytes)
                .map_err(Error::temp_storage)?;
            let len = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
            sum = sum
                .checked_add(len)
                .ok_or_else(|| Error::corrupt_index("BlockDirectory length sum overflows"))?;
        }
        Ok(sum)
    }
}

fn read_directory_record<R: Read>(
    input: &mut R,
    checksum: Checksum,
    expected_len: u64,
    expected_blocks: u64,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<([u8; FRAME_LEN], DirectorySpool)> {
    let frame = read_record_frame(input)?;
    let (record_type, payload_len) = parse_frame(&frame)?;
    if record_type != RECORD_BLOCK_DIRECTORY {
        return Err(Error::corrupt_record("expected BlockDirectory record"));
    }
    if payload_len != expected_len {
        return Err(Error::corrupt_index(
            "BlockDirectory payload length mismatch",
        ));
    }
    let temp = temp_builder(Builder::new())
        .prefix("srep-directory-")
        .tempfile_in(&resources.temp_dir)
        .map_err(Error::temp_storage)?;
    let mut file = temp.reopen().map_err(Error::temp_storage)?;
    let mut reservation = context.temp.reserve(0)?;
    let mut digest = record_digest_start(checksum, &frame);
    let mut bytes = [0u8; format::BLOCK_DIRECTORY_HEADER_LEN];
    input
        .read_exact(&mut bytes)
        .map_err(|error| Error::map_eof(error, "truncated BlockDirectory header"))?;
    digest.update(&bytes);
    if u16::from_le_bytes(bytes[0..2].try_into().unwrap()) != format::SCHEMA_V1
        || u16::from_le_bytes(bytes[2..4].try_into().unwrap()) != 64
        || bytes[4..8].iter().any(|&byte| byte != 0)
        || u64::from_le_bytes(bytes[8..16].try_into().unwrap()) != expected_blocks
    {
        return Err(Error::corrupt_index("BlockDirectory header is invalid"));
    }
    let mut budget_error = None;
    let mut buffer = [0u8; format::BLOCK_DIRECTORY_ENTRY_LEN];
    for expected_id in 0..expected_blocks {
        input
            .read_exact(&mut buffer)
            .map_err(|error| Error::map_eof(error, "truncated BlockDirectory entry"))?;
        digest.update(&buffer);
        let block_id = u64::from_le_bytes(buffer[0..8].try_into().unwrap());
        let dst_start = u64::from_le_bytes(buffer[8..16].try_into().unwrap());
        let block_len = u64::from_le_bytes(buffer[16..24].try_into().unwrap());
        let data_offset = u64::from_le_bytes(buffer[24..32].try_into().unwrap());
        let data_len = u64::from_le_bytes(buffer[32..40].try_into().unwrap());
        let literal_runs = u64::from_le_bytes(buffer[40..48].try_into().unwrap());
        let first_match = u64::from_le_bytes(buffer[48..56].try_into().unwrap());
        let match_count = u64::from_le_bytes(buffer[56..64].try_into().unwrap());
        if block_id != expected_id
            || (expected_id == 0 && dst_start != 0)
            || literal_runs != 1
            || first_match != 0
            || match_count != 0
        {
            return Err(Error::corrupt_index("BlockDirectory entry is invalid"));
        }
        let _ = (block_len, data_offset, data_len);
        if budget_error.is_none() {
            if let Err(error) = reservation.grow(format::BLOCK_DIRECTORY_ENTRY_LEN as u64) {
                budget_error = Some(error);
            } else if let Err(error) = file.write_all(&buffer) {
                reservation.shrink(format::BLOCK_DIRECTORY_ENTRY_LEN as u64);
                return Err(Error::temp_storage(error));
            }
        }
    }
    let digest_bytes = read_digest(input, checksum)?;
    if let Some(error) = budget_error {
        return Err(error);
    }
    verify_bytes(
        checksum,
        &digest.finalize(),
        &digest_bytes,
        "BlockDirectory checksum mismatch",
    )?;
    file.flush().map_err(Error::temp_storage)?;
    file.seek(SeekFrom::Start(0)).map_err(Error::temp_storage)?;
    Ok((
        frame,
        DirectorySpool {
            file,
            _temp: temp,
            _reservation: reservation,
            block_count: expected_blocks,
        },
    ))
}

fn read_index_record<R: Read>(
    input: &mut R,
    checksum: Checksum,
    expected_len: u64,
    expected_blocks: u64,
) -> Result<[u8; FRAME_LEN]> {
    let frame = read_record_frame(input)?;
    let (record_type, payload_len) = parse_frame(&frame)?;
    if record_type != RECORD_INDEX_SECTION {
        return Err(Error::corrupt_record("expected IndexSection record"));
    }
    if payload_len != expected_len {
        return Err(Error::corrupt_index("IndexSection payload length mismatch"));
    }
    let mut digest = record_digest_start(checksum, &frame);
    let mut header = [0u8; format::INDEX_SECTION_HEADER_LEN];
    input
        .read_exact(&mut header)
        .map_err(|error| Error::map_eof(error, "truncated IndexSection header"))?;
    digest.update(&header);
    let match_count = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let block_count = u64::from_le_bytes(header[16..24].try_into().unwrap());
    if u16::from_le_bytes(header[0..2].try_into().unwrap()) != format::SCHEMA_V1
        || u16::from_le_bytes(header[2..4].try_into().unwrap()) != 24
        || u16::from_le_bytes(header[4..6].try_into().unwrap()) != 24
        || header[6] != 0
        || header[7] != 0
        || header[24..32].iter().any(|&byte| byte != 0)
        || match_count != 0
        || block_count != expected_blocks
    {
        return Err(Error::corrupt_index("IndexSection header is invalid"));
    }
    let mut range = [0u8; format::INDEX_RANGE_ENTRY_LEN];
    for expected_id in 0..expected_blocks {
        input
            .read_exact(&mut range)
            .map_err(|error| Error::map_eof(error, "truncated IndexSection range"))?;
        digest.update(&range);
        if u64::from_le_bytes(range[0..8].try_into().unwrap()) != expected_id
            || u64::from_le_bytes(range[8..16].try_into().unwrap()) != 0
            || u64::from_le_bytes(range[16..24].try_into().unwrap()) != 0
        {
            return Err(Error::corrupt_index(
                "IndexSection range entries are invalid for a literal-only archive",
            ));
        }
    }
    let actual = read_digest(input, checksum)?;
    verify_bytes(
        checksum,
        &digest.finalize(),
        &actual,
        "IndexSection checksum mismatch",
    )?;
    Ok(frame)
}

fn read_fixed_record<R: Read, const N: usize>(
    input: &mut R,
    expected: u8,
    checksum: Checksum,
    structural_error: StructuralError,
) -> Result<([u8; FRAME_LEN], [u8; N], [u8; 32])> {
    let frame = read_record_frame(input)?;
    let (record_type, payload_len) = parse_frame(&frame)?;
    let expected_len = if N == 104 {
        (format::SUMMARY_PREFIX_LEN + checksum.width()) as u64
    } else {
        N as u64
    };
    if record_type != expected {
        return Err(Error::corrupt_record("unexpected fixed record type"));
    }
    if payload_len != expected_len {
        return Err(match structural_error {
            StructuralError::Header => Error::corrupt_header("fixed record framing is invalid"),
            StructuralError::Record => Error::corrupt_record("fixed record framing is invalid"),
        });
    }
    let mut payload = [0u8; N];
    input
        .read_exact(&mut payload[..usize::try_from(expected_len).unwrap()])
        .map_err(|error| Error::map_eof(error, "truncated fixed record payload"))?;
    let mut digest = [0u8; 32];
    input
        .read_exact(&mut digest[..checksum.width()])
        .map_err(|error| Error::map_eof(error, "truncated fixed record checksum"))?;
    Ok((frame, payload, digest))
}

#[derive(Clone, Copy)]
enum StructuralError {
    Header,
    Record,
}

fn record_len(frame: &[u8; FRAME_LEN], checksum: Checksum) -> Result<u64> {
    let (_, payload_len) = parse_frame(frame)?;
    record_total_len(payload_len, checksum)
}

pub(crate) fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| Error::corrupt_record("archive offset overflows"))
}

pub(crate) fn u64_len(len: usize) -> Result<u64> {
    u64::try_from(len).map_err(|_| Error::corrupt_record("length exceeds u64"))
}

pub(crate) fn block_len_at(total: u64, block_size: u64, block_id: u64) -> Result<u64> {
    let start = block_id
        .checked_mul(block_size)
        .ok_or_else(|| Error::corrupt_record("block offset overflows"))?;
    total
        .checked_sub(start)
        .map(|remaining| remaining.min(block_size))
        .filter(|&len| len > 0)
        .ok_or_else(|| Error::corrupt_record("block is outside input"))
}

pub(crate) fn expected_block_count(len: u64, block_size: u64) -> Result<u64> {
    if len == 0 {
        return Ok(0);
    }
    if block_size == 0 {
        return Err(Error::corrupt_header("block size is zero"));
    }
    Ok(len.div_ceil(block_size))
}

pub(crate) struct InputSpool {
    pub(crate) file: File,
    pub(crate) _temp: NamedTempFile,
    pub(crate) len: u64,
    pub(crate) _reservation: Reservation,
}

pub(crate) fn spool_input<R: Read>(
    mut input: R,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<InputSpool> {
    let temp_dir = &resources.temp_dir;
    fs::create_dir_all(temp_dir).map_err(Error::temp_storage)?;
    let temp = temp_builder(Builder::new())
        .prefix("srep-input-")
        .tempfile_in(temp_dir)
        .map_err(Error::temp_storage)?;
    let mut file = temp.reopen().map_err(Error::temp_storage)?;
    let mut buf = [0u8; 64 * 1024];
    let mut len = 0u64;
    let mut reservation = context.temp.reserve(0)?;
    loop {
        let n = input.read(&mut buf).map_err(Error::input_io)?;
        if n == 0 {
            break;
        }
        let new_len = len
            .checked_add(n as u64)
            .ok_or_else(|| Error::output_limit("input size overflows"))?;
        if new_len > resources.output_limit {
            return Err(Error::output_limit("input exceeds output limit"));
        }
        if new_len > crate::config::MAX_UNCOMPRESSED {
            return Err(Error::output_limit("input exceeds wire size limit"));
        }
        reservation.grow(n as u64)?;
        if let Err(error) = file.write_all(&buf[..n]) {
            let _ = file.set_len(len);
            reservation.shrink(n as u64);
            return Err(Error::temp_storage(error));
        }
        len = new_len;
    }
    file.flush().map_err(Error::temp_storage)?;
    file.seek(SeekFrom::Start(0)).map_err(Error::temp_storage)?;
    Ok(InputSpool {
        file,
        _temp: temp,
        len,
        _reservation: reservation,
    })
}

pub(crate) struct TempSpool {
    pub(crate) file: File,
    pub(crate) _temp: NamedTempFile,
    pub(crate) reservation: Reservation,
    pub(crate) len: u64,
    budget_error: Option<Error>,
}

impl Write for TempSpool {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let position = self.file.stream_position()?;
        match self.write_at(position, bytes) {
            Ok(()) => Ok(bytes.len()),
            Err(error) => {
                if error.kind() == crate::error::ErrorKind::TempBudgetExceeded {
                    self.budget_error = Some(error);
                    Err(std::io::Error::other("temporary budget exceeded"))
                } else {
                    Err(std::io::Error::other(error.to_string()))
                }
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl TempSpool {
    pub(crate) fn new(resources: &ResourceConfig, context: &ResourceContext) -> Result<Self> {
        let temp = temp_builder(Builder::new())
            .prefix("srep-output-")
            .tempfile_in(&resources.temp_dir)
            .map_err(Error::temp_storage)?;
        let file = temp.reopen().map_err(Error::temp_storage)?;
        let reservation = context.temp.reserve(0)?;
        Ok(Self {
            file,
            _temp: temp,
            reservation,
            len: 0,
            budget_error: None,
        })
    }

    pub(crate) fn write_at(&mut self, position: u64, bytes: &[u8]) -> Result<()> {
        let byte_len = u64::try_from(bytes.len())
            .map_err(|_| Error::temp_limit("temporary write length overflows"))?;
        let end = position
            .checked_add(byte_len)
            .ok_or_else(|| Error::temp_limit("temporary write offset overflows"))?;
        let growth = end.saturating_sub(self.len);
        if growth != 0 {
            self.reservation.grow(growth)?;
        }
        let result = self
            .file
            .seek(SeekFrom::Start(position))
            .map_err(Error::temp_storage)
            .and_then(|_| self.file.write_all(bytes).map_err(Error::temp_storage));
        if let Err(error) = result {
            let _ = self.file.set_len(self.len);
            if growth != 0 {
                self.reservation.shrink(growth);
            }
            return Err(error);
        }
        self.len = self.len.max(end);
        Ok(())
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) -> Result<u64> {
        let position = self.len;
        self.write_at(position, bytes)?;
        Ok(position)
    }

    pub(crate) fn rewind(&mut self) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(Error::temp_storage)
            .map(|_| ())
    }

    pub(crate) fn take_budget_error(&mut self) -> Option<Error> {
        self.budget_error.take()
    }
}

pub(crate) fn temp_builder<'a, 'b>(builder: Builder<'a, 'b>) -> Builder<'a, 'b> {
    #[cfg(unix)]
    {
        let mut builder = builder;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
        builder
    }
    #[cfg(not(unix))]
    {
        builder
    }
}

pub(crate) fn copy_spool<R: Read, W: Write>(input: &mut R, output: &mut W) -> Result<()> {
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer).map_err(Error::input_io)?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(Error::output_io)?;
    }
    output.flush().map_err(Error::output_io)
}

#[allow(clippy::too_many_arguments)]
fn read_datablock<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    write: bool,
    checksum: Checksum,
    frame: &[u8; FRAME_LEN],
    payload_len: u64,
    header: &ArchiveHeader,
    expected_id: u64,
    expected_start: u64,
    expected_len: u64,
    semantic_digest: &mut crate::checksum::Digest,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<(DataBlockHeader, u64)> {
    if payload_len < DATA_BLOCK_HEADER_LEN as u64 {
        return Err(Error::corrupt_record(
            "DataBlock payload shorter than header",
        ));
    }
    let mut fixed = [0u8; DATA_BLOCK_HEADER_LEN];
    input
        .read_exact(&mut fixed)
        .map_err(|error| Error::map_eof(error, "truncated DataBlock header"))?;
    let declared_layout = Layout::from_wire(fixed[2])
        .map_err(|_| Error::corrupt_record("invalid DataBlock layout"))?;
    let block_id = u64::from_le_bytes(fixed[8..16].try_into().unwrap());
    let dst_start = u64::from_le_bytes(fixed[16..24].try_into().unwrap());
    let block_len = u64::from_le_bytes(fixed[24..32].try_into().unwrap());
    let literal_count = u64::from_le_bytes(fixed[32..40].try_into().unwrap());
    let operation_count = u64::from_le_bytes(fixed[40..48].try_into().unwrap());
    if declared_layout != header.layout
        || block_id != expected_id
        || dst_start != expected_start
        || block_len != expected_len
        || fixed[0..2] != crate::format::SCHEMA_V1.to_le_bytes()
        || fixed[3] != 0
        || fixed[4..8].iter().any(|&byte| byte != 0)
    {
        return Err(Error::corrupt_record("DataBlock common header is invalid"));
    }
    let prefix_len = match declared_layout {
        Layout::Index => {
            if literal_count != 1 || operation_count != 0 {
                return Err(Error::corrupt_record("Index-LZ literal counts are invalid"));
            }
            LITERAL_RUN_HEADER_LEN
        }
        Layout::Future => {
            if operation_count != 0 {
                return Err(Error::unsupported_version(
                    "Stage 1 decoder does not implement FutureRegister operations",
                ));
            }
            if literal_count != 1 {
                return Err(Error::corrupt_record("Future literal counts are invalid"));
            }
            LITERAL_RUN_HEADER_LEN
        }
        Layout::Io => {
            if literal_count != 1 || operation_count != 1 {
                return Err(Error::corrupt_record("I/O literal counts are invalid"));
            }
            IO_LITERAL_HEADER_LEN
        }
    };
    if payload_len < (DATA_BLOCK_HEADER_LEN + prefix_len) as u64 {
        return Err(Error::corrupt_record(
            "DataBlock payload is shorter than its operation prefix",
        ));
    }
    let mut operation_prefix = [0u8; IO_LITERAL_HEADER_LEN];
    input
        .read_exact(&mut operation_prefix[..prefix_len])
        .map_err(|error| Error::map_eof(error, "truncated DataBlock operation prefix"))?;
    let exact_tail = match declared_layout {
        Layout::Index | Layout::Future => {
            let dst_offset = u64::from_le_bytes(operation_prefix[0..8].try_into().unwrap());
            let literal_len = u64::from_le_bytes(operation_prefix[8..16].try_into().unwrap());
            if dst_offset != 0 || literal_len != block_len || literal_len == 0 {
                return Err(Error::corrupt_record("LiteralRun does not cover the block"));
            }
            16u64.checked_add(literal_len)
        }
        Layout::Io => {
            let tag = operation_prefix[0];
            if tag == 1 {
                return Err(Error::unsupported_version(
                    "Stage 1 decoder does not implement I/O-LZ match fragments",
                ));
            }
            if tag != 0
                || operation_prefix[1] != 0
                || operation_prefix[2] != 0
                || operation_prefix[3] != 0
            {
                return Err(Error::corrupt_record("I/O operation header is invalid"));
            }
            let encoded_len = u32::from_le_bytes(operation_prefix[4..8].try_into().unwrap()) as u64;
            let literal_len = u64::from_le_bytes(operation_prefix[8..16].try_into().unwrap());
            let expected_encoded = 16u64
                .checked_add(literal_len)
                .ok_or_else(|| Error::corrupt_record("I/O encoded length overflows"))?;
            if encoded_len != expected_encoded || literal_len != block_len || literal_len == 0 {
                return Err(Error::corrupt_record("I/O literal operation is malformed"));
            }
            Some(expected_encoded)
        }
    }
    .ok_or_else(|| Error::corrupt_record("DataBlock length overflows"))?;
    let exact_payload_len = (DATA_BLOCK_HEADER_LEN as u64)
        .checked_add(exact_tail)
        .ok_or_else(|| Error::corrupt_record("DataBlock length overflows"))?;
    if payload_len != exact_payload_len {
        return Err(Error::corrupt_record("DataBlock payload length mismatch"));
    }
    let mut block_digest = block_digest_start(checksum, frame);
    let temp = temp_builder(Builder::new())
        .prefix("srep-block-")
        .tempfile_in(&resources.temp_dir)
        .map_err(Error::temp_storage)?;
    let mut block_file = temp.reopen().map_err(Error::temp_storage)?;
    let mut block_reservation = context.temp.reserve(0)?;
    let mut budget_error = None;
    block_digest.update(&fixed);
    block_digest.update(&operation_prefix[..prefix_len]);
    let block_header = DataBlockHeader {
        layout: declared_layout,
        block_id,
        dst_start,
        uncompressed_len: block_len,
        literal_run_count: literal_count,
        operation_count,
    };
    let mut buffer = [0u8; IO_BUFFER_SIZE];
    let mut remaining = expected_len;
    while remaining > 0 {
        let take = remaining.min(buffer.len() as u64) as usize;
        input
            .read_exact(&mut buffer[..take])
            .map_err(|error| Error::map_eof(error, "truncated DataBlock payload"))?;
        block_digest.update(&buffer[..take]);
        if budget_error.is_none() {
            if let Err(error) = block_reservation.grow(take as u64) {
                budget_error = Some(error);
            } else {
                if let Err(error) = block_file.write_all(&buffer[..take]) {
                    block_reservation.shrink(take as u64);
                    return Err(Error::temp_storage(error));
                }
            }
        }
        remaining -= take as u64;
    }
    let digest = read_digest(input, checksum)?;
    if let Some(error) = budget_error {
        return Err(error);
    }
    block_file.flush().map_err(Error::temp_storage)?;
    block_file
        .seek(SeekFrom::Start(0))
        .map_err(Error::temp_storage)?;
    block_digest.update(&block_id.to_le_bytes());
    block_digest.update(&dst_start.to_le_bytes());
    block_digest.update(&expected_len.to_le_bytes());
    let mut replayed = 0u64;
    while replayed < expected_len {
        let take = (expected_len - replayed).min(buffer.len() as u64) as usize;
        block_file
            .read_exact(&mut buffer[..take])
            .map_err(Error::temp_storage)?;
        block_digest.update(&buffer[..take]);
        replayed += take as u64;
    }
    verify_bytes(
        checksum,
        &block_digest.finalize(),
        &digest,
        "DataBlock representation-plus-semantics checksum mismatch",
    )?;
    block_file
        .seek(SeekFrom::Start(0))
        .map_err(Error::temp_storage)?;
    let mut replayed = 0u64;
    while replayed < expected_len {
        let take = (expected_len - replayed).min(buffer.len() as u64) as usize;
        block_file
            .read_exact(&mut buffer[..take])
            .map_err(Error::temp_storage)?;
        semantic_digest.update(&buffer[..take]);
        if write {
            output
                .write_all(&buffer[..take])
                .map_err(Error::output_io)?;
        }
        replayed += take as u64;
    }
    Ok((block_header, record_total_len(payload_len, checksum)?))
}

fn validate_resources(resources: &ResourceConfig) -> Result<()> {
    if resources.memory == 0 {
        return Err(Error::memory_limit("memory limit must be positive"));
    }
    if resources.temp_limit == 0 {
        return Err(Error::new(
            crate::error::ErrorKind::TempBudgetExceeded,
            "temporary limit must be positive",
        ));
    }
    if resources.output_limit == 0 || resources.output_limit > crate::config::MAX_UNCOMPRESSED {
        return Err(Error::output_limit("output limit is outside wire limits"));
    }
    Ok(())
}

fn validate_directory_entry(
    entry: &BlockDirectoryEntry,
    header: &DataBlockHeader,
    offset: u64,
    total: u64,
    literal_run_count: u64,
) -> Result<()> {
    if entry.block_id != header.block_id
        || entry.dst_start != header.dst_start
        || entry.uncompressed_len != header.uncompressed_len
        || entry.data_record_offset != offset
        || entry.data_record_total_len != total
        || entry.literal_run_count != literal_run_count
        || entry.first_starting_match_index != 0
        || entry.starting_match_count != 0
    {
        return Err(Error::corrupt_index(
            "BlockDirectory entry does not match DataBlock",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_trailer(
    trailer: &Trailer,
    summary_offset: u64,
    summary_total: u64,
    index_offset: u64,
    index_total: u64,
    body_end: u64,
    layout: Layout,
    block_count: u64,
) -> Result<()> {
    if trailer.summary_offset != summary_offset || trailer.summary_total_len != summary_total {
        return Err(Error::corrupt_record(
            "trailer ArchiveSummary offset or length mismatch",
        ));
    }
    if trailer.body_end != body_end {
        return Err(Error::corrupt_record("trailer BodyEnd mismatch"));
    }
    if layout.is_index() {
        if trailer.index_offset != index_offset || trailer.index_total_len != index_total {
            return Err(Error::corrupt_index(
                "trailer IndexSection offset or length mismatch",
            ));
        }
        if block_count == 0 && body_end != summary_offset.saturating_sub(index_total) {
            return Err(Error::corrupt_index(
                "empty Index-LZ BodyEnd must precede IndexSection",
            ));
        }
    } else if trailer.index_offset != 0 || trailer.index_total_len != 0 {
        return Err(Error::corrupt_index(
            "non-Index layout trailer IndexSection fields must be zero",
        ));
    }
    Ok(())
}

// Replace inspect() with a correct implementation that retains header fields.
pub fn inspect_archive<R: Read>(input: R) -> Result<ArchiveInfo> {
    inspect_header(input)
}

pub fn inspect_archive_with_resources<R: Read>(
    input: R,
    resources: &ResourceConfig,
) -> Result<ArchiveInfo> {
    inspect_header_with_resources(input, resources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Checksum, CompressionConfig, Layout, Method};
    use crate::dispatch::{ArchiveKind, classify_prefix};
    use crate::error::ErrorKind;
    use crate::format::{HEADER_LEN, LEGACY_SIGNATURE, NG_V2_MAGIC, PROTOTYPE_V1_MAGIC};

    fn small_config(layout: Layout, checksum: Checksum) -> CompressionConfig {
        CompressionConfig {
            method: Method::M3FixedDigest,
            layout,
            checksum,
            block_size: 1024,
            min_match: 512,
            seed_size: Some(512),
            ..CompressionConfig::default()
        }
    }

    #[test]
    fn temp_spool_reserves_seekable_growth_and_releases_it_on_drop() {
        let resources = ResourceConfig {
            temp_limit: 8,
            ..ResourceConfig::default()
        };
        let context = ResourceContext::with_resources(&resources).unwrap();
        let mut spool = TempSpool::new(&resources, &context).unwrap();
        spool.append(b"1234").unwrap();
        assert_eq!(spool.len, 4);
        assert_eq!(context.temp.current(), 4);
        spool.write_at(2, b"xy").unwrap();
        assert_eq!(spool.len, 4);
        assert_eq!(context.temp.current(), 4);
        assert_eq!(context.temp.high_water(), 4);
        assert!(spool.append(b"56789").is_err());
        assert_eq!(spool.len, 4);
        assert_eq!(context.temp.current(), 4);
        drop(spool);
        assert_eq!(context.temp.current(), 0);
        assert_eq!(context.temp.high_water(), 4);
    }

    #[test]
    fn temp_spool_write_failure_rolls_back_growth_reservation() {
        let resources = ResourceConfig::default();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let mut spool = TempSpool::new(&resources, &context).unwrap();
        let before = context.temp.current();
        let read_only = tempfile::NamedTempFile::new().unwrap();
        spool.file = File::open(read_only.path()).unwrap();
        assert!(spool.append(b"fail").is_err());
        assert_eq!(context.temp.current(), before);
        assert_eq!(spool.len, 0);
    }

    fn roundtrip(layout: Layout, checksum: Checksum, input: &[u8]) -> Vec<u8> {
        let config = small_config(layout, checksum);
        let mut archive = Vec::new();
        let stats = compress_with_candidates(input, &mut archive, &config, []).unwrap();
        assert_eq!(stats.semantic_match_count, 0);
        assert_eq!(stats.covered_bytes, 0);
        assert_eq!(stats.literal_bytes, input.len() as u64);
        assert_eq!(&archive[..8], &NG_V2_MAGIC);
        let mut decoded = Vec::new();
        decompress(archive.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, input);
        archive
    }

    #[test]
    fn phase1_red_compress_must_write_ng_v2_magic() {
        let mut archive = Vec::new();
        compress(
            b"phase1".as_slice(),
            &mut archive,
            &CompressionConfig::default(),
        )
        .unwrap();
        assert_eq!(&archive[..8], b"SREPNG2\0");
    }

    #[test]
    fn phase1_red_prototype_v1_must_be_unsupported_version() {
        let err = verify(&PROTOTYPE_V1_MAGIC[..]).unwrap_err();
        assert_eq!(err.code(), 3);
        assert_eq!(err.message_id(), "SREP_E_UNSUPPORTED_VERSION");
    }

    #[test]
    fn empty_and_nonempty_roundtrip_matrix() {
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                let empty = roundtrip(layout, checksum, b"");
                assert!(verify(empty.as_slice()).is_ok());
                let nonempty = roundtrip(layout, checksum, b"literal-only ng v2 payload");
                let info = inspect_archive(nonempty.as_slice()).unwrap();
                assert_eq!(info.version, 2);
                assert_eq!(info.layout, Some(layout));
                assert_eq!(info.checksum, Some(checksum));
                assert_eq!(info.method, Some(Method::M3FixedDigest));
                assert_eq!(info.semantic_match_count, 0);
                assert_eq!(info.literal_bytes, 26);
                assert_eq!(info.covered_bytes, 0);
            }
        }
    }

    #[test]
    fn multiple_blocks_round_trip() {
        let mut config = small_config(Layout::Index, Checksum::Xxh3);
        config.block_size = 1024;
        let input: Vec<u8> = (0..3000u32).map(|n| n as u8).collect();
        let mut archive = Vec::new();
        let stats = compress_with_candidates(input.as_slice(), &mut archive, &config, []).unwrap();
        assert_eq!(stats.block_count, 3);
        let mut decoded = Vec::new();
        decompress(archive.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, input);
        let info = inspect_archive(archive.as_slice()).unwrap();
        assert_eq!(info.block_count, 3);
        assert_eq!(info.original_size, 3000);
    }

    #[test]
    fn multiblock_total_larger_than_memory_uses_bounded_working_set() {
        let mut config = small_config(Layout::Index, Checksum::Xxh3);
        config.block_size = 1024;
        config.resources.memory = 8192;
        config.resources.temp_limit = 64 * 1024;
        let input: Vec<u8> = (0..10 * 1024).map(|value| value as u8).collect();
        let mut archive = Vec::new();
        compress_with_candidates(&input[..], &mut archive, &config, []).unwrap();
        let mut output = Vec::new();
        let resources = ResourceConfig {
            memory: 8192,
            temp_limit: 64 * 1024,
            ..ResourceConfig::default()
        };
        decompress_with_resources(&archive[..], &mut output, &resources).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn low_memory_rejects_block_working_set_before_allocation() {
        let mut config = small_config(Layout::Index, Checksum::Xxh3);
        config.block_size = 1024;
        config.resources.memory = 1024;
        let error = compress_with_candidates(&vec![1u8; 1024][..], &mut Vec::new(), &config, [])
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MemoryBudgetExceeded);
    }

    #[test]
    fn prototype_v1_and_legacy_dispatch() {
        let proto = verify(&PROTOTYPE_V1_MAGIC[..]).unwrap_err();
        assert_eq!(proto.kind(), ErrorKind::UnsupportedVersion);
        assert_eq!(
            classify_prefix(&LEGACY_SIGNATURE).unwrap(),
            ArchiveKind::Legacy
        );
        let mut legacy_prefix = LEGACY_SIGNATURE.to_vec();
        legacy_prefix.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
        let legacy = verify(&legacy_prefix[..]).unwrap_err();
        assert_eq!(legacy.kind(), ErrorKind::CorruptHeader);
        let bad = verify(&b"NOTSREP!"[..]).unwrap_err();
        assert_eq!(bad.kind(), ErrorKind::CorruptHeader);
    }

    #[test]
    fn reserved_and_trailing_and_truncation_rejected() {
        let archive = roundtrip(Layout::Index, Checksum::Xxh3, b"abc");
        let mut bad_flags = archive.clone();
        bad_flags[9] = 1;
        assert_eq!(
            verify(bad_flags.as_slice()).unwrap_err().kind(),
            ErrorKind::CorruptHeader
        );
        let mut trailing = archive.clone();
        trailing.push(0);
        assert_eq!(
            verify(trailing.as_slice()).unwrap_err().kind(),
            ErrorKind::CorruptRecord
        );
        assert_eq!(
            verify(&archive[..archive.len() - 1]).unwrap_err().kind(),
            ErrorKind::TruncatedArchive
        );
        let mut bad_magic = archive.clone();
        bad_magic[0] ^= 1;
        assert_eq!(
            verify(bad_magic.as_slice()).unwrap_err().kind(),
            ErrorKind::CorruptHeader
        );
    }

    #[test]
    fn checksum_mismatch_on_record_and_digest() {
        let archive = roundtrip(Layout::Future, Checksum::Blake3, b"xyz");
        let mut flipped = archive.clone();
        let last = flipped.len() - TRAILER_LEN - 1;
        flipped[last] ^= 1;
        assert_eq!(
            verify(flipped.as_slice()).unwrap_err().kind(),
            ErrorKind::ChecksumMismatch
        );
    }

    #[test]
    fn datablock_metadata_mutation_is_rejected() {
        let archive = roundtrip(Layout::Index, Checksum::Xxh3, b"mutate-me");
        // DataBlock payload starts after header + two ordinary 64-byte records + directory.
        let mut mutated = archive.clone();
        let method_total = 12 + 64 + 16;
        let layout_total = 12 + 64 + 16;
        let dir_total = 12 + 16 + 64 + 16;
        let payload_header = HEADER_LEN + method_total + layout_total + dir_total + 12;
        mutated[payload_header + 8] ^= 1; // block_id in DataBlock common header
        let error = verify(mutated.as_slice()).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                ErrorKind::ChecksumMismatch | ErrorKind::CorruptRecord | ErrorKind::CorruptIndex
            ),
            "metadata mutation must be rejected, got {:?}",
            error.kind()
        );
    }

    #[test]
    fn unknown_record_type_is_corrupt_record() {
        let mut archive = roundtrip(Layout::Io, Checksum::Xxh3, b"type");
        archive[HEADER_LEN] = 0x7f;
        assert_eq!(
            verify(archive.as_slice()).unwrap_err().kind(),
            ErrorKind::CorruptRecord
        );
    }

    #[test]
    fn malformed_archives_do_not_panic() {
        let archive = roundtrip(Layout::Index, Checksum::Xxh3, b"fuzz");
        for index in 0..archive.len() {
            let mut mutated = archive.clone();
            mutated[index] ^= (index as u8).wrapping_add(1);
            let result = std::panic::catch_unwind(|| verify(mutated.as_slice()));
            assert!(result.is_ok(), "decoder panicked at offset {index}");
        }
    }

    #[test]
    fn empty_index_archive_has_empty_directory_and_index() {
        let archive = roundtrip(Layout::Index, Checksum::Xxh3, b"");
        let info = inspect_archive(archive.as_slice()).unwrap();
        assert_eq!(info.block_count, 0);
        assert_eq!(info.original_size, 0);
        assert!(archive.len() > HEADER_LEN + TRAILER_LEN);
    }

    #[test]
    fn all_method_ids_are_persisted_by_the_writer() {
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
            let mut archive = Vec::new();
            compress(b"persist method".as_slice(), &mut archive, &config).unwrap();
            let info = inspect_archive(archive.as_slice()).unwrap();
            assert_eq!(info.method, Some(method));
            assert_eq!(info.semantic_match_count, 0);
        }
    }

    #[test]
    fn future_match_bytes_are_not_treated_as_literals() {
        let mut archive = roundtrip(Layout::Future, Checksum::Xxh3, b"abcdef");
        let method_total = 12 + 64 + 16;
        let layout_total = 12 + 64 + 16;
        let payload_off = HEADER_LEN + method_total + layout_total + 12;
        // operation_count field in DataBlock common header.
        archive[payload_off + 40] = 1;
        let error = verify(archive.as_slice()).unwrap_err();
        assert!(
            error.kind() == ErrorKind::UnsupportedVersion
                || error.kind() == ErrorKind::CorruptRecord
                || error.kind() == ErrorKind::ChecksumMismatch
        );
        assert_ne!(error.kind(), ErrorKind::InvalidConfiguration);
    }

    #[test]
    fn valid_archive_decodes_with_one_byte_memory_and_bounded_high_water() {
        let mut config = small_config(Layout::Index, Checksum::Blake3);
        config.block_size = 1024;
        let input = vec![0x42; 65 * 1024 + 17];
        let mut archive = Vec::new();
        compress_with_candidates(&input[..], &mut archive, &config, []).unwrap();
        let resources = ResourceConfig {
            memory: 1,
            temp_limit: 128 * 1024,
            ..ResourceConfig::default()
        };
        let context = ResourceContext::with_resources(&resources).unwrap();
        let mut output = Vec::new();
        decompress_with_context(&archive[..], &mut output, &resources, &context).unwrap();
        assert_eq!(output, input);
        assert!(context.memory.high_water() <= resources.memory);
    }

    #[test]
    fn public_writer_emits_no_bytes_after_late_validation_failures() {
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            let archive = roundtrip(layout, Checksum::Xxh3, b"late validation");
            let header = parse_archive_header(&archive[..HEADER_LEN]).unwrap();
            let summary_len = format::SUMMARY_PREFIX_LEN + header.checksum.width();
            let summary_total = FRAME_LEN + summary_len + header.checksum.width();
            let summary_start = archive.len() - TRAILER_LEN - summary_total;
            let cases = [
                ("summary checksum", summary_start + FRAME_LEN + summary_len),
                ("trailer field", archive.len() - TRAILER_LEN + 12),
                ("trailer magic", archive.len() - TRAILER_LEN),
            ];
            for (name, offset) in cases {
                let mut corrupted = archive.clone();
                corrupted[offset] ^= 1;
                let mut output = Vec::new();
                let error = decompress(corrupted.as_slice(), &mut output).unwrap_err();
                assert_eq!(
                    error.kind(),
                    if name == "summary checksum" || name == "semantic digest" {
                        ErrorKind::ChecksumMismatch
                    } else {
                        ErrorKind::CorruptRecord
                    },
                    "{layout:?} {name}"
                );
                assert!(output.is_empty(), "{layout:?} {name} leaked public output");
            }
            let mut semantic_corrupted = archive.clone();
            semantic_corrupted[summary_start + FRAME_LEN + format::SUMMARY_PREFIX_LEN] ^= 1;
            let summary_frame: [u8; FRAME_LEN] = semantic_corrupted
                [summary_start..summary_start + FRAME_LEN]
                .try_into()
                .unwrap();
            let summary_payload = &semantic_corrupted
                [summary_start + FRAME_LEN..summary_start + FRAME_LEN + summary_len];
            let summary_checksum =
                crate::checksum::record_checksum(header.checksum, &summary_frame, summary_payload);
            semantic_corrupted[summary_start + FRAME_LEN + summary_len
                ..summary_start + FRAME_LEN + summary_len + header.checksum.width()]
                .copy_from_slice(&summary_checksum);
            let mut output = Vec::new();
            let error = decompress(semantic_corrupted.as_slice(), &mut output).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ChecksumMismatch);
            assert!(
                error
                    .context()
                    .unwrap_or_default()
                    .contains("ArchiveSummary semantic digest mismatch")
            );
            assert!(
                output.is_empty(),
                "{layout:?} semantic digest leaked public output"
            );
            let mut trailing = archive.clone();
            trailing.push(0);
            let mut output = Vec::new();
            assert_eq!(
                decompress(trailing.as_slice(), &mut output)
                    .unwrap_err()
                    .kind(),
                ErrorKind::CorruptRecord
            );
            assert!(
                output.is_empty(),
                "{layout:?} trailing byte leaked public output"
            );
        }
    }

    #[test]
    fn index_temp_budget_matches_actual_concurrent_spool_bytes() {
        let mut config = small_config(Layout::Index, Checksum::Xxh3);
        config.block_size = 1024;
        for (input, directory_bytes) in [
            (b"".as_slice(), 0u64),
            (b"one block", 64u64),
            (&vec![7u8; 2050], 192u64),
        ] {
            let mut archive = Vec::new();
            compress_with_candidates(input, &mut archive, &config, []).unwrap();
            let generous = ResourceConfig {
                memory: 1,
                temp_limit: 128 * 1024,
                ..ResourceConfig::default()
            };
            let baseline_context = ResourceContext::with_resources(&generous).unwrap();
            let mut baseline_output = Vec::new();
            decompress_with_context(
                &archive[..],
                &mut baseline_output,
                &generous,
                &baseline_context,
            )
            .unwrap();
            assert_eq!(baseline_output, input);
            let expected = baseline_context.temp.high_water();
            assert!(expected >= directory_bytes);
            if expected > 0 {
                let below = ResourceConfig {
                    memory: 1,
                    temp_limit: expected - 1,
                    ..ResourceConfig::default()
                };
                let mut below_output = Vec::new();
                let below_context = ResourceContext::with_resources(&below).unwrap();
                assert_eq!(
                    decompress_with_context(
                        &archive[..],
                        &mut below_output,
                        &below,
                        &below_context,
                    )
                    .unwrap_err()
                    .kind(),
                    ErrorKind::TempBudgetExceeded
                );
            }
            let exact = ResourceConfig {
                memory: 1,
                temp_limit: expected.max(1),
                ..ResourceConfig::default()
            };
            let context = ResourceContext::with_resources(&exact).unwrap();
            let mut output = Vec::new();
            decompress_with_context(archive.as_slice(), &mut output, &exact, &context).unwrap();
            assert_eq!(output, input);
            assert_eq!(context.temp.high_water(), expected.max(1).min(expected));
            assert_eq!(context.temp.current(), 0);
        }
    }

    #[test]
    fn directory_spool_uses_only_physical_entry_bytes() {
        let mut config = small_config(Layout::Index, Checksum::Xxh3);
        config.block_size = 1024;
        for (input, expected_directory_bytes) in [(b"".as_slice(), 0u64), (b"one block", 64u64)] {
            let mut archive = Vec::new();
            compress_with_candidates(input, &mut archive, &config, []).unwrap();
            let resources = ResourceConfig {
                memory: 1,
                temp_limit: if expected_directory_bytes == 0 {
                    1
                } else {
                    4096
                },
                ..ResourceConfig::default()
            };
            let context = ResourceContext::with_resources(&resources).unwrap();
            let mut output = Vec::new();
            decompress_with_context(archive.as_slice(), &mut output, &resources, &context).unwrap();
            assert!(context.temp.high_water() >= expected_directory_bytes);
            if expected_directory_bytes == 0 {
                assert_eq!(context.temp.high_water(), 0);
            }
        }
    }

    #[test]
    fn truncated_variable_payload_or_checksum_precedes_one_byte_memory_policy() {
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            let mut config = small_config(layout, Checksum::Xxh3);
            config.block_size = 1024;
            let input = vec![0x37; 1024];
            let mut archive = Vec::new();
            compress_with_candidates(&input[..], &mut archive, &config, []).unwrap();
            let header = parse_archive_header(&archive[..HEADER_LEN]).unwrap();
            let method_offset = HEADER_LEN;
            let method_total = record_total_len(64, header.checksum).unwrap() as usize;
            let layout_offset = method_offset + method_total;
            let layout_total = method_total;
            let mut records = vec![("MethodParameters", method_offset, 64usize)];
            records.push(("LayoutMetadata", layout_offset, 64));
            let mut cursor = layout_offset + layout_total;
            if layout.is_index() {
                let directory_len = format::directory_payload_len(1).unwrap() as usize;
                records.push(("BlockDirectory", cursor, directory_len));
                cursor += record_total_len(directory_len as u64, header.checksum).unwrap() as usize;
            }
            let data_len =
                u64::from_le_bytes(archive[cursor + 4..cursor + 12].try_into().unwrap()) as usize;
            records.push(("DataBlock", cursor, data_len));
            cursor += record_total_len(data_len as u64, header.checksum).unwrap() as usize;
            if layout.is_index() {
                let index_len = format::index_section_payload_len(0, 1).unwrap() as usize;
                records.push(("IndexSection", cursor, index_len));
                cursor += record_total_len(index_len as u64, header.checksum).unwrap() as usize;
            }
            let summary_len = format::SUMMARY_PREFIX_LEN + header.checksum.width();
            records.push(("ArchiveSummary", cursor, summary_len));
            let resources = ResourceConfig {
                memory: 1,
                temp_limit: 128 * 1024,
                ..ResourceConfig::default()
            };
            for (name, offset, payload_len) in records {
                let payload_start = offset + FRAME_LEN;
                let checksum_start = payload_start + payload_len;
                for cut in [
                    payload_start + payload_len - 1,
                    checksum_start + header.checksum.width() - 1,
                ] {
                    let mut truncated = archive.clone();
                    truncated.truncate(cut);
                    let error = verify_with_resources(&truncated[..], &resources).unwrap_err();
                    assert_eq!(
                        error.kind(),
                        ErrorKind::TruncatedArchive,
                        "{layout:?} {name} cut {cut}"
                    );
                }
            }
            let mut truncated = archive.clone();
            truncated.truncate(archive.len() - 1);
            let error = verify_with_resources(&truncated[..], &resources).unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::TruncatedArchive,
                "{layout:?} Trailer"
            );
        }
    }

    #[test]
    fn fixed_record_length_errors_use_the_record_schema_category() {
        let archive = roundtrip(Layout::Index, Checksum::Xxh3, b"fixed");
        let method_frame = HEADER_LEN;
        let mut method_bad = archive.clone();
        method_bad[method_frame + 4] = 63;
        assert_eq!(
            verify(&method_bad[..]).unwrap_err().kind(),
            ErrorKind::CorruptHeader
        );

        let mut layout_bad = archive.clone();
        let layout_frame = HEADER_LEN + 12 + 64 + 16;
        layout_bad[layout_frame + 4] = 63;
        assert_eq!(
            verify(&layout_bad[..]).unwrap_err().kind(),
            ErrorKind::CorruptRecord
        );

        let mut summary_bad = archive.clone();
        let summary_frame = summary_bad.len() - TRAILER_LEN - (12 + 72 + 16);
        summary_bad[summary_frame + 4] = 71;
        assert_eq!(
            verify(&summary_bad[..]).unwrap_err().kind(),
            ErrorKind::CorruptRecord
        );
    }
}
