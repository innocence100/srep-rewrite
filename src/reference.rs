use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};

use tempfile::Builder;

use crate::checksum::{archive_digest_start, record_digest_start, verify_bytes};
use crate::codec::{
    CompressionStats, InputSpool, TempSpool, block_len_at, expected_block_count, temp_builder,
    u64_len,
};
use crate::config::{Checksum, CompressionConfig, Layout, ResourceConfig};
use crate::error::{Error, Result};
use crate::format::{
    self, ArchiveHeader, BlockDirectoryEntry, DATA_BLOCK_HEADER_LEN, DataBlockHeader, FRAME_LEN,
    FUTURE_REGISTER_LEN, LITERAL_RUN_HEADER_LEN, LayoutMetadata, RECORD_ARCHIVE_SUMMARY,
    RECORD_BLOCK_DIRECTORY, RECORD_DATA_BLOCK, RECORD_INDEX_SECTION, RECORD_LAYOUT_METADATA,
    RECORD_METHOD_PARAMETERS, TRAILER_LEN, encode_archive_header, encode_layout_metadata,
    encode_ordinary_record, encode_trailer, method_parameters_from_config, parse_archive_header,
    parse_archive_summary, parse_frame, parse_layout_metadata, parse_method_parameters,
    parse_trailer, record_total_len, verify_block_checksum, verify_ordinary_checksum,
};
use crate::match_ir::{Match, MatchCandidate, NormalizedMatches, normalize_matches_with_budget};
use crate::resource::{BudgetedVec, ResourceContext};

mod layouts;
mod stores;

use layouts::{BlockPlan, LiteralRef, Operation};
use stores::{ArchiveSpool, BlockParts, ParsedArchive, ParsedBlock, RecordParts};

pub(crate) fn compress_with_candidates<
    R: Read,
    W: Write,
    I: IntoIterator<Item = MatchCandidate>,
>(
    input: R,
    output: W,
    config: &CompressionConfig,
    candidates: I,
    context: &ResourceContext,
) -> Result<CompressionStats> {
    config.validate()?;
    let spool = crate::codec::spool_input(input, &config.resources, context)?;
    let candidates = collect_candidates(candidates, &context.memory)?;
    compress_spooled_with_candidates(spool, config, candidates, context, output)
}

pub(crate) fn compress_spooled_with_candidates<W: Write>(
    spool: crate::codec::InputSpool,
    config: &CompressionConfig,
    mut candidates: BudgetedVec<MatchCandidate>,
    context: &ResourceContext,
    mut output: W,
) -> Result<CompressionStats> {
    config.validate()?;
    let effective_min_match = config.effective_min_match()?;
    validate_candidates(&spool, &mut candidates, effective_min_match)?;
    let normalized = normalize_matches_with_budget(
        candidates.iter().copied(),
        spool.len,
        effective_min_match,
        &context.memory,
    )?;
    let plans = build_plans(&spool, config, &normalized.matches, &context.memory)?;
    let mut staged = TempSpool::new(&config.resources, context)?;
    let stats = match encode_archive(
        &spool,
        config,
        &normalized,
        &plans,
        &context.memory,
        &mut staged,
    ) {
        Ok(stats) => stats,
        Err(error) => return Err(staged.take_budget_error().unwrap_or(error)),
    };
    staged.rewind()?;
    crate::codec::copy_spool(&mut staged.file, &mut output)?;
    Ok(stats)
}

fn collect_candidates<I: IntoIterator<Item = MatchCandidate>>(
    candidates: I,
    budget: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<MatchCandidate>> {
    let mut collected = BudgetedVec::new(budget)?;
    for candidate in candidates {
        collected.push(candidate)?;
    }
    Ok(collected)
}

fn validate_candidates(
    spool: &InputSpool,
    candidates: &mut BudgetedVec<MatchCandidate>,
    min_match: u64,
) -> Result<()> {
    canonicalize_candidates(candidates);
    for candidate in candidates.as_slice() {
        let end = candidate
            .dst
            .checked_add(candidate.len)
            .ok_or_else(|| Error::invalid_match("candidate destination overflows"))?;
        if candidate.src >= candidate.dst || end > spool.len || candidate.src >= spool.len {
            return Err(Error::invalid_match("candidate interval is invalid"));
        }
        if candidate.len <= 25 {
            continue;
        }
        if candidate.len < min_match {
            return Err(Error::invalid_match(
                "candidate is shorter than minimum match",
            ));
        }
        let distance = candidate.dst - candidate.src;
        let mut expected = [0u8; 4096];
        let mut actual = [0u8; 4096];
        let mut file = spool.file.try_clone().map_err(Error::temp_storage)?;
        let mut offset = 0u64;
        let mut period = [0u8; 4096];
        let mut period_len = 0usize;
        if distance <= expected.len() as u64 {
            period_len = usize::try_from(distance)
                .map_err(|_| Error::invalid_match("candidate distance exceeds platform limits"))?;
            file.seek(SeekFrom::Start(candidate.src))
                .map_err(Error::temp_storage)?;
            file.read_exact(&mut period[..period_len])
                .map_err(|error| Error::map_eof(error, "candidate source is unavailable"))?;
        }
        while offset < candidate.len {
            let count = usize::try_from((candidate.len - offset).min(expected.len() as u64))
                .map_err(|_| {
                    Error::invalid_match("candidate validation length exceeds platform limits")
                })?;
            if period_len == 0 {
                read_periodic_file(
                    &mut file,
                    candidate.src,
                    offset % distance,
                    distance,
                    &mut expected[..count],
                )?;
            } else {
                let phase = usize::try_from(offset % distance)
                    .map_err(|_| Error::invalid_match("candidate periodic phase overflows"))?;
                for (index, byte) in expected[..count].iter_mut().enumerate() {
                    *byte = period[(phase + index) % period_len];
                }
            }
            let target_position = candidate
                .dst
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("candidate destination overflows"))?;
            file.seek(SeekFrom::Start(target_position))
                .map_err(Error::temp_storage)?;
            file.read_exact(&mut actual[..count])
                .map_err(|error| Error::map_eof(error, "candidate destination is unavailable"))?;
            if expected[..count] != actual[..count] {
                return Err(Error::invalid_match("candidate bytes do not match"));
            }
            offset = offset
                .checked_add(count as u64)
                .ok_or_else(|| Error::invalid_match("candidate validation offset overflows"))?;
        }
    }
    Ok(())
}

fn canonicalize_candidates(candidates: &mut BudgetedVec<MatchCandidate>) {
    // Finder enumeration can produce the same final triple through multiple
    // interleaved representative distances. Canonicalize globally before the
    // expensive authoritative byte check, retaining the earliest ordinal.
    // This does not alter the normalizer's result: it already deduplicates by
    // the same triple and chooses the minimum insertion ordinal.
    candidates.sort_unstable_by(|a, b| {
        (a.src, a.dst, a.len, a.insertion_ordinal).cmp(&(b.src, b.dst, b.len, b.insertion_ordinal))
    });
    candidates.dedup_by(|a, b| {
        if (a.src, a.dst, a.len) == (b.src, b.dst, b.len) {
            a.insertion_ordinal = a.insertion_ordinal.min(b.insertion_ordinal);
            true
        } else {
            false
        }
    });
}

fn read_periodic_file(
    file: &mut File,
    base: u64,
    mut offset: u64,
    period: u64,
    destination: &mut [u8],
) -> Result<()> {
    if period == 0 {
        return Err(Error::invalid_match("candidate periodic distance is zero"));
    }
    let mut written = 0usize;
    while written < destination.len() {
        let count = usize::try_from((period - offset).min((destination.len() - written) as u64))
            .map_err(|_| Error::invalid_match("candidate periodic read exceeds platform limits"))?;
        let position = base
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("candidate periodic source overflows"))?;
        file.seek(SeekFrom::Start(position))
            .map_err(Error::temp_storage)?;
        file.read_exact(&mut destination[written..written + count])
            .map_err(|error| Error::map_eof(error, "candidate source is unavailable"))?;
        written += count;
        offset = 0;
    }
    Ok(())
}

fn build_plans(
    spool: &InputSpool,
    config: &CompressionConfig,
    matches: &[Match],
    budget: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<BlockPlan>> {
    let blocks = expected_block_count(spool.len, config.block_size)?;
    let mut plans = BudgetedVec::with_capacity(
        usize::try_from(blocks)
            .map_err(|_| Error::memory_limit("block count exceeds platform limits"))?,
        budget,
    )?;
    for block_id in 0..blocks {
        let start = block_id
            .checked_mul(config.block_size)
            .ok_or_else(|| Error::invalid_match("block start overflows"))?;
        let len = block_len_at(spool.len, config.block_size, block_id)?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| Error::invalid_match("block end overflows"))?;
        let literals = literal_ranges(start, end, matches, budget)?;
        let registers = if config.layout == Layout::Future {
            let mut values = BudgetedVec::new(budget)?;
            for item in matches
                .iter()
                .filter(|item| item.src >= start && item.src < end)
            {
                let offset = item.src - start;
                let distance = item.dst - item.src;
                let period = item.len.min(distance);
                values.push((item.origin_match_id, offset, item.dst, item.len, period))?;
            }
            values.sort_unstable_by_key(|&(origin_id, source_offset, destination, _, _)| {
                (source_offset, destination, origin_id)
            });
            values
        } else {
            BudgetedVec::new(budget)?
        };
        let operations = if config.layout == Layout::Io {
            io_operations(start, end, matches, budget)?
        } else {
            BudgetedVec::new(budget)?
        };
        let literal_bytes = literals.iter().try_fold(0u64, |sum, &(_, len)| {
            sum.checked_add(len)
                .ok_or_else(|| Error::invalid_match("literal bytes overflow"))
        })?;
        let payload_len =
            match config.layout {
                Layout::Index => 48u64
                    .checked_add(literals.iter().try_fold(0u64, |sum, &(_, len)| {
                        sum.checked_add(16)
                            .and_then(|n| n.checked_add(len))
                            .ok_or_else(|| Error::corrupt_record("literal payload overflows"))
                    })?)
                    .ok_or_else(|| Error::corrupt_record("DataBlock payload overflows"))?,
                Layout::Future => 48u64
                    .checked_add((registers.len() as u64).checked_mul(40).ok_or_else(|| {
                        Error::corrupt_record("Future register payload overflows")
                    })?)
                    .and_then(|n| {
                        n.checked_add(
                            literal_bytes.checked_add((literals.len() as u64).checked_mul(16)?)?,
                        )
                    })
                    .ok_or_else(|| Error::corrupt_record("Future payload overflows"))?,
                Layout::Io => {
                    let operation_bytes = operations.iter().try_fold(0u64, |sum, operation| {
                        let size = match operation {
                            Operation::Literal(_, len) => 16u64.checked_add(*len),
                            Operation::Match(_, _, _, _) => Some(40),
                        }
                        .ok_or_else(|| Error::corrupt_record("I/O operation overflows"))?;
                        sum.checked_add(size)
                            .ok_or_else(|| Error::corrupt_record("I/O operation bytes overflow"))
                    })?;
                    48u64
                        .checked_add(operation_bytes)
                        .ok_or_else(|| Error::corrupt_record("I/O payload overflows"))?
                }
            };
        plans.push(BlockPlan {
            start,
            len,
            literals,
            registers,
            operations,
            payload_len,
        })?;
    }
    Ok(plans)
}

