//! SREP-NG v3 writer. Independent of `reader.rs`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::codec::{CompressionStats, InputSpool, TempSpool, copy_spool, spool_input};
use crate::config::{CompressionConfig, Layout};
use crate::error::{Error, Result};
use crate::format_v3::{
    ArchiveHeader, Crc32c, GlobalDigest, IO_LITERAL_TAG, IO_MATCH_TAG, MAX_ULEB128_LEN, MAX_WIRE,
    POSITIVE_GAIN_MIN_LEN, TAIL_MAGIC, TAIL_PREFIX_LEN, VERSION, encode_archive_header,
    encode_uleb128_to, tail_len,
};
use crate::match_ir::{
    Match, MatchCandidate, NormalizedMatches, normalize_owned_matches_with_budget,
};
use crate::resource::{BudgetedVec, MemoryBudget, ResourceContext};
use crate::v3::compression_stats_from_header;

const SPOOL_CHUNK: usize = 64 * 1024;

/// Validate `config`, build a [`ResourceContext`] from `config.resources`,
/// spool input, and encode a v3 archive.
pub fn compress_with_candidates<R, W, I>(
    input: R,
    output: W,
    config: &CompressionConfig,
    candidates: I,
) -> Result<CompressionStats>
where
    R: Read,
    W: Write,
    I: IntoIterator<Item = MatchCandidate>,
{
    config.validate()?;
    let context = ResourceContext::with_resources(&config.resources)?;
    compress_with_candidates_with_context(input, output, config, candidates, &context)
}

/// Validate `config`, spool input under `context`, and encode a v3 archive.
pub fn compress_with_candidates_with_context<R, W, I>(
    input: R,
    output: W,
    config: &CompressionConfig,
    candidates: I,
    context: &ResourceContext,
) -> Result<CompressionStats>
where
    R: Read,
    W: Write,
    I: IntoIterator<Item = MatchCandidate>,
{
    config.validate()?;
    let spool = spool_input(input, &config.resources, context)?;
    let candidates = crate::candidate_validation::collect_candidates(candidates, &context.memory)?;
    compress_spooled_with_candidates(spool, config, candidates, context, output)
}

/// Encode a v3 archive from an already-spooled input and collected candidates.
pub(crate) fn compress_spooled_with_candidates<W: Write>(
    spool: InputSpool,
    config: &CompressionConfig,
    mut candidates: BudgetedVec<MatchCandidate>,
    context: &ResourceContext,
    mut output: W,
) -> Result<CompressionStats> {
    config.validate()?;
    let effective_min_match = config.effective_min_match()?;
    crate::candidate_validation::validate_candidates(&spool, &mut candidates, effective_min_match)?;
    let normalized =
        normalize_owned_matches_with_budget(candidates, spool.len, effective_min_match)?;
    validate_selected_matches(
        normalized.as_slice(),
        spool.len,
        effective_min_match,
        config.header_max_distance(),
    )?;
    let header = ArchiveHeader::from_config(config, spool.len)?;
    let mut staged = TempSpool::new(&config.resources, context)?;
    let stats = match encode_archive(
        &spool,
        config,
        &header,
        &normalized,
        &context.memory,
        &mut staged,
    ) {
        Ok(stats) => stats,
        Err(error) => return Err(staged.take_budget_error().unwrap_or(error)),
    };
    staged.rewind()?;
    copy_spool(&mut staged.file, &mut output)?;
    Ok(stats)
}

fn validate_selected_matches(
    matches: &[Match],
    input_len: u64,
    min_match: u64,
    max_distance: u64,
) -> Result<()> {
    let mut previous_end = 0u64;
    for item in matches {
        if item.src >= item.dst {
            return Err(Error::invalid_match(
                "selected match source must precede destination",
            ));
        }
        if item.len < min_match || item.len < POSITIVE_GAIN_MIN_LEN {
            return Err(Error::invalid_match(
                "selected match is shorter than the effective minimum",
            ));
        }
        let end = item
            .dst
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("selected match endpoint overflows"))?;
        if end > input_len || item.src >= input_len {
            return Err(Error::invalid_match(
                "selected match interval exceeds input",
            ));
        }
        if item.dst < previous_end {
            return Err(Error::invalid_match("selected match destinations overlap"));
        }
        let distance = item
            .dst
            .checked_sub(item.src)
            .ok_or_else(|| Error::invalid_match("selected match distance underflows"))?;
        if distance == 0 {
            return Err(Error::invalid_match(
                "selected match distance must be positive",
            ));
        }
        if max_distance != 0 && distance > max_distance {
            return Err(Error::invalid_match(
                "selected match distance exceeds max_distance",
            ));
        }
        previous_end = end;
    }
    Ok(())
}