fn literal_ranges(
    start: u64,
    end: u64,
    matches: &[Match],
    budget: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<(u64, u64)>> {
    let mut cursor = start;
    let mut ranges = BudgetedVec::new(budget)?;
    for item in matches {
        let item_end = item
            .dst
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("match endpoint overflows"))?;
        if item_end <= start || item.dst >= end {
            continue;
        }
        let match_start = item.dst.max(start);
        if match_start > cursor {
            ranges.push((cursor - start, match_start - cursor))?;
        }
        cursor = cursor.max(item_end.min(end));
    }
    if cursor < end {
        ranges.push((cursor - start, end - cursor))?;
    }
    Ok(ranges)
}

fn io_operations(
    start: u64,
    end: u64,
    matches: &[Match],
    budget: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<Operation>> {
    let mut operations = BudgetedVec::new(budget)?;
    let mut cursor = start;
    for item in matches {
        let item_end = item
            .dst
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("match endpoint overflows"))?;
        if item_end <= start || item.dst >= end {
            continue;
        }
        let match_start = item.dst.max(start);
        if match_start > cursor {
            operations.push(Operation::Literal(cursor - start, match_start - cursor))?;
        }
        let fragment_start = match_start;
        let fragment_end = item_end.min(end);
        let source = item
            .src
            .checked_add(fragment_start - item.dst)
            .ok_or_else(|| Error::invalid_match("fragment source overflows"))?;
        operations.push(Operation::Match(
            item.origin_match_id,
            source,
            fragment_start,
            fragment_end - fragment_start,
        ))?;
        cursor = fragment_end;
    }
    if cursor < end {
        operations.push(Operation::Literal(cursor - start, end - cursor))?;
    }
    Ok(operations)
}

fn encode_archive<W: Write>(
    spool: &InputSpool,
    config: &CompressionConfig,
    normalized: &NormalizedMatches,
    plans: &[BlockPlan],
    memory: &crate::resource::MemoryBudget,
    output: &mut W,
) -> Result<CompressionStats> {
    let header = ArchiveHeader::from_config(config)?;
    let header_bytes = encode_archive_header(&header)?;
    let method_payload = format::encode_method_parameters(&method_parameters_from_config(config)?);
    let block_count = plans.len() as u64;
    let encoded_operations = match config.layout {
        Layout::Index => 0,
        Layout::Future => normalized.matches.len() as u64,
        Layout::Io => plans
            .iter()
            .try_fold(0u64, |sum, plan| {
                sum.checked_add(plan.operations.len() as u64)
            })
            .ok_or_else(|| Error::corrupt_record("operation count overflows"))?,
    };
    let meta = LayoutMetadata {
        layout: config.layout,
        uncompressed_len: spool.len,
        block_count,
        data_record_count: block_count,
        semantic_match_count: normalized.matches.len() as u64,
        covered_bytes: normalized.covered_bytes,
        literal_bytes: normalized.literal_bytes,
        encoded_operation_count: encoded_operations,
    };
    let layout_payload = encode_layout_metadata(&meta)?;
    let method_record =
        encode_ordinary_record(RECORD_METHOD_PARAMETERS, &method_payload, config.checksum)?;
    let layout_record =
        encode_ordinary_record(RECORD_LAYOUT_METADATA, &layout_payload, config.checksum)?;
    let mut offset = 80u64 + u64_len(method_record.len())? + u64_len(layout_record.len())?;
    let directory_record = if config.layout.is_index() {
        let directory_payload_len = format::directory_payload_len(block_count)?;
        let directory_total = record_total_len(directory_payload_len, config.checksum)?;
        let mut entries = BudgetedVec::with_capacity(
            usize::try_from(block_count).map_err(|_| {
                Error::memory_limit("directory entry count exceeds platform limits")
            })?,
            memory,
        )?;
        let mut data_offset = offset
            .checked_add(directory_total)
            .ok_or_else(|| Error::corrupt_index("directory data offset overflows"))?;
        let mut first_match = 0u64;
        for (block_id, plan) in plans.iter().enumerate() {
            let total = record_total_len(plan.payload_len, config.checksum)?;
            let starting = normalized
                .matches
                .iter()
                .filter(|item| {
                    plan.start
                        .checked_add(plan.len)
                        .is_some_and(|end| item.dst >= plan.start && item.dst < end)
                })
                .count() as u64;
            entries.push(BlockDirectoryEntry {
                block_id: block_id as u64,
                dst_start: plan.start,
                uncompressed_len: plan.len,
                data_record_offset: data_offset,
                data_record_total_len: total,
                literal_run_count: plan.literals.len() as u64,
                first_starting_match_index: first_match,
                starting_match_count: starting,
            })?;
            first_match = first_match
                .checked_add(starting)
                .ok_or_else(|| Error::corrupt_index("directory match count overflows"))?;
            data_offset = data_offset
                .checked_add(total)
                .ok_or_else(|| Error::corrupt_index("directory offset overflows"))?;
        }
        let payload = format::encode_block_directory_with_budget(&entries, memory)?;
        let record = format::encode_ordinary_record_with_budget(
            RECORD_BLOCK_DIRECTORY,
            &payload,
            config.checksum,
            memory,
        )?;
        offset = offset
            .checked_add(u64_len(record.len())?)
            .ok_or_else(|| Error::corrupt_record("archive offset overflows"))?;
        Some(record)
    } else {
        None
    };
    write_all(output, &header_bytes)?;
    write_all(output, &method_record)?;
    write_all(output, &layout_record)?;
    if let Some(record) = &directory_record {
        write_all(output, record)?;
    }
    let mut digest = archive_digest_start(
        config.checksum,
        &header_bytes,
        &method_payload,
        &layout_payload,
    );
    let mut total_data = 0u64;
    for (block_id, plan) in plans.iter().enumerate() {
        let block = read_range(&spool.file, plan.start, plan.len, memory)?;
        let payload = encode_block_payload(
            plan,
            &block,
            &normalized.matches,
            config,
            block_id as u64,
            memory,
        )?;
        let block_header = DataBlockHeader {
            layout: config.layout,
            block_id: block_id as u64,
            dst_start: plan.start,
            uncompressed_len: plan.len,
            literal_run_count: plan.literals.len() as u64,
            operation_count: match config.layout {
                Layout::Index => 0,
                Layout::Future => plan.registers.len() as u64,
                Layout::Io => plan.operations.len() as u64,
            },
        };
        let record = format::encode_data_block_record_with_budget(
            &payload,
            config.checksum,
            block_header.block_id,
            block_header.dst_start,
            &block,
            memory,
        )?;
        write_all(output, &record)?;
        digest.update(&block);
        total_data = total_data
            .checked_add(record.len() as u64)
            .ok_or_else(|| Error::corrupt_record("data record bytes overflow"))?;
    }
    let body_end = offset
        .checked_add(total_data)
        .ok_or_else(|| Error::corrupt_record("body end overflows"))?;
    let (index_record, index_offset, index_total) = if config.layout.is_index() {
        let payload =
            encode_index_matches(block_count, config.block_size, &normalized.matches, memory)?;
        let record = format::encode_ordinary_record_with_budget(
            RECORD_INDEX_SECTION,
            &payload,
            config.checksum,
            memory,
        )?;
        let start = body_end;
        let total = record.len() as u64;
        (Some(record), start, total)
    } else {
        (None, 0, 0)
    };
    if let Some(record) = &index_record {
        write_all(output, record)?;
    }
    let summary_payload = format::encode_archive_summary_with_budget(
        config.checksum,
        spool.len,
        block_count,
        normalized.matches.len() as u64,
        normalized.covered_bytes,
        normalized.literal_bytes,
        total_data,
        index_total,
        block_count + 3 + 2 * u64::from(config.layout.is_index()),
        &digest.finalize(),
        memory,
    )?;
    let summary_record = format::encode_ordinary_record_with_budget(
        RECORD_ARCHIVE_SUMMARY,
        &summary_payload,
        config.checksum,
        memory,
    )?;
    let summary_offset = body_end
        .checked_add(index_total)
        .ok_or_else(|| Error::corrupt_record("summary offset overflows"))?;
    let total_archive_len = summary_offset
        .checked_add(summary_record.len() as u64)
        .and_then(|end| end.checked_add(TRAILER_LEN as u64))
        .ok_or_else(|| Error::corrupt_record("archive length overflows"))?;
    let trailer = encode_trailer(&format::Trailer {
        summary_offset,
        summary_total_len: summary_record.len() as u64,
        index_offset,
        index_total_len: index_total,
        total_archive_len,
        body_end,
    });
    write_all(output, &summary_record)?;
    write_all(output, &trailer)?;
    output.flush().map_err(Error::output_io)?;
    Ok(CompressionStats {
        original_size: spool.len,
        archive_size: total_archive_len,
        payload_size: total_data,
        block_count,
        compressed_blocks: block_count,
        reference_count: normalized.matches.len() as u64,
        semantic_match_count: normalized.matches.len() as u64,
        covered_bytes: normalized.covered_bytes,
        literal_bytes: normalized.literal_bytes,
        method: Some(config.method),
        layout: Some(config.layout),
        checksum: Some(config.checksum),
    })
}

fn encode_block_payload(
    plan: &BlockPlan,
    block: &[u8],
    matches: &[Match],
    config: &CompressionConfig,
    block_id: u64,
    memory: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<u8>> {
    let mut payload = BudgetedVec::with_capacity(
        usize::try_from(plan.payload_len)
            .map_err(|_| Error::memory_limit("DataBlock payload exceeds platform limits"))?,
        memory,
    )?;
    push_common_header(
        &mut payload,
        config.layout,
        block_id,
        plan.start,
        plan.len,
        plan.literals.len() as u64,
        match config.layout {
            Layout::Index => 0,
            Layout::Future => plan.registers.len() as u64,
            Layout::Io => plan.operations.len() as u64,
        },
    )?;
    match config.layout {
        Layout::Index => {
            for &(offset, len) in &plan.literals {
                push_literal(&mut payload, offset, block_range(block, offset, len)?)?;
            }
        }
        Layout::Future => {
            for &(id, source_offset, destination, total_len, period) in &plan.registers {
                for value in [id, source_offset, destination, total_len, period] {
                    payload.push_bytes(&value.to_le_bytes())?;
                }
            }
            for &(offset, len) in &plan.literals {
                push_literal(&mut payload, offset, block_range(block, offset, len)?)?;
            }
        }
        Layout::Io => {
            for operation in &plan.operations {
                match *operation {
                    Operation::Literal(offset, len) => {
                        payload.push(0)?;
                        payload.push_bytes(&[0, 0, 0])?;
                        let encoded = 16u64
                            .checked_add(len)
                            .ok_or_else(|| Error::corrupt_record("I/O literal length overflows"))?;
                        let encoded = u32::try_from(encoded)
                            .map_err(|_| Error::corrupt_record("I/O literal length exceeds u32"))?;
                        payload.push_bytes(&encoded.to_le_bytes())?;
                        payload.push_bytes(&len.to_le_bytes())?;
                        payload.push_bytes(block_range(block, offset, len)?)?;
                    }
                    Operation::Match(id, src, dst, len) => {
                        payload.push(1)?;
                        payload.push_bytes(&[0, 0, 0])?;
                        payload.push_bytes(&40u32.to_le_bytes())?;
                        for value in [id, src, dst, len] {
                            payload.push_bytes(&value.to_le_bytes())?;
                        }
                    }
                }
            }
        }
    }
    if payload.len() as u64 != plan.payload_len {
        return Err(Error::corrupt_record("encoded DataBlock length mismatch"));
    }
    let _ = matches;
    Ok(payload)
}

fn push_common_header(
    payload: &mut BudgetedVec<u8>,
    layout: Layout,
    block_id: u64,
    start: u64,
    len: u64,
    literals: u64,
    operations: u64,
) -> Result<()> {
    payload.push_bytes(&1u16.to_le_bytes())?;
    payload.push(layout.wire_id())?;
    payload.push(0)?;
    payload.push_bytes(&0u32.to_le_bytes())?;
    for value in [block_id, start, len, literals, operations] {
        payload.push_bytes(&value.to_le_bytes())?;
    }
    Ok(())
}

fn push_literal(payload: &mut BudgetedVec<u8>, offset: u64, bytes: &[u8]) -> Result<()> {
    payload.push_bytes(&offset.to_le_bytes())?;
    payload.push_bytes(&(bytes.len() as u64).to_le_bytes())?;
    payload.push_bytes(bytes)?;
    Ok(())
}

fn encode_index_matches(
    block_count: u64,
    block_size: u64,
    matches: &[Match],
    memory: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<u8>> {
    let len = format::index_section_payload_len(matches.len() as u64, block_count)?;
    let size = usize::try_from(len)
        .map_err(|_| Error::memory_limit("IndexSection exceeds platform limits"))?;
    let mut payload = BudgetedVec::with_capacity(size, memory)?;
    payload.resize(size, 0)?;
    let output = payload.as_mut_slice();
    output[..2].copy_from_slice(&format::SCHEMA_V1.to_le_bytes());
    output[2..4].copy_from_slice(&24u16.to_le_bytes());
    output[4..6].copy_from_slice(&24u16.to_le_bytes());
    output[8..16].copy_from_slice(&(matches.len() as u64).to_le_bytes());
    output[16..24].copy_from_slice(&block_count.to_le_bytes());
    let mut cursor = format::INDEX_SECTION_HEADER_LEN;
    for item in matches {
        for value in [item.src, item.dst, item.len] {
            let end = cursor
                .checked_add(8)
                .ok_or_else(|| Error::corrupt_index("IndexSection cursor overflows"))?;
            output
                .get_mut(cursor..end)
                .ok_or_else(|| Error::corrupt_index("IndexSection entry is outside payload"))?
                .copy_from_slice(&value.to_le_bytes());
            cursor = cursor
                .checked_add(8)
                .ok_or_else(|| Error::corrupt_index("IndexSection cursor overflows"))?;
        }
    }
    let mut first = 0usize;
    for block_id in 0..block_count {
        let mut actual = 0usize;
        while first
            .checked_add(actual)
            .is_some_and(|end| end < matches.len())
            && matches
                .get(first + actual)
                .is_some_and(|item| item.dst / block_size == block_id)
        {
            actual += 1;
        }
        let offset = cursor;
        let end = offset
            .checked_add(24)
            .ok_or_else(|| Error::corrupt_index("IndexSection range offset overflows"))?;
        let entry = output
            .get_mut(offset..end)
            .ok_or_else(|| Error::corrupt_index("IndexSection range is outside payload"))?;
        entry[0..8].copy_from_slice(&block_id.to_le_bytes());
        entry[8..16].copy_from_slice(&(first as u64).to_le_bytes());
        entry[16..24].copy_from_slice(&(actual as u64).to_le_bytes());
        cursor = cursor
            .checked_add(24)
            .ok_or_else(|| Error::corrupt_index("IndexSection cursor overflows"))?;
        first = first
            .checked_add(actual)
            .ok_or_else(|| Error::corrupt_index("IndexSection match cursor overflows"))?;
    }
    Ok(payload)
}

fn read_range(
    file: &File,
    start: u64,
    len: u64,
    memory: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<u8>> {
    let size =
        usize::try_from(len).map_err(|_| Error::memory_limit("block exceeds platform limits"))?;
    let mut bytes = BudgetedVec::with_capacity(size, memory)?;
    bytes.resize(size, 0)?;
    let mut file = file.try_clone().map_err(Error::temp_storage)?;
    file.seek(SeekFrom::Start(start))
        .map_err(Error::temp_storage)?;
    file.read_exact(bytes.as_mut_slice())
        .map_err(|error| Error::map_eof(error, "truncated input spool"))?;
    Ok(bytes)
}

fn block_range(block: &[u8], offset: u64, len: u64) -> Result<&[u8]> {
    let start = usize::try_from(offset)
        .map_err(|_| Error::memory_limit("block offset exceeds platform limits"))?;
    let length = usize::try_from(len)
        .map_err(|_| Error::memory_limit("block length exceeds platform limits"))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| Error::invalid_match("block range overflows"))?;
    block
        .get(start..end)
        .ok_or_else(|| Error::invalid_match("block range is outside block"))
}

fn write_all<W: Write>(output: &mut W, bytes: &[u8]) -> Result<()> {
    output.write_all(bytes).map_err(Error::output_io)
}

pub(crate) fn decode_with_candidates<R: Read, W: Write>(
    input: R,
    mut output: W,
    header_bytes: Vec<u8>,
    resources: &ResourceConfig,
    context: &ResourceContext,
    write: bool,
) -> Result<CompressionStats> {
    let archive = spool_archive(input, header_bytes, resources, context)?;
    let parsed = parse_reference_archive(&archive.file, archive.len, resources, context)?;
    if parsed.meta.uncompressed_len > resources.output_limit {
        return Err(Error::output_limit(
            "archive uncompressed length exceeds output limit",
        ));
    }
    let mut history = TempSpool::new(resources, context)?;
    let stats = reconstruct(&parsed, &mut history, &context.memory)?;
    if write {
        history.rewind()?;
        crate::codec::copy_spool(&mut history.file, &mut output)?;
    }
    Ok(stats)
}

pub(crate) fn inspect_matches<R: Read>(
    input: R,
    header_bytes: Vec<u8>,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<crate::match_ir::InspectedMatches> {
    let archive = spool_archive(input, header_bytes, resources, context)?;
    let parsed = parse_reference_archive(&archive.file, archive.len, resources, context)?;
    if parsed.meta.uncompressed_len > resources.output_limit {
        return Err(Error::output_limit(
            "archive uncompressed length exceeds output limit",
        ));
    }
    let matches = match parsed.header.layout {
        Layout::Index => copy_matches(&parsed.matches, &context.memory)?,
        Layout::Future => future_matches(&parsed, &context.memory)?,
        Layout::Io => reassemble_io(&parsed, &context.memory)?,
    };
    validate_match_sequence(&matches, &parsed.meta, parsed.effective_min_match)?;
    validate_block_representations(
        &parsed.blocks,
        &matches,
        &parsed.meta,
        parsed.header.block_size,
        parsed.header.layout,
    )?;
    let mut validation_history = TempSpool::new(resources, context)?;
    reconstruct(&parsed, &mut validation_history, &context.memory)?;
    let mut owned = BudgetedVec::with_capacity(matches.len(), &context.memory)?;
    for item in &matches {
        owned.push(*item)?;
    }
    Ok(crate::match_ir::InspectedMatches { matches: owned })
}

fn spool_archive<R: Read>(
    mut input: R,
    header: Vec<u8>,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<ArchiveSpool> {
    fs::create_dir_all(&resources.temp_dir).map_err(Error::temp_storage)?;
    let temp = temp_builder(Builder::new())
        .prefix("srep-reference-archive-")
        .tempfile_in(&resources.temp_dir)
        .map_err(Error::temp_storage)?;
    let mut file = temp.reopen().map_err(Error::temp_storage)?;
    let mut reservation = context.temp.reserve(0)?;
    reservation.grow(header.len() as u64)?;
    if let Err(error) = file.write_all(&header) {
        reservation.shrink(header.len() as u64);
        return Err(Error::temp_storage(error));
    }
    let mut buffer = [0u8; 64 * 1024];
    let mut len = header.len() as u64;
    loop {
        let count = input.read(&mut buffer).map_err(Error::input_io)?;
        if count == 0 {
            break;
        }
        let next = len
            .checked_add(count as u64)
            .ok_or_else(|| Error::output_limit("archive length overflows"))?;
        if next > crate::config::MAX_UNCOMPRESSED {
            return Err(Error::output_limit("archive exceeds wire size limit"));
        }
        reservation.grow(count as u64)?;
        if let Err(error) = file.write_all(&buffer[..count]) {
            reservation.shrink(count as u64);
            return Err(Error::temp_storage(error));
        }
        len = next;
    }
    file.flush().map_err(Error::temp_storage)?;
    file.seek(SeekFrom::Start(0)).map_err(Error::temp_storage)?;
    Ok(ArchiveSpool {
        file,
        _temp: temp,
        len,
        _reservation: reservation,
    })
}

fn parse_reference_archive(
    file: &File,
    file_len: u64,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<ParsedArchive> {
    if file_len < (format::HEADER_LEN + TRAILER_LEN) as u64 {
        return Err(Error::truncated("truncated NG v2 archive"));
    }
    let header_bytes = read_range(file, 0, format::HEADER_LEN as u64, &context.memory)?;
    let header = parse_archive_header(&header_bytes)?;
    let trailer_bytes = read_range(
        file,
        file_len - TRAILER_LEN as u64,
        TRAILER_LEN as u64,
        &context.memory,
    )?;
    let trailer = parse_trailer(&trailer_bytes, header.layout, file_len)?;
    let mut offset = format::HEADER_LEN as u64;
    let method_record = read_record_at(
        file,
        offset,
        header.checksum,
        RECORD_METHOD_PARAMETERS,
        64,
        &context.memory,
    )?;
    let method_frame = method_record.frame;
    let method_payload = method_record.payload;
    let method_digest = method_record.digest;
    let method_total = method_record.total;
    let method_parameters = parse_method_parameters(&method_payload, &header)?;
    let effective_min_match = header.effective_min_match(&method_parameters)?;
    verify_ordinary_checksum(
        header.checksum,
        &method_frame,
        &method_payload,
        &method_digest[..header.checksum.width()],
    )?;
    offset = offset
        .checked_add(method_total)
        .ok_or_else(|| Error::corrupt_record("archive offset overflows"))?;
    let layout_record = read_record_at(
        file,
        offset,
        header.checksum,
        RECORD_LAYOUT_METADATA,
        64,
        &context.memory,
    )?;
    let layout_frame = layout_record.frame;
    let layout_payload = layout_record.payload;
    let layout_digest = layout_record.digest;
    let layout_total = layout_record.total;
    let meta = parse_layout_metadata(&layout_payload, &header)?;
    if meta.block_count != expected_block_count(meta.uncompressed_len, header.block_size)? {
        return Err(Error::corrupt_record(
            "block_count does not match uncompressed length and block size",
        ));
    }
    let minimum_block_record = record_total_len(48, header.checksum)?;
    let remaining_after_headers = file_len
        .checked_sub(offset)
        .ok_or_else(|| Error::truncated("truncated NG v2 body"))?;
    if meta.block_count > remaining_after_headers / minimum_block_record {
        return Err(Error::corrupt_record(
            "physical archive cannot contain the declared block count",
        ));
    }
    verify_ordinary_checksum(
        header.checksum,
        &layout_frame,
        &layout_payload,
        &layout_digest[..header.checksum.width()],
    )?;
    offset = offset
        .checked_add(layout_total)
        .ok_or_else(|| Error::corrupt_record("archive offset overflows"))?;
    let directory_offset = if header.layout.is_index() {
        Some(offset)
    } else {
        None
    };
    let data_offset = if let Some(directory_offset) = directory_offset {
        let frame = read_frame_at(file, directory_offset)?;
        let (kind, payload_len) = parse_frame(&frame)?;
        if kind != RECORD_BLOCK_DIRECTORY {
            return Err(Error::corrupt_record("expected BlockDirectory record"));
        }
        let expected = format::directory_payload_len(meta.block_count)?;
        if payload_len != expected {
            return Err(Error::corrupt_index(
                "BlockDirectory payload length mismatch",
            ));
        }
        let total = record_total_len(payload_len, header.checksum)?;
        let end = directory_offset
            .checked_add(total)
            .ok_or_else(|| Error::corrupt_record("directory record offset overflows"))?;
        if end > file_len {
            return Err(Error::truncated("truncated BlockDirectory record"));
        }
        end
    } else {
        offset
    };
    scan_data_block_records(
        file,
        data_offset,
        meta.block_count,
        header.checksum,
        header.layout,
        file_len,
    )?;
    let directory = if let Some(directory_offset) = directory_offset {
        let expected = format::directory_payload_len(meta.block_count)?;
        let record = read_record_at(
            file,
            directory_offset,
            header.checksum,
            RECORD_BLOCK_DIRECTORY,
            expected,
            &context.memory,
        )?;
        verify_ordinary_checksum(
            header.checksum,
            &record.frame,
            &record.payload,
            &record.digest[..header.checksum.width()],
        )?;
        Some(format::parse_block_directory_with_budget(
            &record.payload,
            meta.block_count,
            &context.memory,
        )?)
    } else {
        None
    };
    offset = data_offset;
    let block_capacity = usize::try_from(meta.block_count)
        .map_err(|_| Error::memory_limit("block count exceeds platform limits"))?;
    let mut blocks = BudgetedVec::with_capacity(block_capacity, &context.memory)?;
    for block_id in 0..meta.block_count {
        validate_datablock_structure(file, offset, header.checksum, header.layout, file_len)?;
        let record = read_record_at_any(
            file,
            offset,
            header.checksum,
            RECORD_DATA_BLOCK,
            &context.memory,
        )?;
        let frame = record.frame;
        let payload = record.payload;
        let digest = record.digest;
        let total = record.total;
        let expected_len = block_len_at(meta.uncompressed_len, header.block_size, block_id)?;
        let block = parse_block_payload(
            &payload,
            &header,
            block_id,
            block_id
                .checked_mul(header.block_size)
                .ok_or_else(|| Error::corrupt_record("block start overflows"))?,
            expected_len,
            &context.memory,
        )?;
        let literal_run_count = block.literals.len() as u64;
        let register_count = block.registers.len() as u64;
        let operation_count = block.operations.len() as u64;
        blocks.push(ParsedBlock {
            frame,
            digest,
            payload,
            literals: block.literals,
            operations: block.operations,
            registers: block.registers,
        })?;
        if let Some(entries) = &directory {
            let entry = &entries[block_id as usize];
            if entry.data_record_offset != offset
                || entry.data_record_total_len != total
                || entry.block_id != block_id
                || entry.dst_start
                    != block_id
                        .checked_mul(header.block_size)
                        .ok_or_else(|| Error::corrupt_index("block start overflows"))?
                || entry.uncompressed_len != expected_len
                || entry.literal_run_count != literal_run_count
                || entry.first_starting_match_index > meta.semantic_match_count
                || entry.starting_match_count > meta.semantic_match_count
                || entry
                    .first_starting_match_index
                    .checked_add(entry.starting_match_count)
                    .is_none_or(|end| end > meta.semantic_match_count)
            {
                return Err(Error::corrupt_index(
                    "BlockDirectory entry does not match DataBlock",
                ));
            }
        }
        if block.literal_count != literal_run_count
            || block.operation_count
                != match header.layout {
                    Layout::Index => 0,
                    Layout::Future => register_count,
                    Layout::Io => operation_count,
                }
        {
            return Err(Error::corrupt_record(
                "DataBlock counts do not match payload",
            ));
        }
        offset = offset
            .checked_add(total)
            .ok_or_else(|| Error::corrupt_record("archive offset overflows"))?;
    }
    let (index_offset, index_total, matches) = if header.layout.is_index() {
        let index_offset = offset;
        let (total, matches) = read_index_section_at(
            file,
            offset,
            header.checksum,
            &meta,
            header.block_size,
            effective_min_match,
            &context.memory,
        )?;
        (index_offset, total, matches)
    } else {
        (0, 0, BudgetedVec::new(&context.memory)?)
    };
    if let Some(entries) = &directory {
        let mut prior_end = 0u64;
        for (block_id, entry) in entries.iter().enumerate() {
            let expected_first = prior_end;
            let expected_count = matches
                .iter()
                .filter(|item| item.dst / header.block_size == block_id as u64)
                .count() as u64;
            if entry.first_starting_match_index != expected_first
                || entry.starting_match_count != expected_count
                || entry.first_starting_match_index != prior_end
            {
                return Err(Error::corrupt_index(
                    "BlockDirectory match range does not match IndexSection",
                ));
            }
            prior_end = prior_end
                .checked_add(expected_count)
                .ok_or_else(|| Error::corrupt_index("BlockDirectory range overflows"))?;
        }
        if prior_end != matches.len() as u64 {
            return Err(Error::corrupt_index("BlockDirectory ranges omit matches"));
        }
    }
    let actual_future_registers = blocks.iter().try_fold(0u64, |sum, block| {
        sum.checked_add(block.registers.len() as u64)
            .ok_or_else(|| Error::corrupt_record("Future register count overflows"))
    })?;
    let actual_io_operations = blocks.iter().try_fold(0u64, |sum, block| {
        sum.checked_add(block.operations.len() as u64)
            .ok_or_else(|| Error::corrupt_record("I/O operation count overflows"))
    })?;
    let expected_operations = match header.layout {
        Layout::Index => 0,
        Layout::Future => meta.semantic_match_count,
        Layout::Io => actual_io_operations,
    };
    if meta.encoded_operation_count != expected_operations
        || (header.layout == Layout::Future && actual_future_registers != meta.semantic_match_count)
    {
        return Err(Error::corrupt_record("encoded operation count mismatch"));
    }
    offset = offset
        .checked_add(index_total)
        .ok_or_else(|| Error::corrupt_record("archive offset overflows"))?;
    let summary_offset = offset;
    let expected_summary = (format::SUMMARY_PREFIX_LEN + header.checksum.width()) as u64;
    let summary_record = read_record_at(
        file,
        offset,
        header.checksum,
        RECORD_ARCHIVE_SUMMARY,
        expected_summary,
        &context.memory,
    )?;
    let summary_frame = summary_record.frame;
    let summary_payload = summary_record.payload;
    let summary_digest = summary_record.digest;
    let summary_total = summary_record.total;
    let (_, _, _, summary_semantic) = parse_archive_summary(&summary_payload, &header, &meta)?;
    verify_ordinary_checksum(
        header.checksum,
        &summary_frame,
        &summary_payload,
        &summary_digest[..header.checksum.width()],
    )?;
    let expected_data_bytes = blocks.iter().try_fold(0u64, |sum, block| {
        sum.checked_add(record_total_len(
            block.payload.len() as u64,
            header.checksum,
        )?)
        .ok_or_else(|| Error::corrupt_record("data record bytes overflow"))
    })?;
    let (declared_data_bytes, declared_index_bytes, _, _) =
        parse_archive_summary(&summary_payload, &header, &meta)?;
    if declared_data_bytes != expected_data_bytes || declared_index_bytes != index_total {
        return Err(Error::corrupt_record(
            "ArchiveSummary serialized byte counts do not match records",
        ));
    }
    let body_end = if header.layout.is_index() {
        summary_offset - index_total
    } else {
        summary_offset
    };
    if trailer.summary_offset != summary_offset
        || trailer.summary_total_len != summary_total
        || trailer.index_offset != index_offset
        || trailer.index_total_len != index_total
        || trailer.body_end != body_end
    {
        return Err(Error::corrupt_record(
            "trailer offsets do not match records",
        ));
    }
    if blocks.len() as u64 != expected_block_count(meta.uncompressed_len, header.block_size)? {
        return Err(Error::corrupt_record(
            "DataBlock count does not match metadata",
        ));
    }
    if summary_offset
        .checked_add(summary_total)
        .and_then(|end| end.checked_add(TRAILER_LEN as u64))
        != Some(file_len)
    {
        return Err(Error::corrupt_record("trailer does not cover archive end"));
    }
    if header.checksum.width() > summary_digest.len() {
        return Err(Error::checksum_mismatch("summary checksum width mismatch"));
    }
    if !header.layout.is_index() && (index_offset != 0 || index_total != 0) {
        return Err(Error::corrupt_index(
            "non-Index archive contains IndexSection",
        ));
    }
    let directory_entries = directory;
    let _ = (resources, summary_semantic);
    if let Some(entries) = &directory_entries
        && entries.len() as u64 != meta.block_count
    {
        return Err(Error::corrupt_index("BlockDirectory count mismatch"));
    }
    Ok(ParsedArchive {
        header,
        header_bytes: header_bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::corrupt_header("header length is invalid"))?,
        method_parameters,
        effective_min_match,
        method_payload,
        layout_payload,
        meta,
        blocks,
        matches,
        summary_payload,
        summary_offset,
        summary_total,
    })
}

fn scan_data_block_records(
    file: &File,
    mut offset: u64,
    block_count: u64,
    checksum: Checksum,
    layout: Layout,
    file_len: u64,
) -> Result<()> {
    for _ in 0..block_count {
        let frame = read_frame_at(file, offset)?;
        let (kind, payload_len) = parse_frame(&frame)?;
        if kind != RECORD_DATA_BLOCK {
            return Err(Error::corrupt_record("expected DataBlock record"));
        }
        validate_datablock_payload_header(file, offset, payload_len, checksum, layout, file_len)?;
        offset = offset
            .checked_add(record_total_len(payload_len, checksum)?)
            .ok_or_else(|| Error::corrupt_record("DataBlock scan offset overflows"))?;
        if offset > file_len {
            return Err(Error::truncated("truncated DataBlock record"));
        }
    }
    let tail = read_frame_at(file, offset)?;
    let kind = tail[0];
    let expected = if layout.is_index() {
        RECORD_INDEX_SECTION
    } else {
        RECORD_ARCHIVE_SUMMARY
    };
    if kind != expected {
        return Err(Error::corrupt_record(
            "DataBlock count does not match archive records",
        ));
    }
    Ok(())
}

fn validate_datablock_structure(
    file: &File,
    offset: u64,
    checksum: Checksum,
    layout: Layout,
    file_len: u64,
) -> Result<()> {
    let frame = read_frame_at(file, offset)?;
    let (kind, payload_len) = parse_frame(&frame)?;
    if kind != RECORD_DATA_BLOCK {
        return Err(Error::corrupt_record("expected DataBlock record"));
    }
    validate_datablock_payload_header(file, offset, payload_len, checksum, layout, file_len)
}

fn validate_datablock_payload_header(
    file: &File,
    offset: u64,
    payload_len: u64,
    checksum: Checksum,
    layout: Layout,
    file_len: u64,
) -> Result<()> {
    let payload_start = offset
        .checked_add(FRAME_LEN as u64)
        .ok_or_else(|| Error::corrupt_record("DataBlock payload offset overflows"))?;
    let payload_end = payload_start
        .checked_add(payload_len)
        .and_then(|end| end.checked_add(checksum.width() as u64))
        .ok_or_else(|| Error::corrupt_record("DataBlock record length overflows"))?;
    if payload_end > file_len {
        return Err(Error::truncated("truncated DataBlock record"));
    }
    if payload_len < DATA_BLOCK_HEADER_LEN as u64 {
        return Err(Error::corrupt_record(
            "DataBlock payload shorter than header",
        ));
    }
    let mut fixed = [0u8; DATA_BLOCK_HEADER_LEN];
    let mut input = file.try_clone().map_err(Error::temp_storage)?;
    input
        .seek(SeekFrom::Start(payload_start))
        .map_err(Error::temp_storage)?;
    input
        .read_exact(&mut fixed)
        .map_err(|error| Error::map_eof(error, "truncated DataBlock header"))?;
    if fixed[0..2] != crate::format::SCHEMA_V1.to_le_bytes()
        || fixed[2] != layout.wire_id()
        || fixed[3] != 0
        || fixed[4..8].iter().any(|&byte| byte != 0)
    {
        return Err(Error::corrupt_record("DataBlock common header is invalid"));
    }
    let literal_count = u64::from_le_bytes(fixed[32..40].try_into().unwrap());
    let operation_count = u64::from_le_bytes(fixed[40..48].try_into().unwrap());
    let minimum = match layout {
        Layout::Index => {
            if operation_count != 0 {
                return Err(Error::corrupt_record(
                    "Index DataBlock operation count is invalid",
                ));
            }
            literal_count
                .checked_mul(LITERAL_RUN_HEADER_LEN as u64)
                .and_then(|bytes| (DATA_BLOCK_HEADER_LEN as u64).checked_add(bytes))
        }
        Layout::Future => operation_count
            .checked_mul(FUTURE_REGISTER_LEN as u64)
            .and_then(|bytes| {
                literal_count
                    .checked_mul(LITERAL_RUN_HEADER_LEN as u64)
                    .and_then(|literals| bytes.checked_add(literals))
            })
            .and_then(|bytes| (DATA_BLOCK_HEADER_LEN as u64).checked_add(bytes)),
        Layout::Io => {
            if literal_count > operation_count {
                return Err(Error::corrupt_record(
                    "I/O literal count exceeds operation count",
                ));
            }
            operation_count
                .checked_mul(8)
                .and_then(|bytes| (DATA_BLOCK_HEADER_LEN as u64).checked_add(bytes))
        }
    };
    if payload_len
        < minimum.ok_or_else(|| Error::corrupt_record("DataBlock count arithmetic overflows"))?
    {
        return Err(Error::corrupt_record(
            "DataBlock counts exceed payload length",
        ));
    }
    let payload_body_start = payload_start
        .checked_add(DATA_BLOCK_HEADER_LEN as u64)
        .ok_or_else(|| Error::corrupt_record("DataBlock payload offset overflows"))?;
    scan_datablock_payload(
        file,
        payload_body_start,
        payload_len - DATA_BLOCK_HEADER_LEN as u64,
        layout,
        literal_count,
        operation_count,
    )?;
    Ok(())
}

fn scan_datablock_payload(
    file: &File,
    mut position: u64,
    payload_tail_len: u64,
    layout: Layout,
    literal_count: u64,
    operation_count: u64,
) -> Result<()> {
    let payload_end = position
        .checked_add(payload_tail_len)
        .ok_or_else(|| Error::corrupt_record("DataBlock payload end overflows"))?;
    let mut input = file.try_clone().map_err(Error::temp_storage)?;
    match layout {
        Layout::Index => {
            for _ in 0..literal_count {
                let mut header = [0u8; LITERAL_RUN_HEADER_LEN];
                input
                    .read_exact(&mut header)
                    .map_err(|error| Error::map_eof(error, "truncated LiteralRun header"))?;
                let len = u64::from_le_bytes(header[8..16].try_into().unwrap());
                position = position
                    .checked_add(LITERAL_RUN_HEADER_LEN as u64)
                    .and_then(|value| value.checked_add(len))
                    .ok_or_else(|| Error::corrupt_record("LiteralRun length overflows"))?;
                if position > payload_end {
                    return Err(Error::corrupt_record(
                        "LiteralRun exceeds DataBlock payload",
                    ));
                }
                input
                    .seek(SeekFrom::Start(position))
                    .map_err(Error::temp_storage)?;
            }
        }
        Layout::Future => {
            for _ in 0..operation_count {
                let mut register = [0u8; FUTURE_REGISTER_LEN];
                input
                    .read_exact(&mut register)
                    .map_err(|error| Error::map_eof(error, "truncated FutureRegister"))?;
                position = position
                    .checked_add(FUTURE_REGISTER_LEN as u64)
                    .ok_or_else(|| Error::corrupt_record("FutureRegister cursor overflows"))?;
            }
            for _ in 0..literal_count {
                let mut header = [0u8; LITERAL_RUN_HEADER_LEN];
                input
                    .read_exact(&mut header)
                    .map_err(|error| Error::map_eof(error, "truncated LiteralRun header"))?;
                let len = u64::from_le_bytes(header[8..16].try_into().unwrap());
                position = position
                    .checked_add(LITERAL_RUN_HEADER_LEN as u64)
                    .and_then(|value| value.checked_add(len))
                    .ok_or_else(|| Error::corrupt_record("LiteralRun length overflows"))?;
                if position > payload_end {
                    return Err(Error::corrupt_record(
                        "LiteralRun exceeds DataBlock payload",
                    ));
                }
                input
                    .seek(SeekFrom::Start(position))
                    .map_err(Error::temp_storage)?;
            }
        }
        Layout::Io => {
            for _ in 0..operation_count {
                let mut operation_header = [0u8; 8];
                input
                    .read_exact(&mut operation_header)
                    .map_err(|error| Error::map_eof(error, "truncated I/O operation header"))?;
                let encoded = u32::from_le_bytes(operation_header[4..8].try_into().unwrap()) as u64;
                if encoded < 8 {
                    return Err(Error::corrupt_record("I/O operation length is invalid"));
                }
                position = position
                    .checked_add(encoded)
                    .ok_or_else(|| Error::corrupt_record("I/O operation length overflows"))?;
                if position > payload_end {
                    return Err(Error::corrupt_record(
                        "I/O operation exceeds DataBlock payload",
                    ));
                }
                input
                    .seek(SeekFrom::Start(position))
                    .map_err(Error::temp_storage)?;
            }
        }
    }
    if position != payload_end {
        return Err(Error::corrupt_record(
            "DataBlock payload has trailing bytes",
        ));
    }
    Ok(())
}

fn read_frame_at(file: &File, offset: u64) -> Result<[u8; FRAME_LEN]> {
    let mut input = file.try_clone().map_err(Error::temp_storage)?;
    input
        .seek(SeekFrom::Start(offset))
        .map_err(Error::temp_storage)?;
    let mut frame = [0u8; FRAME_LEN];
    input
        .read_exact(&mut frame)
        .map_err(|error| Error::map_eof(error, "truncated record frame"))?;
    Ok(frame)
}

fn read_record_at(
    file: &File,
    offset: u64,
    checksum: Checksum,
    expected: u8,
    expected_payload: u64,
    memory: &crate::resource::MemoryBudget,
) -> Result<RecordParts> {
    let record = read_record_at_any(file, offset, checksum, expected, memory)?;
    if record.payload.len() as u64 != expected_payload {
        return Err(Error::corrupt_record("record payload length mismatch"));
    }
    Ok(record)
}

fn read_record_at_any(
    file: &File,
    offset: u64,
    checksum: Checksum,
    expected: u8,
    memory: &crate::resource::MemoryBudget,
) -> Result<RecordParts> {
    let mut file = file.try_clone().map_err(Error::temp_storage)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(Error::temp_storage)?;
    let mut frame = [0u8; FRAME_LEN];
    file.read_exact(&mut frame)
        .map_err(|error| Error::map_eof(error, "truncated record frame"))?;
    let (kind, len) = parse_frame(&frame)?;
    if kind != expected {
        return Err(Error::corrupt_record("unexpected record type"));
    }
    let total = record_total_len(len, checksum)?;
    let physical_len = file.metadata().map_err(Error::temp_storage)?.len();
    if offset
        .checked_add(total)
        .ok_or_else(|| Error::corrupt_record("record end overflows"))?
        > physical_len
    {
        return Err(Error::truncated("truncated record payload or checksum"));
    }
    let payload_len = usize::try_from(len)
        .map_err(|_| Error::memory_limit("record payload exceeds platform limits"))?;
    let mut payload = BudgetedVec::with_capacity(payload_len, memory)?;
    payload.resize(payload_len, 0)?;
    file.read_exact(payload.as_mut_slice())
        .map_err(|error| Error::map_eof(error, "truncated record payload"))?;
    let mut digest = [0u8; 64];
    file.read_exact(&mut digest[..checksum.width()])
        .map_err(|error| Error::map_eof(error, "truncated record checksum"))?;
    Ok(RecordParts {
        frame,
        payload,
        digest,
        total,
    })
}

fn read_index_section_at(
    file: &File,
    offset: u64,
    checksum: Checksum,
    meta: &LayoutMetadata,
    block_size: u64,
    effective_min_match: u64,
    memory: &crate::resource::MemoryBudget,
) -> Result<(u64, BudgetedVec<Match>)> {
    let mut input = file.try_clone().map_err(Error::temp_storage)?;
    input
        .seek(SeekFrom::Start(offset))
        .map_err(Error::temp_storage)?;
    let mut frame = [0u8; FRAME_LEN];
    input
        .read_exact(&mut frame)
        .map_err(|error| Error::map_eof(error, "truncated IndexSection frame"))?;
    let raw_payload_len = u64::from_le_bytes(frame[4..12].try_into().unwrap());
    if raw_payload_len == u64::MAX {
        return Err(Error::invalid_match(
            "IndexSection payload length is not a valid match index length",
        ));
    }
    let (kind, payload_len) = parse_frame(&frame)?;
    if kind != RECORD_INDEX_SECTION {
        return Err(Error::corrupt_record("expected IndexSection record"));
    }
    if payload_len < format::INDEX_SECTION_HEADER_LEN as u64 {
        return Err(Error::corrupt_index("IndexSection header is truncated"));
    }
    let physical_len = input.metadata().map_err(Error::temp_storage)?.len();
    let frame_end = offset
        .checked_add(record_total_len(payload_len, checksum)?)
        .ok_or_else(|| Error::corrupt_record("IndexSection end overflows"))?;
    if frame_end > physical_len {
        return Err(Error::truncated("truncated IndexSection record"));
    }
    let mut digest = record_digest_start(checksum, &frame);
    let mut header = [0u8; format::INDEX_SECTION_HEADER_LEN];
    input
        .read_exact(&mut header)
        .map_err(|error| Error::map_eof(error, "truncated IndexSection header"))?;
    digest.update(&header);
    if header[0..2] != format::SCHEMA_V1.to_le_bytes()
        || header[2..4] != (format::INDEX_MATCH_ENTRY_LEN as u16).to_le_bytes()
        || header[4..6] != (format::INDEX_RANGE_ENTRY_LEN as u16).to_le_bytes()
        || header[6..8].iter().any(|&byte| byte != 0)
        || header[24..32].iter().any(|&byte| byte != 0)
    {
        return Err(Error::corrupt_index("IndexSection header is invalid"));
    }
    let match_count = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let block_count = u64::from_le_bytes(header[16..24].try_into().unwrap());
    if match_count != meta.semantic_match_count {
        return Err(Error::corrupt_index(
            "IndexSection match_count does not match LayoutMetadata",
        ));
    }
    if block_count != meta.block_count {
        return Err(Error::corrupt_index(
            "IndexSection block_count does not match LayoutMetadata",
        ));
    }
    let expected_payload = format::index_section_payload_len(match_count, block_count)?;
    if payload_len != expected_payload {
        return Err(Error::corrupt_index(
            "IndexSection frame payload does not match embedded counts",
        ));
    }
    let count = usize::try_from(match_count)
        .map_err(|_| Error::memory_limit("IndexSection match count exceeds platform limits"))?;
    let mut matches = BudgetedVec::with_capacity(count, memory)?;
    let mut entry = [0u8; format::INDEX_MATCH_ENTRY_LEN];
    for id in 0..match_count {
        input
            .read_exact(&mut entry)
            .map_err(|error| Error::map_eof(error, "truncated IndexSection match entry"))?;
        digest.update(&entry);
        let src = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let dst = u64::from_le_bytes(entry[8..16].try_into().unwrap());
        let len = u64::from_le_bytes(entry[16..24].try_into().unwrap());
        if len < effective_min_match {
            return Err(Error::invalid_match(
                "IndexSection match is shorter than effective minimum",
            ));
        }
        matches.push(Match {
            src,
            dst,
            len,
            origin_match_id: id,
        })?;
    }
    let mut previous_first = 0u64;
    let mut range = [0u8; format::INDEX_RANGE_ENTRY_LEN];
    for block_id in 0..block_count {
        input
            .read_exact(&mut range)
            .map_err(|error| Error::map_eof(error, "truncated IndexSection range entry"))?;
        digest.update(&range);
        let actual_block = u64::from_le_bytes(range[0..8].try_into().unwrap());
        let first = u64::from_le_bytes(range[8..16].try_into().unwrap());
        let count = u64::from_le_bytes(range[16..24].try_into().unwrap());
        if actual_block != block_id || first != previous_first {
            return Err(Error::corrupt_index(
                "IndexSection ranges are not contiguous",
            ));
        }
        let end = first
            .checked_add(count)
            .ok_or_else(|| Error::corrupt_index("IndexSection range overflows"))?;
        if end > match_count {
            return Err(Error::corrupt_index("IndexSection range exceeds matches"));
        }
        let first_index = usize::try_from(first)
            .map_err(|_| Error::corrupt_index("IndexSection range exceeds platform limits"))?;
        let end_index = usize::try_from(end)
            .map_err(|_| Error::corrupt_index("IndexSection range exceeds platform limits"))?;
        let owned = matches
            .as_slice()
            .get(first_index..end_index)
            .ok_or_else(|| Error::corrupt_index("IndexSection range is outside matches"))?;
        for item in owned {
            if item.dst / block_size != block_id {
                return Err(Error::corrupt_index("IndexSection range owns wrong match"));
            }
        }
        previous_first = end;
    }
    if previous_first != match_count {
        return Err(Error::corrupt_index(
            "IndexSection ranges omit match entries",
        ));
    }
    let mut actual_digest = [0u8; 32];
    input
        .read_exact(&mut actual_digest[..checksum.width()])
        .map_err(|error| Error::map_eof(error, "truncated IndexSection checksum"))?;
    verify_bytes(
        checksum,
        &digest.finalize(),
        &actual_digest[..checksum.width()],
        "IndexSection checksum mismatch",
    )?;
    Ok((record_total_len(payload_len, checksum)?, matches))
}

fn parse_block_payload(
    payload: &[u8],
    header: &ArchiveHeader,
    id: u64,
    start: u64,
    len: u64,
    memory: &crate::resource::MemoryBudget,
) -> Result<BlockParts> {
    if payload.len() < 48 {
        return Err(Error::corrupt_record("truncated DataBlock"));
    }
    let common = payload
        .get(..48)
        .ok_or_else(|| Error::corrupt_record("truncated DataBlock header"))?;
    if common[0..2] != format::SCHEMA_V1.to_le_bytes()
        || common[3] != 0
        || common[4..8].iter().any(|&byte| byte != 0)
    {
        return Err(Error::corrupt_record(
            "DataBlock header flags or schema are invalid",
        ));
    }
    let block_id = read_u64(common, 8)?;
    let dst_start = read_u64(common, 16)?;
    let block_len = read_u64(common, 24)?;
    let literal_count = read_u64(common, 32)?;
    let operation_count = read_u64(common, 40)?;
    if block_id != id
        || dst_start != start
        || block_len != len
        || common[2] != header.layout.wire_id()
    {
        return Err(Error::corrupt_record("DataBlock identity mismatch"));
    }
    let mut cursor = 48usize;
    let expected_prefix = match header.layout {
        Layout::Index => 0u64,
        Layout::Future => operation_count,
        Layout::Io => operation_count,
    };
    let mut registers = if header.layout == Layout::Future {
        BudgetedVec::with_capacity(
            usize::try_from(operation_count)
                .map_err(|_| Error::memory_limit("operation count exceeds platform limits"))?,
            memory,
        )?
    } else {
        BudgetedVec::new(memory)?
    };
    if header.layout == Layout::Future {
        let required = operation_count
            .checked_mul(40)
            .and_then(|n| n.checked_add(cursor as u64))
            .ok_or_else(|| Error::corrupt_record("Future register payload overflows"))?;
        if required > payload.len() as u64 {
            return Err(Error::corrupt_record("Future registers exceed payload"));
        }
        for _ in 0..operation_count {
            let register_end = cursor
                .checked_add(40)
                .ok_or_else(|| Error::corrupt_record("Future register cursor overflows"))?;
            if payload.get(cursor..register_end).is_none() {
                return Err(Error::corrupt_record("truncated FutureRegister"));
            }
            let entry = payload
                .get(
                    cursor
                        ..cursor.checked_add(40).ok_or_else(|| {
                            Error::corrupt_record("Future register cursor overflows")
                        })?,
                )
                .ok_or_else(|| Error::corrupt_record("truncated FutureRegister"))?;
            let values = [
                read_u64(entry, 0)?,
                read_u64(entry, 8)?,
                read_u64(entry, 16)?,
                read_u64(entry, 24)?,
                read_u64(entry, 32)?,
            ];
            registers.push((values[0], values[1], values[2], values[3], values[4]))?;
            cursor = cursor
                .checked_add(40)
                .ok_or_else(|| Error::corrupt_record("Future register cursor overflows"))?;
        }
    }
    let literal_capacity = usize::try_from(literal_count)
        .map_err(|_| Error::memory_limit("literal count exceeds platform limits"))?;
    let mut literals = BudgetedVec::with_capacity(literal_capacity, memory)?;
    let mut literal_operation_count = 0u64;
    let mut previous_literal_end = None;
    let mut operations = if header.layout == Layout::Io {
        BudgetedVec::with_capacity(
            usize::try_from(operation_count)
                .map_err(|_| Error::memory_limit("operation count exceeds platform limits"))?,
            memory,
        )?
    } else {
        BudgetedVec::new(memory)?
    };
    if header.layout == Layout::Io {
        let mut destination = 0u64;
        for _ in 0..operation_count {
            let header_end = cursor
                .checked_add(8)
                .ok_or_else(|| Error::corrupt_record("I/O operation cursor overflows"))?;
            if payload.get(cursor..header_end).is_none() {
                return Err(Error::corrupt_record("truncated I/O operation"));
            }
            let operation_header = payload
                .get(cursor..header_end)
                .ok_or_else(|| Error::corrupt_record("truncated I/O operation header"))?;
            let tag = operation_header[0];
            if operation_header[1] != 0 || operation_header[2] != 0 || operation_header[3] != 0 {
                return Err(Error::corrupt_record("I/O operation flags are nonzero"));
            }
            let encoded = u32::from_le_bytes(operation_header[4..8].try_into().unwrap()) as usize;
            let operation_end = cursor
                .checked_add(encoded)
                .ok_or_else(|| Error::corrupt_record("I/O operation cursor overflows"))?;
            if encoded < 8 || payload.get(cursor..operation_end).is_none() {
                return Err(Error::corrupt_record("I/O operation length is invalid"));
            }
            if tag == 0 {
                if encoded < 16 {
                    return Err(Error::corrupt_record("I/O literal is truncated"));
                }
                let operation = payload
                    .get(cursor..operation_end)
                    .ok_or_else(|| Error::corrupt_record("truncated I/O literal operation"))?;
                let literal_len = read_u64(operation, 8)?;
                let expected_encoded = 16u64
                    .checked_add(literal_len)
                    .ok_or_else(|| Error::corrupt_record("I/O literal length overflows"))?;
                if literal_len == 0 || encoded as u64 != expected_encoded {
                    return Err(Error::corrupt_record("I/O literal length mismatch"));
                }
                let literal_start = cursor
                    .checked_add(16)
                    .ok_or_else(|| Error::corrupt_record("I/O literal cursor overflows"))?;
                let literal_range = literal_start..operation_end;
                if let Some(Operation::Literal(previous_offset, previous_len)) = operations.last()
                    && previous_offset
                        .checked_add(*previous_len)
                        .is_some_and(|end| end == destination)
                {
                    return Err(Error::corrupt_record("adjacent I/O literal operations"));
                }
                literals.push(LiteralRef {
                    offset: destination,
                    bytes: literal_range,
                })?;
                literal_operation_count += 1;
                operations.push(Operation::Literal(destination, literal_len))?;
                destination = destination
                    .checked_add(literal_len)
                    .ok_or_else(|| Error::corrupt_record("I/O destination overflows"))?;
            } else if tag == 1 {
                if encoded != 40 {
                    return Err(Error::corrupt_record("I/O match length mismatch"));
                }
                let operation = payload
                    .get(cursor..operation_end)
                    .ok_or_else(|| Error::corrupt_record("truncated I/O match operation"))?;
                let values = [
                    read_u64(operation, 8)?,
                    read_u64(operation, 16)?,
                    read_u64(operation, 24)?,
                    read_u64(operation, 32)?,
                ];
                let expected_destination = start
                    .checked_add(destination)
                    .ok_or_else(|| Error::invalid_match("I/O destination overflows"))?;
                if values[3] == 0 || values[2] != expected_destination {
                    return Err(Error::corrupt_record(
                        "I/O match destination is not ordered",
                    ));
                }
                operations.push(Operation::Match(values[0], values[1], values[2], values[3]))?;
                destination = destination
                    .checked_add(values[3])
                    .ok_or_else(|| Error::corrupt_record("I/O destination overflows"))?;
            } else {
                return Err(Error::corrupt_record("unknown I/O operation tag"));
            }
            cursor = operation_end;
        }
    } else {
        for _ in 0..literal_count {
            let header_end = cursor
                .checked_add(16)
                .ok_or_else(|| Error::corrupt_record("LiteralRun cursor overflows"))?;
            if payload.get(cursor..header_end).is_none() {
                return Err(Error::corrupt_record("truncated LiteralRun"));
            }
            let literal_header = payload
                .get(cursor..header_end)
                .ok_or_else(|| Error::corrupt_record("truncated LiteralRun"))?;
            let offset = read_u64(literal_header, 0)?;
            let literal_len = read_u64(literal_header, 8)?;
            let begin = cursor
                .checked_add(16)
                .ok_or_else(|| Error::corrupt_record("LiteralRun cursor overflows"))?;
            let end = begin
                .checked_add(
                    usize::try_from(literal_len)
                        .map_err(|_| Error::memory_limit("literal exceeds platform limits"))?,
                )
                .ok_or_else(|| Error::corrupt_record("literal length overflows"))?;
            if end > payload.len()
                || literal_len == 0
                || offset
                    .checked_add(literal_len)
                    .ok_or_else(|| Error::corrupt_record("literal endpoint overflows"))?
                    > len
            {
                return Err(Error::corrupt_record("LiteralRun is outside block"));
            }
            if previous_literal_end.is_some_and(|end| offset <= end) {
                return Err(Error::corrupt_record("LiteralRuns are not ordered"));
            }
            literals.push(LiteralRef {
                offset,
                bytes: begin..end,
            })?;
            previous_literal_end = Some(
                offset
                    .checked_add(literal_len)
                    .ok_or_else(|| Error::corrupt_record("LiteralRun endpoint overflows"))?,
            );
            cursor = end;
        }
    }
    if header.layout != Layout::Io && expected_prefix != operation_count {
        return Err(Error::corrupt_record(
            "DataBlock operation count is invalid",
        ));
    }
    if cursor != payload.len() {
        return Err(Error::corrupt_record("trailing DataBlock payload bytes"));
    }
    if header.layout == Layout::Io && literal_operation_count != literal_count {
        return Err(Error::corrupt_record(
            "I/O literal count does not match operations",
        ));
    }
    if header.layout == Layout::Io {
        let end = operations_end(&operations);
        if end != len {
            return Err(Error::corrupt_record(
                "I/O operations do not cover the block",
            ));
        }
    }
    Ok(BlockParts {
        literals,
        operations,
        registers,
        literal_count,
        operation_count,
    })
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| Error::corrupt_record("u64 field offset overflows"))?;
    let field = bytes
        .get(offset..end)
        .ok_or_else(|| Error::corrupt_record("truncated u64 field"))?;
    Ok(u64::from_le_bytes(field.try_into().unwrap()))
}

fn operations_end(operations: &[Operation]) -> u64 {
    operations
        .iter()
        .map(|operation| match operation {
            Operation::Literal(_, len) => *len,
            Operation::Match(_, _, _, len) => *len,
        })
        .sum()
}

fn validate_block_representations(
    blocks: &[ParsedBlock],
    matches: &[Match],
    meta: &LayoutMetadata,
    block_size: u64,
    layout: Layout,
) -> Result<()> {
    for (block_id, block) in blocks.iter().enumerate() {
        let start = (block_id as u64)
            .checked_mul(block_size)
            .ok_or_else(|| Error::corrupt_record("block start overflows"))?;
        let end = start
            .checked_add(block_len_at(
                meta.uncompressed_len,
                block_size,
                block_id as u64,
            )?)
            .ok_or_else(|| Error::corrupt_record("block end overflows"))?;
        validate_literal_stream(start, end, matches, LiteralRanges::new(block, layout))?;
        if layout == Layout::Future {
            for &(id, offset, dst, len, period) in &block.registers {
                let source = start
                    .checked_add(offset)
                    .ok_or_else(|| Error::invalid_match("Future source overflows"))?;
                let item = matches
                    .get(id as usize)
                    .ok_or_else(|| Error::invalid_match("Future origin ID is out of range"))?;
                if item.src != source
                    || item.dst != dst
                    || item.len != len
                    || period != len.min(dst - source)
                {
                    return Err(Error::invalid_match(
                        "Future register does not identify its canonical match",
                    ));
                }
            }
        }
    }
    Ok(())
}

struct LiteralRanges<'a> {
    block: &'a ParsedBlock,
    layout: Layout,
    index: usize,
}

impl<'a> LiteralRanges<'a> {
    fn new(block: &'a ParsedBlock, layout: Layout) -> Self {
        Self {
            block,
            layout,
            index: 0,
        }
    }
}

impl Iterator for LiteralRanges<'_> {
    type Item = (u64, u64);

    fn next(&mut self) -> Option<Self::Item> {
        match self.layout {
            Layout::Io => {
                while let Some(operation) = self.block.operations.get(self.index) {
                    self.index += 1;
                    if let Operation::Literal(offset, len) = operation {
                        return Some((*offset, *len));
                    }
                }
                None
            }
            Layout::Index | Layout::Future => {
                let literal = self.block.literals.get(self.index)?;
                self.index += 1;
                Some((
                    literal.offset,
                    (literal.bytes.end - literal.bytes.start) as u64,
                ))
            }
        }
    }
}

fn validate_literal_stream<I>(start: u64, end: u64, matches: &[Match], mut actual: I) -> Result<()>
where
    I: Iterator<Item = (u64, u64)>,
{
    let mut cursor = start;
    for item in matches {
        let item_end = item
            .dst
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("match endpoint overflows"))?;
        if item_end <= start || item.dst >= end {
            continue;
        }
        let match_start = item.dst.max(start);
        if match_start > cursor && actual.next() != Some((cursor - start, match_start - cursor)) {
            return Err(Error::corrupt_record(
                "literal runs are not the maximal uncovered intervals",
            ));
        }
        cursor = cursor.max(item_end.min(end));
    }
    if cursor < end && actual.next() != Some((cursor - start, end - cursor)) {
        return Err(Error::corrupt_record(
            "literal runs are not the maximal uncovered intervals",
        ));
    }
    if actual.next().is_some() {
        return Err(Error::corrupt_record(
            "literal runs contain extra intervals",
        ));
    }
    Ok(())
}