fn encode_archive(
    spool: &InputSpool,
    config: &CompressionConfig,
    header: &ArchiveHeader,
    normalized: &NormalizedMatches,
    memory: &MemoryBudget,
    staged: &mut TempSpool,
) -> Result<CompressionStats> {
    let mut encoded = GlobalDigest::encoded(header.checksum);
    let mut plaintext = GlobalDigest::plaintext(header.checksum);
    write_hashed(staged, &mut encoded, &encode_archive_header(header)?)?;

    let source_order = if config.layout == Layout::Future {
        source_ordered_indices(normalized.as_slice(), memory)?
    } else {
        BudgetedVec::new(memory)?
    };

    let mut payload_size = 0u64;
    let block_count = header.block_count()?;
    let mut source_cursor = 0usize;
    let mut io_carry = 0u64;
    let mut io_match_index = 0usize;
    let mut plaintext_file = clone_spool_file(spool)?;
    plaintext_file
        .seek(SeekFrom::Start(0))
        .map_err(Error::temp_storage)?;

    for block_id in 0..block_count {
        let start = block_id
            .checked_mul(header.block_size)
            .ok_or_else(|| Error::invalid_match("block start overflows"))?;
        let block_len = crate::format_v3::block_len_at(spool.len, header.block_size, block_id)?;
        let end = start
            .checked_add(block_len)
            .ok_or_else(|| Error::invalid_match("block end overflows"))?;
        let crc = hash_block_plaintext(&mut plaintext_file, start, block_len, &mut plaintext)?;
        let before = staged.len;
        match config.layout {
            Layout::Index => emit_literals(
                spool,
                start,
                end,
                normalized.as_slice(),
                staged,
                &mut encoded,
            )?,
            Layout::Future => {
                emit_future_registers(
                    start,
                    end,
                    normalized.as_slice(),
                    source_order.as_slice(),
                    &mut source_cursor,
                    staged,
                    &mut encoded,
                )?;
                emit_literals(
                    spool,
                    start,
                    end,
                    normalized.as_slice(),
                    staged,
                    &mut encoded,
                )?;
            }
            Layout::Io => {
                emit_io_operations(
                    spool,
                    start,
                    end,
                    normalized.as_slice(),
                    &mut io_match_index,
                    &mut io_carry,
                    staged,
                    &mut encoded,
                )?;
            }
        }
        write_hashed(staged, &mut encoded, &crc)?;
        payload_size = payload_size
            .checked_add(
                staged
                    .len
                    .checked_sub(before)
                    .ok_or_else(|| Error::corrupt_record("v3 block body length underflow"))?,
            )
            .ok_or_else(|| Error::corrupt_record("v3 payload size overflows"))?;
    }

    if config.layout == Layout::Io && io_carry != 0 {
        return Err(Error::invalid_match("I/O match carry is unresolved at EOF"));
    }
    if config.layout == Layout::Future && source_cursor != source_order.len() {
        return Err(Error::invalid_match(
            "Future registers were not fully emitted",
        ));
    }

    let body_end = staged.len;
    let (index_offset, index_total_length) = if config.layout.is_index() {
        let expected = compact_index_len(normalized.as_slice())?;
        let written = write_compact_index(normalized.as_slice(), staged, &mut encoded)?;
        if written != expected {
            return Err(Error::corrupt_index(
                "IndexSection length does not match the checked size",
            ));
        }
        (body_end, written)
    } else {
        (0, 0)
    };

    let tail_bytes = u64::try_from(tail_len(header.checksum))
        .map_err(|_| Error::invalid_config("v3 tail length overflows u64"))?;
    let tail_start = staged.len;
    if !config.layout.is_index() && tail_start != body_end {
        return Err(Error::corrupt_record(
            "Future-LZ/I/O-LZ body end must equal tail start",
        ));
    }
    let archive_length = tail_start
        .checked_add(tail_bytes)
        .ok_or_else(|| Error::invalid_config("v3 archive length overflows"))?;
    if archive_length > MAX_WIRE {
        return Err(Error::invalid_config("v3 archive length exceeds MAX_WIRE"));
    }
    if config.layout.is_index() {
        let index_end = index_offset
            .checked_add(index_total_length)
            .ok_or_else(|| Error::corrupt_index("Index-LZ locator overflow"))?;
        if index_end != tail_start || body_end != index_offset {
            return Err(Error::corrupt_index("Index-LZ locators are inconsistent"));
        }
    }

    let plaintext_digest = plaintext.finalize();
    let mut prefix = [0u8; TAIL_PREFIX_LEN];
    prefix[..8].copy_from_slice(&TAIL_MAGIC);
    prefix[8] = VERSION;
    prefix[12..20].copy_from_slice(&archive_length.to_le_bytes());
    prefix[20..28].copy_from_slice(&index_offset.to_le_bytes());
    prefix[28..36].copy_from_slice(&index_total_length.to_le_bytes());
    prefix[36..44].copy_from_slice(&body_end.to_le_bytes());
    write_hashed(staged, &mut encoded, &prefix)?;
    write_hashed(staged, &mut encoded, &plaintext_digest)?;
    let encoded_digest = encoded.finalize();
    staged.append(&encoded_digest)?;
    if staged.len != archive_length {
        return Err(Error::corrupt_record(
            "staged v3 archive length does not match the tail",
        ));
    }
    staged.file.flush().map_err(Error::temp_storage)?;

    compression_stats_from_header(
        header,
        archive_length,
        payload_size,
        u64::try_from(normalized.len())
            .map_err(|_| Error::invalid_match("match count exceeds MAX_WIRE"))?,
        normalized.covered_bytes,
        normalized.literal_bytes,
    )
}

fn source_ordered_indices(matches: &[Match], memory: &MemoryBudget) -> Result<BudgetedVec<usize>> {
    let mut indices = BudgetedVec::with_capacity(matches.len(), memory)?;
    for index in 0..matches.len() {
        indices.push(index)?;
    }
    indices.sort_unstable_by(|left, right| {
        let a = &matches[*left];
        let b = &matches[*right];
        (a.src, a.dst, a.origin_match_id).cmp(&(b.src, b.dst, b.origin_match_id))
    });
    Ok(indices)
}

fn clone_spool_file(spool: &InputSpool) -> Result<File> {
    // `File::try_clone` duplicates the descriptor and shares the kernel
    // offset. Reopen so literal copies cannot disturb plaintext hashing.
    spool._temp.reopen().map_err(Error::temp_storage)
}

fn write_hashed(staged: &mut TempSpool, digest: &mut GlobalDigest, bytes: &[u8]) -> Result<()> {
    staged.append(bytes)?;
    digest.update(bytes);
    Ok(())
}

fn write_uleb_hashed(
    staged: &mut TempSpool,
    digest: &mut GlobalDigest,
    value: u64,
) -> Result<usize> {
    let mut buf = [0u8; MAX_ULEB128_LEN];
    let n = encode_uleb128_to(&mut buf, value)?;
    write_hashed(staged, digest, &buf[..n])?;
    Ok(n)
}