fn reconstruct(
    parsed: &ParsedArchive,
    history: &mut TempSpool,
    memory: &crate::resource::MemoryBudget,
) -> Result<CompressionStats> {
    let effective_min_match = parsed
        .header
        .effective_min_match(&parsed.method_parameters)?;
    if effective_min_match != parsed.effective_min_match {
        return Err(Error::corrupt_header(
            "effective minimum does not match MethodParameters",
        ));
    }
    let owned_matches = match parsed.header.layout {
        Layout::Future => Some(future_matches(parsed, memory)?),
        Layout::Io => Some(reassemble_io(parsed, memory)?),
        Layout::Index => None,
    };
    let matches: &[Match] = owned_matches.as_deref().unwrap_or(&parsed.matches);
    validate_match_sequence(matches, &parsed.meta, effective_min_match)?;
    let mut digest = archive_digest_start(
        parsed.header.checksum,
        &parsed.header_bytes,
        &parsed.method_payload,
        &parsed.layout_payload,
    );
    let mut covered = 0u64;
    for item in matches {
        covered = covered
            .checked_add(item.len)
            .ok_or_else(|| Error::corrupt_record("coverage overflows"))?;
    }
    if covered != parsed.meta.covered_bytes
        || matches.len() as u64 != parsed.meta.semantic_match_count
    {
        return Err(Error::corrupt_record("semantic counters do not match"));
    }
    let mut total_data = 0u64;
    for (id, block) in parsed.blocks.iter().enumerate() {
        let start = id as u64 * parsed.header.block_size;
        let len = block_len_at(
            parsed.meta.uncompressed_len,
            parsed.header.block_size,
            id as u64,
        )?;
        let block_len_usize = usize::try_from(len)
            .map_err(|_| Error::memory_limit("reconstructed block exceeds platform limits"))?;
        let mut bytes = BudgetedVec::with_capacity(block_len_usize, memory)?;
        bytes.resize(block_len_usize, 0)?;
        let mut filled = BudgetedVec::with_capacity(block_len_usize, memory)?;
        filled.resize(block_len_usize, 0u8)?;
        for literal in &block.literals {
            let begin = usize::try_from(literal.offset)
                .map_err(|_| Error::corrupt_record("literal offset exceeds platform limits"))?;
            let literal_bytes = block
                .payload
                .as_slice()
                .get(literal.bytes.clone())
                .ok_or_else(|| Error::corrupt_record("literal bytes are outside payload"))?;
            let end = begin
                .checked_add(literal_bytes.len())
                .ok_or_else(|| Error::corrupt_record("literal endpoint overflows"))?;
            if end > bytes.len()
                || filled.as_slice()[begin..end]
                    .iter()
                    .any(|&value| value != 0)
            {
                return Err(Error::corrupt_record("literal coverage overlaps"));
            }
            bytes.as_mut_slice()[begin..end].copy_from_slice(literal_bytes);
            filled.as_mut_slice()[begin..end].fill(1);
        }
        for item in matches {
            let item_end = item
                .dst
                .checked_add(item.len)
                .ok_or_else(|| Error::invalid_match("match destination overflows"))?;
            let overlap_start = item.dst.max(start);
            let block_end = start
                .checked_add(len)
                .ok_or_else(|| Error::invalid_match("block end overflows"))?;
            let overlap_end = item_end.min(block_end);
            if overlap_start >= overlap_end {
                continue;
            }
            for dst in overlap_start..overlap_end {
                let local = (dst - start) as usize;
                if filled[local] != 0 {
                    return Err(Error::invalid_match("destination coverage overlaps"));
                }
                let distance = item
                    .dst
                    .checked_sub(item.src)
                    .ok_or_else(|| Error::invalid_match("match distance underflows"))?;
                let source = item
                    .src
                    .checked_add((dst - item.dst) % distance)
                    .ok_or_else(|| Error::invalid_match("match source overflows"))?;
                if source >= dst {
                    return Err(Error::invalid_match("match source is not available"));
                }
                let mut byte = [0u8; 1];
                if source >= start && source < block_end {
                    let source_local = usize::try_from(source - start)
                        .map_err(|_| Error::memory_limit("match source exceeds platform limits"))?;
                    if filled[source_local] == 0 {
                        return Err(Error::invalid_match("match source is not yet available"));
                    }
                    byte[0] = bytes[source_local];
                } else {
                    let write_position = history
                        .file
                        .stream_position()
                        .map_err(Error::temp_storage)?;
                    let mut file = history.file.try_clone().map_err(Error::temp_storage)?;
                    file.seek(SeekFrom::Start(source))
                        .map_err(Error::temp_storage)?;
                    file.read_exact(&mut byte)
                        .map_err(|error| Error::map_eof(error, "match source is unavailable"))?;
                    history
                        .file
                        .seek(SeekFrom::Start(write_position))
                        .map_err(Error::temp_storage)?;
                }
                bytes[local] = byte[0];
                filled[local] = 1;
            }
        }
        if filled.iter().any(|&value| value == 0) {
            return Err(Error::corrupt_record("block has a literal or match gap"));
        }
        let frame_payload = &block.payload;
        verify_block_checksum(
            parsed.header.checksum,
            &block.frame,
            frame_payload,
            id as u64,
            start,
            &bytes,
            &block.digest[..parsed.header.checksum.width()],
        )?;
        history.append(&bytes)?;
        digest.update(&bytes);
        total_data += record_total_len(frame_payload.len() as u64, parsed.header.checksum)?;
    }
    verify_bytes(
        parsed.header.checksum,
        &digest.finalize(),
        parsed
            .summary_payload
            .as_slice()
            .get(format::SUMMARY_PREFIX_LEN..)
            .ok_or_else(|| Error::corrupt_record("ArchiveSummary digest is truncated"))?,
        "ArchiveSummary semantic digest mismatch",
    )?;
    if history.file.metadata().map_err(Error::temp_storage)?.len() != parsed.meta.uncompressed_len {
        return Err(Error::corrupt_record(
            "history length does not match metadata",
        ));
    }
    Ok(CompressionStats {
        original_size: parsed.meta.uncompressed_len,
        archive_size: parsed
            .summary_offset
            .checked_add(parsed.summary_total)
            .and_then(|end| end.checked_add(TRAILER_LEN as u64))
            .ok_or_else(|| Error::corrupt_record("archive length overflows"))?,
        payload_size: total_data,
        block_count: parsed.meta.block_count,
        compressed_blocks: parsed.meta.block_count,
        reference_count: matches.len() as u64,
        semantic_match_count: matches.len() as u64,
        covered_bytes: parsed.meta.covered_bytes,
        literal_bytes: parsed.meta.literal_bytes,
        method: Some(parsed.header.method),
        layout: Some(parsed.header.layout),
        checksum: Some(parsed.header.checksum),
    })
}