fn uleb_encoded_len(value: u64) -> Result<u64> {
    if value > MAX_WIRE {
        return Err(Error::invalid_config("value exceeds MAX_WIRE"));
    }
    let mut value = value;
    let mut len = 1u64;
    while value >= 0x80 {
        value >>= 7;
        len = len
            .checked_add(1)
            .ok_or_else(|| Error::invalid_config("ULEB128 length overflows"))?;
    }
    Ok(len)
}

fn compact_index_len(matches: &[Match]) -> Result<u64> {
    let count = u64::try_from(matches.len())
        .map_err(|_| Error::invalid_match("match count exceeds MAX_WIRE"))?;
    if count > MAX_WIRE {
        return Err(Error::invalid_match("match count exceeds MAX_WIRE"));
    }
    let mut len = uleb_encoded_len(count)?;
    let mut previous_end = 0u64;
    for item in matches {
        let dst_gap = item
            .dst
            .checked_sub(previous_end)
            .ok_or_else(|| Error::invalid_match("compact index destination overlaps"))?;
        let distance = item
            .dst
            .checked_sub(item.src)
            .ok_or_else(|| Error::invalid_match("compact index distance underflows"))?;
        if distance == 0 {
            return Err(Error::invalid_match(
                "compact index distance must be positive",
            ));
        }
        let end = item
            .dst
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("compact index endpoint overflows"))?;
        len = len
            .checked_add(uleb_encoded_len(dst_gap)?)
            .ok_or_else(|| Error::corrupt_index("IndexSection length overflows"))?;
        len = len
            .checked_add(uleb_encoded_len(distance)?)
            .ok_or_else(|| Error::corrupt_index("IndexSection length overflows"))?;
        len = len
            .checked_add(uleb_encoded_len(item.len)?)
            .ok_or_else(|| Error::corrupt_index("IndexSection length overflows"))?;
        previous_end = end;
    }
    Ok(len)
}

fn write_compact_index(
    matches: &[Match],
    staged: &mut TempSpool,
    digest: &mut GlobalDigest,
) -> Result<u64> {
    let count = u64::try_from(matches.len())
        .map_err(|_| Error::invalid_match("match count exceeds MAX_WIRE"))?;
    let mut written = write_uleb_hashed(staged, digest, count)? as u64;
    let mut previous_end = 0u64;
    for item in matches {
        if item.src >= item.dst || item.len < POSITIVE_GAIN_MIN_LEN {
            return Err(Error::invalid_match("compact index match is invalid"));
        }
        let dst_gap = item
            .dst
            .checked_sub(previous_end)
            .ok_or_else(|| Error::invalid_match("compact index destination overlaps"))?;
        let distance = item
            .dst
            .checked_sub(item.src)
            .ok_or_else(|| Error::invalid_match("compact index distance underflows"))?;
        if distance == 0 {
            return Err(Error::invalid_match(
                "compact index distance must be positive",
            ));
        }
        let end = item
            .dst
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("compact index endpoint overflows"))?;
        written = written
            .checked_add(write_uleb_hashed(staged, digest, dst_gap)? as u64)
            .ok_or_else(|| Error::corrupt_index("IndexSection length overflows"))?;
        written = written
            .checked_add(write_uleb_hashed(staged, digest, distance)? as u64)
            .ok_or_else(|| Error::corrupt_index("IndexSection length overflows"))?;
        written = written
            .checked_add(write_uleb_hashed(staged, digest, item.len)? as u64)
            .ok_or_else(|| Error::corrupt_index("IndexSection length overflows"))?;
        previous_end = end;
    }
    Ok(written)
}

fn hash_block_plaintext(
    file: &mut File,
    start: u64,
    block_len: u64,
    plaintext: &mut GlobalDigest,
) -> Result<[u8; 4]> {
    file.seek(SeekFrom::Start(start))
        .map_err(Error::temp_storage)?;
    let mut crc = Crc32c::new();
    let mut remaining = block_len;
    let mut buf = [0u8; SPOOL_CHUNK];
    while remaining > 0 {
        let count = usize::try_from(remaining.min(SPOOL_CHUNK as u64)).map_err(|_| {
            Error::temp_storage_context("block read length exceeds platform limits")
        })?;
        file.read_exact(&mut buf[..count])
            .map_err(|error| Error::map_eof(error, "truncated v3 input spool plaintext"))?;
        plaintext.update(&buf[..count]);
        crc.update(&buf[..count]);
        remaining -= count as u64;
    }
    Ok(crc.finalize_le())
}

fn emit_literals(
    spool: &InputSpool,
    start: u64,
    end: u64,
    matches: &[Match],
    staged: &mut TempSpool,
    digest: &mut GlobalDigest,
) -> Result<()> {
    for_each_literal_range(start, end, matches, |absolute, len| {
        copy_spool_range_hashed(spool, absolute, len, staged, digest)
    })
}

fn for_each_literal_range(
    start: u64,
    end: u64,
    matches: &[Match],
    mut emit: impl FnMut(u64, u64) -> Result<()>,
) -> Result<()> {
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
            emit(cursor, match_start - cursor)?;
        }
        cursor = cursor.max(item_end.min(end));
    }
    if cursor < end {
        emit(cursor, end - cursor)?;
    }
    Ok(())
}

fn copy_spool_range_hashed(
    spool: &InputSpool,
    start: u64,
    len: u64,
    staged: &mut TempSpool,
    digest: &mut GlobalDigest,
) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let mut file = clone_spool_file(spool)?;
    file.seek(SeekFrom::Start(start))
        .map_err(Error::temp_storage)?;
    let mut remaining = len;
    let mut buf = [0u8; SPOOL_CHUNK];
    while remaining > 0 {
        let count = usize::try_from(remaining.min(SPOOL_CHUNK as u64)).map_err(|_| {
            Error::temp_storage_context("literal copy length exceeds platform limits")
        })?;
        file.read_exact(&mut buf[..count])
            .map_err(|error| Error::map_eof(error, "truncated v3 input spool literals"))?;
        write_hashed(staged, digest, &buf[..count])?;
        remaining -= count as u64;
    }
    Ok(())
}