fn block_start_from_payload(block: &ParsedBlock) -> Result<u64> {
    if block.payload.len() < 24 {
        return Err(Error::corrupt_record("truncated DataBlock header"));
    }
    read_u64(&block.payload, 16)
}

fn copy_matches(
    source: &[Match],
    memory: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<Match>> {
    let mut result = BudgetedVec::with_capacity(source.len(), memory)?;
    for item in source {
        result.push(*item)?;
    }
    Ok(result)
}

fn future_matches(
    parsed: &ParsedArchive,
    memory: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<Match>> {
    let count = usize::try_from(parsed.meta.semantic_match_count)
        .map_err(|_| Error::memory_limit("Future origin count exceeds platform limits"))?;
    let mut by_id = BudgetedVec::with_capacity(count, memory)?;
    by_id.resize(count, None)?;
    for block in &parsed.blocks {
        let block_start = block_start_from_payload(block)?;
        let block_len = read_u64(&block.payload, 24)?;
        let mut previous_key = None;
        for &(id, offset, dst, len, period) in &block.registers {
            let key = (offset, dst, id);
            if previous_key.is_some_and(|previous| previous > key) {
                return Err(Error::invalid_match(
                    "Future registers are not source ordered",
                ));
            }
            previous_key = Some(key);
            if id as usize >= by_id.len() || by_id[id as usize].is_some() {
                return Err(Error::invalid_match(
                    "duplicate or out-of-range Future origin ID",
                ));
            }
            let src = block_start
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("Future source overflows"))?;
            if offset >= block_len
                || src >= parsed.meta.uncompressed_len
                || dst <= src
                || dst
                    .checked_add(len)
                    .ok_or_else(|| Error::invalid_match("Future destination overflows"))?
                    > parsed.meta.uncompressed_len
            {
                return Err(Error::invalid_match("Future register interval is invalid"));
            }
            if period != len.min(dst - src) {
                return Err(Error::invalid_match(
                    "Future period does not match match distance",
                ));
            }
            if len < parsed.effective_min_match {
                return Err(Error::invalid_match(
                    "Future match is shorter than effective minimum",
                ));
            }
            by_id[id as usize] = Some(Match {
                src,
                dst,
                len,
                origin_match_id: id,
            });
        }
    }
    if by_id.iter().any(Option::is_none) {
        return Err(Error::invalid_match("missing Future origin ID"));
    }
    let mut output = BudgetedVec::with_capacity(count, memory)?;
    for item in &by_id {
        output.push(item.ok_or_else(|| Error::invalid_match("missing Future origin ID"))?)?;
    }
    Ok(output)
}

fn validate_match_sequence(matches: &[Match], meta: &LayoutMetadata, min_match: u64) -> Result<()> {
    if matches.len() as u64 != meta.semantic_match_count {
        return Err(Error::invalid_match("match count mismatch"));
    }
    let mut previous_end = 0u64;
    for (id, item) in matches.iter().enumerate() {
        let destination_end = item
            .dst
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("match destination overflows"))?;
        if item.origin_match_id != id as u64
            || item.src >= item.dst
            || item.len == 0
            || item.len < min_match
            || destination_end > meta.uncompressed_len
            || item
                .src
                .checked_add(item.len.min(item.dst - item.src))
                .is_none_or(|end| end > meta.uncompressed_len)
            || (id > 0 && item.dst < previous_end)
        {
            return Err(Error::invalid_match("noncanonical match sequence"));
        }
        previous_end = destination_end;
    }
    Ok(())
}