fn emit_future_registers(
    start: u64,
    end: u64,
    matches: &[Match],
    source_order: &[usize],
    source_cursor: &mut usize,
    staged: &mut TempSpool,
    digest: &mut GlobalDigest,
) -> Result<()> {
    if *source_cursor < source_order.len() && matches[source_order[*source_cursor]].src < start {
        return Err(Error::invalid_match(
            "Future register source is outside its owning block",
        ));
    }
    let first = *source_cursor;
    let mut count = 0u64;
    let mut next = first;
    while next < source_order.len() && matches[source_order[next]].src < end {
        count = count
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("Future register count overflows"))?;
        next += 1;
    }
    write_uleb_hashed(staged, digest, count)?;
    for index in source_order[first..next].iter().copied() {
        let item = matches[index];
        let source_offset = item
            .src
            .checked_sub(start)
            .ok_or_else(|| Error::invalid_match("Future source_offset underflows"))?;
        if source_offset >= end - start {
            return Err(Error::invalid_match(
                "Future source_offset exceeds the owning block",
            ));
        }
        let distance = item
            .dst
            .checked_sub(item.src)
            .ok_or_else(|| Error::invalid_match("Future distance underflows"))?;
        write_uleb_hashed(staged, digest, source_offset)?;
        write_uleb_hashed(staged, digest, distance)?;
        write_uleb_hashed(staged, digest, item.len)?;
    }
    *source_cursor = next;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_io_operations(
    spool: &InputSpool,
    start: u64,
    end: u64,
    matches: &[Match],
    match_index: &mut usize,
    carry: &mut u64,
    staged: &mut TempSpool,
    digest: &mut GlobalDigest,
) -> Result<()> {
    let mut cursor = start;
    if *carry != 0 {
        let remaining_block = end
            .checked_sub(cursor)
            .ok_or_else(|| Error::invalid_match("I/O block remaining underflows"))?;
        let take = (*carry).min(remaining_block);
        *carry -= take;
        cursor = cursor
            .checked_add(take)
            .ok_or_else(|| Error::invalid_match("I/O carry destination overflows"))?;
    }
    while cursor < end {
        while *match_index < matches.len() {
            let item = matches[*match_index];
            let item_end = item
                .dst
                .checked_add(item.len)
                .ok_or_else(|| Error::invalid_match("I/O match endpoint overflows"))?;
            if item_end <= cursor {
                *match_index += 1;
                continue;
            }
            break;
        }
        if *match_index < matches.len() && matches[*match_index].dst == cursor {
            let item = matches[*match_index];
            let distance = item
                .dst
                .checked_sub(item.src)
                .ok_or_else(|| Error::invalid_match("I/O distance underflows"))?;
            write_hashed(staged, digest, &[IO_MATCH_TAG])?;
            write_uleb_hashed(staged, digest, distance)?;
            write_uleb_hashed(staged, digest, item.len)?;
            let remaining_block = end - cursor;
            let take = item.len.min(remaining_block);
            if take < item.len {
                *carry = item.len - take;
            }
            cursor += take;
            *match_index += 1;
            continue;
        }
        let gap_end = if *match_index < matches.len() && matches[*match_index].dst < end {
            matches[*match_index].dst
        } else {
            end
        };
        if gap_end <= cursor {
            return Err(Error::invalid_match("I/O literal gap is empty or inverted"));
        }
        let literal_len = gap_end - cursor;
        if literal_len == 0 || gap_end > end {
            return Err(Error::invalid_match(
                "I/O literal must be positive and fit the current block",
            ));
        }
        write_hashed(staged, digest, &[IO_LITERAL_TAG])?;
        write_uleb_hashed(staged, digest, literal_len)?;
        copy_spool_range_hashed(spool, cursor, literal_len, staged, digest)?;
        cursor = gap_end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Checksum, Method, ResourceConfig};
    use crate::error::ErrorKind;
    use crate::format_v3::{
        ArchiveTail, ENCODED_DOMAIN, HEADER_LEN, NG_V3_MAGIC, PLAINTEXT_DOMAIN, TAIL_LEN_BLAKE3,
        TAIL_LEN_XXH3, crc32c, crc32c_le, decode_compact_index, decode_uleb128,
        encode_uleb128_into, parse_archive_header, parse_archive_tail, plaintext_digest,
        validate_tail_layout,
    };
    use std::io::Cursor;
    use std::path::PathBuf;

    const TEST_TEMP: &str = "/tmp/opencode/v3-writer";

    fn test_resources() -> ResourceConfig {
        std::fs::create_dir_all(TEST_TEMP).unwrap();
        ResourceConfig {
            temp_dir: PathBuf::from(TEST_TEMP),
            ..ResourceConfig::default()
        }
    }

    fn m1_config(layout: Layout, checksum: Checksum) -> CompressionConfig {
        CompressionConfig {
            method: Method::M1RollingCdc,
            layout,
            checksum,
            block_size: 1024,
            min_match: 32,
            seed_size: None,
            target_chunk: Some(4096),
            max_distance: None,
            rep_overlay: None,
            resources: test_resources(),
        }
    }

    fn candidate(src: u64, dst: u64, len: u64, ordinal: u64) -> MatchCandidate {
        MatchCandidate {
            src,
            dst,
            len,
            insertion_ordinal: ordinal,
        }
    }

    fn materialize(len: usize, matches: &[(u64, u64, u64)]) -> Vec<u8> {
        let mut data = vec![0u8; len];
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = (index.wrapping_mul(17) + 3) as u8;
        }
        let mut ordered = matches.to_vec();
        ordered.sort_unstable_by_key(|&(_, dst, _)| dst);
        for &(src, dst, match_len) in &ordered {
            let distance = dst - src;
            for offset in 0..match_len {
                data[(dst + offset) as usize] = data[(src + (offset % distance)) as usize];
            }
        }
        data
    }

    fn compress_bytes(
        input: &[u8],
        config: &CompressionConfig,
        candidates: impl IntoIterator<Item = MatchCandidate>,
    ) -> (Vec<u8>, CompressionStats) {
        let mut archive = Vec::new();
        let stats =
            compress_with_candidates(Cursor::new(input), &mut archive, config, candidates).unwrap();
        (archive, stats)
    }

    fn assemble_no_match(config: &CompressionConfig, plaintext: &[u8]) -> Vec<u8> {
        let header = ArchiveHeader::from_config(config, plaintext.len() as u64).unwrap();
        let header_bytes = encode_archive_header(&header).unwrap();
        let mut encoded = GlobalDigest::encoded(config.checksum);
        encoded.update(&header_bytes);
        let mut body = Vec::new();
        let blocks = header.block_count().unwrap();
        for block_id in 0..blocks {
            let start = (block_id * header.block_size) as usize;
            let block_len = crate::format_v3::block_len_at(
                header.uncompressed_length,
                header.block_size,
                block_id,
            )
            .unwrap() as usize;
            let slice = &plaintext[start..start + block_len];
            match config.layout {
                Layout::Index => body.extend_from_slice(slice),
                Layout::Future => {
                    body.push(0);
                    body.extend_from_slice(slice);
                }
                Layout::Io => {
                    body.push(IO_LITERAL_TAG);
                    encode_uleb128_into(block_len as u64, &mut body).unwrap();
                    body.extend_from_slice(slice);
                }
            }
            body.extend_from_slice(&crc32c_le(slice));
        }
        encoded.update(&body);
        let mut index = Vec::new();
        if config.layout.is_index() {
            index.push(0x00);
            encoded.update(&index);
        }
        let body_end = HEADER_LEN as u64 + body.len() as u64;
        let index_offset = if config.layout.is_index() {
            body_end
        } else {
            0
        };
        let index_total_length = index.len() as u64;
        let tail_n = tail_len(config.checksum) as u64;
        let tail_start = body_end + index_total_length;
        let archive_length = tail_start + tail_n;
        let plaintext_digest = plaintext_digest(config.checksum, plaintext);
        let tail = crate::format_v3::encode_archive_tail(
            &ArchiveTail {
                archive_length,
                index_offset,
                index_total_length,
                body_end,
                plaintext_digest: plaintext_digest.clone(),
                encoded_digest: vec![0; config.checksum.width()],
            },
            config.checksum,
        )
        .unwrap();
        encoded.update(&tail[..TAIL_PREFIX_LEN + config.checksum.width()]);
        let encoded_digest = encoded.finalize();
        let mut out = header_bytes.to_vec();
        out.extend_from_slice(&body);
        out.extend_from_slice(&index);
        out.extend_from_slice(&tail[..TAIL_PREFIX_LEN]);
        out.extend_from_slice(&plaintext_digest);
        out.extend_from_slice(&encoded_digest);
        out
    }

    fn parse_header_and_tail(archive: &[u8]) -> (crate::format_v3::ArchiveHeader, ArchiveTail) {
        let header = parse_archive_header(&archive[..HEADER_LEN]).unwrap();
        let tail_n = tail_len(header.checksum);
        let tail = parse_archive_tail(&archive[archive.len() - tail_n..], header.checksum).unwrap();
        validate_tail_layout(&tail, header.layout, header.checksum, archive.len() as u64).unwrap();
        (header, tail)
    }

    fn apply_matches(plaintext_len: usize, matches: &[Match], literals: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; plaintext_len];
        let mut covered = vec![false; plaintext_len];
        for item in matches {
            for offset in 0..item.len {
                covered[(item.dst + offset) as usize] = true;
            }
        }
        let mut literal_cursor = 0usize;
        for (index, slot) in out.iter_mut().enumerate() {
            if !covered[index] {
                *slot = literals[literal_cursor];
                literal_cursor += 1;
            }
        }
        assert_eq!(literal_cursor, literals.len());
        let mut ordered = matches.to_vec();
        ordered.sort_unstable_by(|a, b| a.dst.cmp(&b.dst).then(a.src.cmp(&b.src)));
        for item in ordered {
            for offset in 0..item.len {
                let dst = (item.dst + offset) as usize;
                let src = (item.src + offset) as usize;
                out[dst] = out[src];
            }
        }
        out
    }

    #[test]
    fn empty_archives_match_spec_both_algorithms() {
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                let config = m1_config(layout, checksum);
                let expected = assemble_no_match(&config, b"");
                let (archive, stats) = compress_bytes(b"", &config, []);
                assert_eq!(archive, expected);
                assert_eq!(
                    archive.len() as u64,
                    crate::format_v3::empty_archive_len(layout, checksum)
                );
                assert_eq!(&archive[..8], &NG_V3_MAGIC);
                let (header, tail) = parse_header_and_tail(&archive);
                assert_eq!(header.uncompressed_length, 0);
                assert_eq!(header.block_count().unwrap(), 0);
                assert_eq!(stats.original_size, 0);
                assert_eq!(stats.archive_size, archive.len() as u64);
                assert_eq!(stats.payload_size, 0);
                assert_eq!(stats.block_count, 0);
                assert_eq!(stats.compressed_blocks, 0);
                assert_eq!(stats.reference_count, 0);
                assert_eq!(stats.semantic_match_count, 0);
                assert_eq!(stats.literal_bytes, 0);
                assert_eq!(stats.method, Some(Method::M1RollingCdc));
                assert_eq!(stats.layout, Some(layout));
                assert_eq!(stats.checksum, Some(checksum));
                assert_eq!(header.method, Method::M1RollingCdc);
                assert_eq!(header.layout, layout);
                assert_eq!(header.checksum, checksum);
                assert_eq!(header.min_match, 32);
                assert_eq!(header.seed_size, 48);
                assert_eq!(header.target_chunk, 4096);
                let width = checksum.width();
                assert_eq!(tail.plaintext_digest, plaintext_digest(checksum, b""));
                assert_eq!(PLAINTEXT_DOMAIN.len(), 18);
                assert_eq!(ENCODED_DOMAIN.len(), 16);
                if layout.is_index() {
                    assert_eq!(tail.index_offset, 80);
                    assert_eq!(tail.index_total_length, 1);
                    assert_eq!(tail.body_end, 80);
                    assert_eq!(archive[80], 0x00);
                    assert_eq!(
                        tail.archive_length,
                        80 + 1
                            + if checksum == Checksum::Xxh3 {
                                TAIL_LEN_XXH3
                            } else {
                                TAIL_LEN_BLAKE3
                            } as u64
                    );
                } else {
                    assert_eq!(tail.index_offset, 0);
                    assert_eq!(tail.index_total_length, 0);
                    assert_eq!(tail.body_end, 80);
                    assert_eq!(
                        tail.archive_length,
                        80 + if checksum == Checksum::Xxh3 {
                            TAIL_LEN_XXH3
                        } else {
                            TAIL_LEN_BLAKE3
                        } as u64
                    );
                }
                let domain_end = archive.len() - width;
                let mut encoded = GlobalDigest::encoded(checksum);
                encoded.update(&archive[..domain_end]);
                assert_eq!(encoded.finalize(), tail.encoded_digest);
            }
        }
    }

    #[test]
    fn abc_no_match_matches_spec_both_algorithms() {
        const ABC: &[u8] = b"abc";
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                let config = m1_config(layout, checksum);
                let expected = assemble_no_match(&config, ABC);
                let (archive, stats) = compress_bytes(ABC, &config, []);
                assert_eq!(archive, expected);
                let (header, tail) = parse_header_and_tail(&archive);
                assert_eq!(header.uncompressed_length, 3);
                assert_eq!(header.block_count().unwrap(), 1);
                assert_eq!(stats.original_size, 3);
                assert_eq!(stats.literal_bytes, 3);
                assert_eq!(stats.covered_bytes, 0);
                assert_eq!(stats.semantic_match_count, 0);
                assert_eq!(stats.reference_count, 0);
                assert_eq!(stats.block_count, 1);
                assert_eq!(crc32c(ABC), crc32c(b"abc"));
                let crc = crc32c_le(ABC);
                match layout {
                    Layout::Index => {
                        assert_eq!(&archive[80..83], ABC);
                        assert_eq!(&archive[83..87], &crc);
                        assert_eq!(archive[87], 0x00);
                        assert_eq!(tail.body_end, 87);
                        assert_eq!(tail.index_offset, 87);
                        assert_eq!(tail.index_total_length, 1);
                        assert_eq!(stats.payload_size, 7);
                    }
                    Layout::Future => {
                        assert_eq!(archive[80], 0x00);
                        assert_eq!(&archive[81..84], ABC);
                        assert_eq!(&archive[84..88], &crc);
                        assert_eq!(tail.body_end, 88);
                        assert_eq!(stats.payload_size, 8);
                    }
                    Layout::Io => {
                        assert_eq!(archive[80], IO_LITERAL_TAG);
                        assert_eq!(archive[81], 0x03);
                        assert_eq!(&archive[82..85], ABC);
                        assert_eq!(&archive[85..89], &crc);
                        assert_eq!(tail.body_end, 89);
                        assert_eq!(stats.payload_size, 9);
                    }
                }
                assert_eq!(tail.plaintext_digest, plaintext_digest(checksum, ABC));
            }
        }
    }

    #[test]
    fn partial_and_full_literal_blocks_match_independent_assembly() {
        for &len in &[1024usize, 1025] {
            let plaintext: Vec<u8> = (0..len).map(|index| (index % 251) as u8).collect();
            for layout in [Layout::Index, Layout::Future, Layout::Io] {
                let config = m1_config(layout, Checksum::Xxh3);
                let expected = assemble_no_match(&config, &plaintext);
                let (archive, stats) = compress_bytes(&plaintext, &config, []);
                assert_eq!(archive, expected);
                assert_eq!(stats.literal_bytes, len as u64);
                assert_eq!(stats.block_count, if len == 1024 { 1 } else { 2 });
                let (header, tail) = parse_header_and_tail(&archive);
                assert_eq!(header.uncompressed_length, len as u64);
                if layout.is_index() && len == 1025 {
                    assert_eq!(tail.body_end, 1113);
                    assert_eq!(tail.index_offset, 1113);
                    assert_eq!(archive[1113], 0x00);
                }
                let first_crc_at = match layout {
                    Layout::Index => 80 + 1024,
                    Layout::Future => 80 + 1 + 1024,
                    Layout::Io => {
                        let mut encoded_len = Vec::new();
                        encode_uleb128_into(1024, &mut encoded_len).unwrap();
                        80 + 1 + encoded_len.len() + 1024
                    }
                };
                assert_eq!(
                    &archive[first_crc_at..first_crc_at + 4],
                    &crc32c_le(&plaintext[..1024.min(len)])
                );
            }
        }
    }

    #[test]
    fn three_layouts_preserve_original_ir_stats() {
        let matches = [(0, 1024, 64), (64, 1088, 64)];
        let input = materialize(1152, &matches);
        let candidates = [candidate(0, 1024, 64, 0), candidate(64, 1088, 64, 1)];
        let mut stats = Vec::new();
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            let config = m1_config(layout, Checksum::Blake3);
            let (archive, got) = compress_bytes(&input, &config, candidates);
            assert_eq!(got.semantic_match_count, 2);
            assert_eq!(got.reference_count, 2);
            assert_eq!(got.covered_bytes, 128);
            assert_eq!(got.literal_bytes, 1024);
            assert_eq!(got.block_count, 2);
            let (header, tail) = parse_header_and_tail(&archive);
            assert_eq!(header.layout, layout);
            assert_eq!(header.checksum, Checksum::Blake3);
            assert_eq!(header.uncompressed_length, 1152);
            if layout.is_index() {
                let index = &archive[tail.index_offset as usize
                    ..(tail.index_offset + tail.index_total_length) as usize];
                let decoded = decode_compact_index(index, 1152, 32, 0).unwrap();
                assert_eq!(decoded[0].src, 0);
                assert_eq!(decoded[0].dst, 1024);
                assert_eq!(decoded[0].len, 64);
                assert_eq!(decoded[1].src, 64);
                assert_eq!(decoded[1].dst, 1088);
                assert_eq!(decoded[1].len, 64);
                let mut expected = Vec::new();
                encode_uleb128_into(2, &mut expected).unwrap();
                crate::format_v3::encode_triple_into(1024, 1024, 64, &mut expected).unwrap();
                crate::format_v3::encode_triple_into(0, 1024, 64, &mut expected).unwrap();
                assert_eq!(index, expected);
                let literals = &archive[80..80 + 1024];
                assert_eq!(literals, &input[..1024]);
                assert_eq!(&archive[80 + 1024..80 + 1028], &crc32c_le(&input[..1024]));
                assert_eq!(
                    &archive[80 + 1028..80 + 1032],
                    &crc32c_le(&input[1024..1152])
                );
                let rebuilt = apply_matches(1152, &decoded, literals);
                assert_eq!(rebuilt, input);
            }
            if layout == Layout::Io {
                let body0 = &archive[80..];
                assert_eq!(body0[0], IO_LITERAL_TAG);
                let (lit_len, n) = decode_uleb128(&body0[1..]).unwrap();
                assert_eq!(lit_len, 1024);
                let crc0 = 1 + n + 1024;
                assert_eq!(&body0[crc0..crc0 + 4], &crc32c_le(&input[..1024]));
                let body1 = &body0[crc0 + 4..];
                assert_eq!(body1[0], IO_MATCH_TAG);
                let (distance, n1) = decode_uleb128(&body1[1..]).unwrap();
                let (total_len, n2) = decode_uleb128(&body1[1 + n1..]).unwrap();
                assert_eq!(distance, 1024);
                assert_eq!(total_len, 64);
                let second = 1 + n1 + n2;
                assert_eq!(body1[second], IO_MATCH_TAG);
            }
            if layout == Layout::Future {
                let (count, n) = decode_uleb128(&archive[80..]).unwrap();
                assert_eq!(count, 2);
                let mut cursor = 80 + n;
                let mut registers = Vec::new();
                for _ in 0..count {
                    let (source_offset, n0) = decode_uleb128(&archive[cursor..]).unwrap();
                    cursor += n0;
                    let (distance, n1) = decode_uleb128(&archive[cursor..]).unwrap();
                    cursor += n1;
                    let (total_len, n2) = decode_uleb128(&archive[cursor..]).unwrap();
                    cursor += n2;
                    registers.push((source_offset, distance, total_len));
                }
                assert_eq!(registers[0], (0, 1024, 64));
                assert_eq!(registers[1], (64, 1024, 64));
                assert_eq!(&archive[cursor..cursor + 1024], &input[..1024]);
            }
            stats.push(got);
        }
        assert_eq!(stats[0].semantic_match_count, stats[1].semantic_match_count);
        assert_eq!(stats[1].reference_count, stats[2].reference_count);
        assert_eq!(stats[0].covered_bytes, stats[2].covered_bytes);
        assert_eq!(stats[0].literal_bytes, stats[2].literal_bytes);
    }

    #[test]
    fn cross_block_io_carry_preserves_one_origin() {
        let matches = [(0, 1000, 2200)];
        let input = materialize(3300, &matches);
        let config = m1_config(Layout::Io, Checksum::Xxh3);
        let (archive, stats) = compress_bytes(&input, &config, [candidate(0, 1000, 2200, 0)]);
        assert_eq!(stats.semantic_match_count, 1);
        assert_eq!(stats.reference_count, 1);
        assert_eq!(stats.covered_bytes, 2200);
        assert_eq!(stats.literal_bytes, 1100);
        assert_eq!(stats.block_count, 4);
        let (_header, tail) = parse_header_and_tail(&archive);
        assert_eq!(tail.index_offset, 0);
        let mut cursor = HEADER_LEN;
        let mut body_lens = Vec::new();
        for block_id in 0..4u64 {
            let start = cursor;
            let block_len = crate::format_v3::block_len_at(3300, 1024, block_id).unwrap();
            let block_plain =
                &input[(block_id * 1024) as usize..(block_id * 1024 + block_len) as usize];
            if block_id == 0 {
                assert_eq!(archive[cursor], IO_LITERAL_TAG);
                cursor += 1;
                let (lit, n) = decode_uleb128(&archive[cursor..]).unwrap();
                assert_eq!(lit, 1000);
                cursor += n + 1000;
                assert_eq!(archive[cursor], IO_MATCH_TAG);
                cursor += 1;
                let (distance, n1) = decode_uleb128(&archive[cursor..]).unwrap();
                cursor += n1;
                let (total_len, n2) = decode_uleb128(&archive[cursor..]).unwrap();
                cursor += n2;
                assert_eq!(distance, 1000);
                assert_eq!(total_len, 2200);
            } else if block_id == 3 {
                assert_eq!(archive[cursor], IO_LITERAL_TAG);
                cursor += 1;
                let (lit, n) = decode_uleb128(&archive[cursor..]).unwrap();
                assert_eq!(lit, 100);
                cursor += n + 100;
            }
            assert_eq!(&archive[cursor..cursor + 4], &crc32c_le(block_plain));
            cursor += 4;
            body_lens.push(cursor - start);
        }
        assert_eq!(body_lens[1], 4);
        assert_eq!(body_lens[2], 4);
        assert_eq!(cursor as u64, tail.body_end);
        assert_eq!(
            &archive[HEADER_LEN + 3..HEADER_LEN + 3 + 1000],
            &input[..1000]
        );
    }

    #[test]
    fn future_source_span_and_same_block_overlap() {
        let matches = [(0, 1000, 1100), (1000, 2100, 64), (100, 120, 64)];
        let input = materialize(2164, &matches);
        let config = m1_config(Layout::Future, Checksum::Xxh3);
        let (archive, stats) = compress_bytes(
            &input,
            &config,
            [
                candidate(0, 1000, 1100, 0),
                candidate(1000, 2100, 64, 1),
                candidate(100, 120, 64, 2),
            ],
        );
        assert_eq!(stats.semantic_match_count, 3);
        assert_eq!(stats.reference_count, 3);
        assert_eq!(stats.covered_bytes, 1228);
        assert_eq!(stats.literal_bytes, 936);
        let (count, n) = decode_uleb128(&archive[80..]).unwrap();
        assert_eq!(count, 3);
        let mut cursor = 80 + n;
        let mut registers = Vec::new();
        for _ in 0..count {
            let (source_offset, n0) = decode_uleb128(&archive[cursor..]).unwrap();
            cursor += n0;
            let (distance, n1) = decode_uleb128(&archive[cursor..]).unwrap();
            cursor += n1;
            let (total_len, n2) = decode_uleb128(&archive[cursor..]).unwrap();
            cursor += n2;
            registers.push((source_offset, distance, total_len));
        }
        assert_eq!(
            registers,
            vec![(0, 1000, 1100), (100, 20, 64), (1000, 1100, 64)]
        );
        let mut decoded = registers
            .iter()
            .map(|&(source_offset, distance, total_len)| Match {
                src: source_offset,
                dst: source_offset + distance,
                len: total_len,
                origin_match_id: 0,
            })
            .collect::<Vec<_>>();
        decoded.sort_unstable_by(|a, b| a.dst.cmp(&b.dst).then(a.src.cmp(&b.src)));
        let literal_len = 936;
        let literals = &archive[cursor..cursor + literal_len];
        cursor += literal_len;
        assert_eq!(&archive[cursor..cursor + 4], &crc32c_le(&input[..1024]));
        cursor += 4;
        assert_eq!(archive[cursor], 0x00);
        cursor += 1;
        assert_eq!(&archive[cursor..cursor + 4], &crc32c_le(&input[1024..2048]));
        cursor += 4;
        assert_eq!(archive[cursor], 0x00);
        cursor += 1;
        assert_eq!(&archive[cursor..cursor + 4], &crc32c_le(&input[2048..2164]));
        let rebuilt = apply_matches(2164, &decoded, literals);
        assert_eq!(&rebuilt[..1024], &input[..1024]);
        assert_eq!(&rebuilt[1024..], &input[1024..]);
    }

    #[test]
    fn index_cross_block_keeps_one_triple() {
        let matches = [(0, 1000, 2200)];
        let input = materialize(3300, &matches);
        let config = m1_config(Layout::Index, Checksum::Xxh3);
        let (archive, stats) = compress_bytes(&input, &config, [candidate(0, 1000, 2200, 0)]);
        assert_eq!(stats.reference_count, 1);
        assert_eq!(stats.semantic_match_count, 1);
        let (_header, tail) = parse_header_and_tail(&archive);
        let index = &archive
            [tail.index_offset as usize..(tail.index_offset + tail.index_total_length) as usize];
        let decoded = decode_compact_index(index, 3300, 32, 0).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].src, 0);
        assert_eq!(decoded[0].dst, 1000);
        assert_eq!(decoded[0].len, 2200);
        assert_eq!(&archive[80..1080], &input[..1000]);
        assert_eq!(&archive[1080..1084], &crc32c_le(&input[..1024]));
        assert_eq!(&archive[1084..1088], &crc32c_le(&input[1024..2048]));
        assert_eq!(&archive[1088..1092], &crc32c_le(&input[2048..3072]));
        assert_eq!(&archive[1092..1192], &input[3200..3300]);
        assert_eq!(&archive[1192..1196], &crc32c_le(&input[3072..3300]));
    }

    #[test]
    fn candidate_failure_invalid_config_output_io_and_temp_budget_rollback() {
        let config = m1_config(Layout::Index, Checksum::Xxh3);
        let input: Vec<u8> = (0..64).map(|index| index as u8).collect();
        let mut out = Vec::new();
        let error = compress_with_candidates(
            Cursor::new(&input),
            &mut out,
            &config,
            [candidate(0, 32, 32, 0)],
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidMatch);
        assert!(out.is_empty());

        let mut bad = config.clone();
        bad.block_size = 1;
        let error = compress_with_candidates(Cursor::new(b"abc"), &mut out, &bad, []).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidConfiguration);
        assert!(out.is_empty());

        struct FailWriter;
        impl Write for FailWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("public output failed"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error =
            compress_with_candidates(Cursor::new(b""), FailWriter, &config, []).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::OutputIo);

        let mut tight = config.clone();
        tight.resources.temp_limit = 40;
        let resources = tight.resources.clone();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let error =
            compress_with_candidates_with_context(Cursor::new(b""), &mut out, &tight, [], &context)
                .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TempBudgetExceeded);
        assert!(out.is_empty());
        assert_eq!(context.temp.current(), 0);
        assert_eq!(context.memory.current(), 0);

        let resources = test_resources();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let ok_config = m1_config(Layout::Future, Checksum::Xxh3);
        compress_with_candidates_with_context(
            Cursor::new(b"abc"),
            &mut out,
            &ok_config,
            [],
            &context,
        )
        .unwrap();
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);

        let context = ResourceContext::with_resources(&resources).unwrap();
        let unique: Vec<u8> = (0..64).map(|index| index as u8).collect();
        let error = compress_with_candidates_with_context(
            Cursor::new(&unique),
            std::io::sink(),
            &ok_config,
            [candidate(0, 32, 32, 0)],
            &context,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidMatch);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }

    #[test]
    fn selected_match_beyond_max_distance_is_invalid() {
        let mut config = m1_config(Layout::Index, Checksum::Xxh3);
        config.max_distance = Some(512);
        let input = materialize(1152, &[(0, 1024, 64)]);
        let error = compress_with_candidates(
            Cursor::new(&input),
            std::io::sink(),
            &config,
            [candidate(0, 1024, 64, 0)],
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidMatch);
    }
}