fn reassemble_io(
    parsed: &ParsedArchive,
    memory: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<Match>> {
    let count = usize::try_from(parsed.meta.semantic_match_count)
        .map_err(|_| Error::memory_limit("I/O origin count exceeds platform limits"))?;
    let mut results: BudgetedVec<Option<Match>> = BudgetedVec::with_capacity(count, memory)?;
    results.resize(count, None)?;
    let mut closed = BudgetedVec::with_capacity(count, memory)?;
    closed.resize(count, 0u8)?;
    let mut last_origin = None;
    let mut may_continue = None::<(u64, u64, u64)>;
    for (block_id, block) in parsed.blocks.iter().enumerate() {
        let start = (block_id as u64)
            .checked_mul(parsed.header.block_size)
            .ok_or_else(|| Error::invalid_match("I/O block start overflows"))?;
        let block_end = start
            + block_len_at(
                parsed.meta.uncompressed_len,
                parsed.header.block_size,
                block_id as u64,
            )?;
        for operation in &block.operations {
            if let Operation::Match(id, src, dst, len) = *operation {
                let slot = results
                    .get_mut(id as usize)
                    .ok_or_else(|| Error::invalid_match("I/O origin ID is out of range"))?;
                let continuing =
                    if let Some((pending_id, source_end, destination_end)) = may_continue.take() {
                        if id == pending_id && src == source_end && dst == destination_end {
                            true
                        } else {
                            closed[pending_id as usize] = 1;
                            false
                        }
                    } else {
                        false
                    };
                if closed[id as usize] != 0 {
                    return Err(Error::invalid_match("I/O origin appears after closure"));
                }
                if len == 0
                    || closed[id as usize] != 0
                    || dst < start
                    || dst
                        .checked_add(len)
                        .ok_or_else(|| Error::invalid_match("I/O fragment endpoint overflows"))?
                        > start
                            + block_len_at(
                                parsed.meta.uncompressed_len,
                                parsed.header.block_size,
                                block_id as u64,
                            )?
                {
                    return Err(Error::invalid_match("invalid I/O match fragment"));
                }
                if let Some(previous) = slot {
                    if !continuing
                        || last_origin != Some(id)
                            && previous.dst.checked_add(previous.len).ok_or_else(|| {
                                Error::invalid_match("I/O fragment endpoint overflows")
                            })? != start
                    {
                        return Err(Error::invalid_match("I/O match fragments are interleaved"));
                    }
                    if previous
                        .src
                        .checked_add(previous.len)
                        .ok_or_else(|| Error::invalid_match("I/O source endpoint overflows"))?
                        != src
                        || previous.dst.checked_add(previous.len).ok_or_else(|| {
                            Error::invalid_match("I/O destination endpoint overflows")
                        })? != dst
                    {
                        return Err(Error::invalid_match("I/O fragments are not contiguous"));
                    }
                    previous.len = previous
                        .len
                        .checked_add(len)
                        .ok_or_else(|| Error::invalid_match("I/O match length overflows"))?;
                } else {
                    *slot = Some(Match {
                        src,
                        dst,
                        len,
                        origin_match_id: id,
                    });
                }
                let fragment_end = dst
                    .checked_add(len)
                    .ok_or_else(|| Error::invalid_match("I/O fragment endpoint overflows"))?;
                if fragment_end < block_end {
                    closed[id as usize] = 1;
                    may_continue = None;
                } else {
                    let source_end = src
                        .checked_add(len)
                        .ok_or_else(|| Error::invalid_match("I/O source endpoint overflows"))?;
                    may_continue = Some((id, source_end, fragment_end));
                }
                last_origin = Some(id);
            } else if let Some((pending_id, _, _)) = may_continue.take() {
                closed[pending_id as usize] = 1;
            }
        }
    }
    if let Some((pending_id, _, _)) = may_continue {
        closed[pending_id as usize] = 1;
    }
    if results.iter().any(Option::is_none) {
        return Err(Error::invalid_match("missing I/O origin"));
    }
    let mut output = BudgetedVec::with_capacity(count, memory)?;
    for item in &results {
        output.push(item.ok_or_else(|| Error::invalid_match("missing I/O origin"))?)?;
    }
    Ok(output)
}

#[cfg(test)]
mod validation_tests {
    use std::io::Cursor;

    use super::*;
    use crate::codec::spool_input;
    use crate::config::ResourceConfig;

    fn candidate(src: u64, dst: u64, len: u64, ordinal: u64) -> MatchCandidate {
        MatchCandidate {
            src,
            dst,
            len,
            insertion_ordinal: ordinal,
        }
    }

    #[test]
    fn validation_canonicalizes_interleaved_duplicate_triples_and_keeps_minimum_ordinal() {
        let input = b"0123456789abcdef".repeat(1024);
        let resources = ResourceConfig::default();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let spool = spool_input(Cursor::new(&input), &resources, &context).unwrap();
        let mut candidates = BudgetedVec::new(&context.memory).unwrap();
        for item in [
            candidate(0, 4096, 8192, 91),
            candidate(0, 4096, 8192, 7),
            candidate(0, 4096, 8192, 43),
        ] {
            candidates.push(item).unwrap();
        }

        validate_candidates(&spool, &mut candidates, 512).unwrap();

        assert_eq!(candidates.as_slice(), &[candidate(0, 4096, 8192, 7)]);
    }

    #[test]
    fn validation_chunked_periodic_comparison_is_exact_for_large_distances() {
        let block: Vec<u8> = (0..8192).map(|value| (value * 17) as u8).collect();
        let input = [block.as_slice(), block.as_slice()].concat();
        let resources = ResourceConfig::default();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let spool = spool_input(Cursor::new(&input), &resources, &context).unwrap();
        let mut candidates = BudgetedVec::new(&context.memory).unwrap();
        candidates.push(candidate(0, 8192, 8192, 0)).unwrap();

        validate_candidates(&spool, &mut candidates, 512).unwrap();

        let mut corrupted = input.clone();
        corrupted[8192 + 4096] ^= 1;
        let corrupted_spool = spool_input(Cursor::new(&corrupted), &resources, &context).unwrap();
        let mut invalid = BudgetedVec::new(&context.memory).unwrap();
        invalid.push(candidate(0, 8192, 8192, 0)).unwrap();
        assert_eq!(
            validate_candidates(&corrupted_spool, &mut invalid, 512)
                .unwrap_err()
                .kind(),
            crate::error::ErrorKind::InvalidMatch
        );
    }
}
