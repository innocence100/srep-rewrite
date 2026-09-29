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

/// Bounded private writer entry. No public bytes are written here. B0 publishes
/// the returned stage only after CopyPermit preflight succeeds.
pub(crate) fn stage_selected(
    spool: &InputSpool,
    config: &CompressionConfig,
    selected: &crate::offload::matches::DiskSelectedStore,
    session: &crate::offload::session::CompressionSession,
) -> Result<(crate::codec::SealedStagedFile, CompressionStats)> {
    let result = stage_selected_inner(spool, config, selected, session, 4096);
    match result {
        Ok(value) => {
            session.retry_cleanup()?;
            Ok(value)
        }
        Err(error) => {
            if let Err(cleanup) = session.retry_cleanup() {
                eprintln!("{cleanup}");
            }
            Err(error)
        }
    }
}

struct StageOutput {
    spool: TempSpool,
    whole: blake3::Hasher,
    encoded: GlobalDigest,
}
impl StageOutput {
    fn append(&mut self, bytes: &[u8], encoded: bool) -> Result<()> {
        self.spool.append(bytes)?;
        self.whole.update(bytes);
        if encoded {
            self.encoded.update(bytes);
        }
        Ok(())
    }
    fn uleb(&mut self, n: u64) -> Result<()> {
        let mut b = [0; MAX_ULEB128_LEN];
        let len = encode_uleb128_to(&mut b, n)?;
        self.append(&b[..len], true)
    }
}

/// One reusable independently reopened input handle. Hashing and literal passes
/// explicitly seek; their logical positions cannot interfere. Buffer is fixed.
struct BoundedInput<'a> {
    file: File,
    _handle: crate::offload::session::ResourceLease,
    _memory: crate::resource::Reservation,
    session: &'a crate::offload::session::CompressionSession,
    buffer: [u8; SPOOL_CHUNK],
}
impl<'a> BoundedInput<'a> {
    fn new(
        spool: &InputSpool,
        session: &'a crate::offload::session::CompressionSession,
    ) -> Result<Self> {
        let handle = session.acquire_handles(1)?;
        let memory = session.memory().reserve(SPOOL_CHUNK as u64 + 4096)?;
        session.checkpoint(crate::offload::session::Operation::Open)?;
        let file = spool._temp.reopen().map_err(Error::temp_storage)?;
        Ok(Self {
            file,
            _handle: handle,
            _memory: memory,
            session,
            buffer: [0; SPOOL_CHUNK],
        })
    }
    fn range(
        &mut self,
        start: u64,
        len: u64,
        context: &'static str,
        mut consume: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        use crate::offload::session::Operation;
        let mut permit = self.session.reserve_io(u128::from(len))?;
        self.session.checkpoint(Operation::Seek)?;
        self.file
            .seek(SeekFrom::Start(start))
            .map_err(Error::temp_storage)?;
        let mut remaining = len;
        while remaining != 0 {
            let wanted = remaining.min(SPOOL_CHUNK as u64) as usize;
            self.session.checkpoint(Operation::Read)?;
            let n = match self.file.read(&mut self.buffer[..wanted]) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                r => r.map_err(|e| Error::map_eof(e, context))?,
            };
            if n == 0 {
                return Err(Error::truncated(context));
            }
            permit.consume(n as u64)?;
            consume(&self.buffer[..n])?;
            remaining -= n as u64;
        }
        Ok(())
    }
}

fn stage_selected_inner(
    spool: &InputSpool,
    config: &CompressionConfig,
    selected: &crate::offload::matches::DiskSelectedStore,
    session: &crate::offload::session::CompressionSession,
    source_chunk: usize,
) -> Result<(crate::codec::SealedStagedFile, CompressionStats)> {
    use crate::offload::contracts::{ReplayStore, SelectedStore, SourceOrderBuilder};
    use crate::offload::matches::DiskSourceOrderBuilder;
    use crate::offload::session::Operation;
    config.validate()?;
    session.check_cancelled()?;
    selected.verify(session)?;
    if selected.input_len() != spool.len {
        return Err(Error::invalid_match(
            "selected input length differs from spool",
        ));
    }
    let min = config.effective_min_match()?;
    let max = config.header_max_distance();
    let summary = selected.summary();
    let mut cursor = selected.cursor()?;
    let mut previous_end = 0;
    let mut count = 0u64;
    let mut covered = 0u64;
    let mut expected_index = uleb_encoded_len(summary.count)?;
    while let Some(item) = cursor.next()? {
        session.check_cancelled()?;
        validate_selected_matches(std::slice::from_ref(&item), spool.len, min, max)?;
        if item.dst < previous_end || item.origin_match_id != count {
            return Err(Error::invalid_match("selected stream order/ID invalid"));
        }
        let gap_bytes = uleb_encoded_len(item.dst - previous_end)?;
        let distance_bytes = uleb_encoded_len(item.dst - item.src)?;
        let length_bytes = uleb_encoded_len(item.len)?;
        expected_index = expected_index
            .checked_add(gap_bytes)
            .and_then(|n| n.checked_add(distance_bytes))
            .and_then(|n| n.checked_add(length_bytes))
            .ok_or_else(|| Error::corrupt_index("IndexSection length overflows"))?;
        previous_end = item
            .dst
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("selected endpoint overflow"))?;
        count = count
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("selected count overflow"))?;
        covered = covered
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("selected coverage overflow"))?;
    }
    drop(cursor);
    if count != summary.count
        || covered != summary.covered_bytes
        || spool.len.checked_sub(covered) != Some(summary.literal_bytes)
    {
        return Err(Error::corrupt_index("selected stream summary mismatch"));
    }
    let source = if config.layout == Layout::Future {
        Some(DiskSourceOrderBuilder::with_chunk_records(source_chunk)?.build(selected, session)?)
    } else {
        None
    };
    let mut source_cursor = source.as_ref().map(|s| s.cursor_in(session)).transpose()?;
    let mut source_position = 0u64;
    let header = ArchiveHeader::from_config(config, spool.len)?;
    let mut input = BoundedInput::new(spool, session)?;
    let mut out = StageOutput {
        spool: TempSpool::new_in_session(session)?,
        whole: blake3::Hasher::new(),
        encoded: GlobalDigest::encoded(header.checksum),
    };
    out.append(&encode_archive_header(&header)?, true)?;
    let mut plaintext = GlobalDigest::plaintext(header.checksum);
    let mut destination = selected.cursor()?;
    let mut next = destination.next()?;
    let mut carry = 0u64;
    let mut payload_size = 0u64;
    for block in 0..header.block_count()? {
        session.check_cancelled()?;
        let start = block
            .checked_mul(header.block_size)
            .ok_or_else(|| Error::invalid_match("block start overflows"))?;
        let length = crate::format_v3::block_len_at(spool.len, header.block_size, block)?;
        let end = start
            .checked_add(length)
            .ok_or_else(|| Error::invalid_match("block end overflows"))?;
        let mut crc = Crc32c::new();
        input.range(
            start,
            length,
            "truncated v3 input spool plaintext",
            |bytes| {
                plaintext.update(bytes);
                crc.update(bytes);
                Ok(())
            },
        )?;
        let before = out.spool.len;
        if let Some(cursor) = source_cursor.as_mut() {
            cursor.seek_row(source_position)?;
            let first = source_position;
            let mut limit = first;
            while let Some(item) = cursor.next()? {
                if item.src < start {
                    return Err(Error::invalid_match("Future source outside owning block"));
                }
                if item.src >= end {
                    break;
                }
                limit = limit
                    .checked_add(1)
                    .ok_or_else(|| Error::invalid_match("Future count overflow"))?;
            }
            out.uleb(limit - first)?;
            cursor.seek_row(first)?;
            for _ in first..limit {
                let item = cursor
                    .next()?
                    .ok_or_else(|| Error::corrupt_index("Future source replay ended early"))?;
                out.uleb(item.src - start)?;
                out.uleb(item.dst - item.src)?;
                out.uleb(item.len)?;
            }
            source_position = limit;
        }
        let mut position = start;
        if config.layout == Layout::Io {
            let take = carry.min(length);
            carry -= take;
            position += take;
            while position < end {
                if let Some(item) = next {
                    if item.dst < position {
                        return Err(Error::invalid_match("I/O destination cursor regressed"));
                    }
                    if item.dst == position {
                        out.append(&[IO_MATCH_TAG], true)?;
                        out.uleb(item.dst - item.src)?;
                        out.uleb(item.len)?;
                        let take = item.len.min(end - position);
                        carry = item.len - take;
                        position += take;
                        next = destination.next()?;
                        continue;
                    }
                }
                let gap_end = next.map_or(end, |m| m.dst.min(end));
                if gap_end <= position {
                    return Err(Error::invalid_match("I/O empty literal gap"));
                }
                out.append(&[IO_LITERAL_TAG], true)?;
                out.uleb(gap_end - position)?;
                input.range(
                    position,
                    gap_end - position,
                    "truncated v3 input spool literals",
                    |b| out.append(b, true),
                )?;
                position = gap_end;
            }
        } else {
            while position < end {
                let Some(item) = next else {
                    input.range(
                        position,
                        end - position,
                        "truncated v3 input spool literals",
                        |b| out.append(b, true),
                    )?;
                    break;
                };
                let item_end = item
                    .dst
                    .checked_add(item.len)
                    .ok_or_else(|| Error::invalid_match("match endpoint overflow"))?;
                if item_end <= position {
                    next = destination.next()?;
                    continue;
                }
                if item.dst >= end {
                    input.range(
                        position,
                        end - position,
                        "truncated v3 input spool literals",
                        |b| out.append(b, true),
                    )?;
                    break;
                }
                if item.dst > position {
                    input.range(
                        position,
                        item.dst - position,
                        "truncated v3 input spool literals",
                        |b| out.append(b, true),
                    )?;
                }
                position = item_end.min(end);
                if item_end <= end {
                    next = destination.next()?;
                }
            }
        }
        out.append(&crc.finalize_le(), true)?;
        payload_size = payload_size
            .checked_add(out.spool.len - before)
            .ok_or_else(|| Error::corrupt_record("payload size overflow"))?;
    }
    if carry != 0 || next.is_some() {
        return Err(Error::invalid_match(
            "unresolved selected destination at EOF",
        ));
    }
    if source.as_ref().is_some_and(|s| s.len() != source_position) {
        return Err(Error::invalid_match("Future registers not fully emitted"));
    }
    let body_end = out.spool.len;
    let (index_offset, index_length) = if config.layout == Layout::Index {
        out.uleb(summary.count)?;
        let mut cursor = selected.cursor()?;
        let mut previous_end = 0;
        while let Some(item) = cursor.next()? {
            out.uleb(item.dst - previous_end)?;
            out.uleb(item.dst - item.src)?;
            out.uleb(item.len)?;
            previous_end = item.dst + item.len;
        }
        let written = out.spool.len - body_end;
        if written != expected_index {
            return Err(Error::corrupt_index("IndexSection replay length mismatch"));
        }
        (body_end, written)
    } else {
        (0, 0)
    };
    let tail_start = out.spool.len;
    let archive_len = tail_start
        .checked_add(tail_len(header.checksum) as u64)
        .filter(|n| *n <= MAX_WIRE)
        .ok_or_else(|| Error::invalid_config("archive length exceeds MAX_WIRE"))?;
    if config.layout == Layout::Index {
        if index_offset.checked_add(index_length) != Some(tail_start) {
            return Err(Error::corrupt_index("Index locators inconsistent"));
        }
    } else if tail_start != body_end {
        return Err(Error::corrupt_record("sequential body end inconsistent"));
    }
    let mut prefix = [0; TAIL_PREFIX_LEN];
    prefix[..8].copy_from_slice(&TAIL_MAGIC);
    prefix[8] = VERSION;
    prefix[12..20].copy_from_slice(&archive_len.to_le_bytes());
    prefix[20..28].copy_from_slice(&index_offset.to_le_bytes());
    prefix[28..36].copy_from_slice(&index_length.to_le_bytes());
    prefix[36..44].copy_from_slice(&body_end.to_le_bytes());
    out.append(&prefix, true)?;
    out.append(&plaintext.finalize(), true)?;
    let encoded =
        std::mem::replace(&mut out.encoded, GlobalDigest::encoded(header.checksum)).finalize();
    out.append(&encoded, false)?;
    if out.spool.len != archive_len {
        return Err(Error::corrupt_record("staged length inconsistent"));
    }
    drop(destination);
    drop(source_cursor);
    if let Some(source) = &source {
        source.verify(session)?;
    }
    selected.verify(session)?;
    drop(source);
    drop(input);
    session.checkpoint(Operation::Flush)?;
    out.spool.flush().map_err(Error::temp_storage)?;
    let expected = *out.whole.finalize().as_bytes();
    let sealed = crate::codec::SealedStagedFile::seal(out.spool, archive_len, expected, session)?;
    let stats = compression_stats_from_header(
        &header,
        archive_len,
        payload_size,
        summary.count,
        summary.covered_bytes,
        summary.literal_bytes,
    )?;
    Ok((sealed, stats))
}

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

    fn bounded_archive(
        input: &[u8],
        config: &CompressionConfig,
        candidates: &[MatchCandidate],
        chunk: usize,
    ) -> (Vec<u8>, CompressionStats) {
        use crate::offload::contracts::{
            CandidateRow, CandidateRunSink, CandidateSink, CandidateValidator, Provenance,
            Scheduler,
        };
        let context = ResourceContext::with_resources(&config.resources).unwrap();
        let session = crate::offload::session::CompressionSession::new(
            &context,
            &config.resources,
            crate::offload::session::OffloadPolicy::default(),
        )
        .unwrap();
        let spool = spool_input(input, &config.resources, &context).unwrap();
        let mut sink = crate::offload::candidate_sink::ExternalCandidateSink::with_input_len(
            &session, spool.len, chunk,
        )
        .unwrap();
        for &candidate in candidates {
            sink.emit(CandidateRow {
                candidate,
                provenance: Provenance::EXTERNAL,
            })
            .unwrap();
        }
        let validated = crate::offload::normalize::Validator::new(
            &spool,
            config.effective_min_match().unwrap(),
        )
        .validate(sink.finish().unwrap(), &session)
        .unwrap();
        let builder = crate::offload::matches::DiskSelectedBuilder::new(
            spool.len,
            config.effective_min_match().unwrap(),
            &session,
        )
        .unwrap();
        let selected = crate::offload::schedule::DiskScheduler::new(builder)
            .schedule(validated, &session)
            .unwrap();
        let (mut stage, stats) =
            stage_selected_inner(&spool, config, &selected, &session, chunk.min(4096)).unwrap();
        let permit = crate::codec::prepare_publication(&mut stage, &session).unwrap();
        let mut out = Vec::new();
        crate::codec::copy_staged_exact(&mut stage, &mut out, permit, &session).unwrap();
        drop(stage);
        drop(selected);
        drop(spool);
        session.retry_cleanup().unwrap();
        assert_eq!(context.temp.current(), 0);
        assert_eq!(session.handle_usage().current, 7);
        assert_eq!(session.file_usage().current, 0);
        drop(session);
        assert_eq!(context.memory.current(), 0);
        (out, stats)
    }

    #[test]
    fn disk_scheduler_writer_matches_ram_bytes_all_layouts_checksums_and_methods() {
        let triples = [(100, 120, 64), (0, 1000, 1100), (1000, 2100, 64)];
        let data = materialize(2300, &triples);
        let rows = [
            candidate(100, 120, 64, 5),
            candidate(0, 1000, 1100, 9),
            candidate(1000, 2100, 64, 2),
            candidate(0, 1000, 1100, 1),
        ];
        for method in [
            Method::M0Rep,
            Method::M1RollingCdc,
            Method::M2Order1Cdc,
            Method::M3FixedDigest,
            Method::M4Reread,
            Method::M5Exhaustive,
        ] {
            for layout in [Layout::Index, Layout::Future, Layout::Io] {
                for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                    let mut config = CompressionConfig::for_method(method);
                    config.layout = layout;
                    config.checksum = checksum;
                    config.block_size = 1024;
                    config.min_match = 32;
                    config.resources = test_resources();
                    config.max_distance = Some(4096);
                    let (expected, stats) = compress_bytes(&data, &config, rows);
                    let (actual, got) = bounded_archive(&data, &config, &rows, 2);
                    assert_eq!(actual, expected, "{method:?}/{layout:?}/{checksum:?}");
                    assert_eq!(got, stats);
                }
            }
        }
    }
    #[test]
    fn bounded_empty_literal_carry_ties_and_page_sized_selected_stores() {
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                let config = m1_config(layout, checksum);
                for data in [
                    Vec::new(),
                    b"abc".to_vec(),
                    (0..1025).map(|n| (n % 251) as u8).collect(),
                ] {
                    assert_eq!(
                        bounded_archive(&data, &config, &[], 3),
                        compress_bytes(&data, &config, [])
                    );
                }
                let data = materialize(3300, &[(0, 1000, 2200)]);
                let rows = [candidate(0, 1000, 2200, 0)];
                assert_eq!(
                    bounded_archive(&data, &config, &rows, 1),
                    compress_bytes(&data, &config, rows)
                );
                let data = vec![0; 3300];
                let rows = [
                    candidate(0, 100, 60, 8),
                    candidate(0, 100, 100, 1),
                    candidate(0, 160, 100, 4),
                    candidate(1, 160, 100, 0),
                ];
                assert_eq!(
                    bounded_archive(&data, &config, &rows, 2),
                    compress_bytes(&data, &config, rows)
                );
            }
        }
        let data = vec![0; 140_000];
        let rows = (0..1100)
            .map(|id| candidate((1099 - id) % 97, 200 + id * 100, 32, id))
            .collect::<Vec<_>>();
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            let config = m1_config(layout, Checksum::Blake3);
            assert_eq!(
                bounded_archive(&data, &config, &rows, 17),
                compress_bytes(&data, &config, rows.iter().copied())
            );
        }
    }

    fn selected_fixture<'a>(
        s: &'a crate::offload::session::CompressionSession,
    ) -> crate::offload::matches::DiskSelectedStore {
        use crate::offload::contracts::{SelectedBuilder, SelectedSummary};
        let mut b = crate::offload::matches::DiskSelectedBuilder::new(2048, 32, s).unwrap();
        b.push(Match {
            src: 0,
            dst: 1024,
            len: 32,
            origin_match_id: 0,
        })
        .unwrap();
        b.finish(SelectedSummary {
            count: 1,
            covered_bytes: 32,
            literal_bytes: 2016,
        })
        .unwrap()
    }
    #[test]
    fn bounded_writer_failures_publish_nothing_and_release_stage_resources() {
        use crate::offload::session::{CompressionSession, FaultAction, OffloadPolicy, Operation};
        for operation in [Operation::Read, Operation::Write, Operation::Flush] {
            let config = m1_config(Layout::Index, Checksum::Xxh3);
            let c = ResourceContext::with_resources(&config.resources).unwrap();
            let s =
                CompressionSession::new(&c, &config.resources, OffloadPolicy::default()).unwrap();
            let spool = spool_input(&vec![0; 2048][..], &config.resources, &c).unwrap();
            let selected = selected_fixture(&s);
            let before = c.temp.current();
            s.inject(operation, 0, FaultAction::Fail);
            assert!(stage_selected(&spool, &config, &selected, &s).is_err());
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), before);
            drop(selected);
            drop(spool);
            drop(s);
            assert_eq!(c.temp.current(), 0);
            assert_eq!(c.memory.current(), 0);
        }
        let config = m1_config(Layout::Future, Checksum::Xxh3);
        let c = ResourceContext::with_resources(&config.resources).unwrap();
        let s = CompressionSession::new(&c, &config.resources, OffloadPolicy::default()).unwrap();
        let spool = spool_input(&vec![0; 2048][..], &config.resources, &c).unwrap();
        let selected = selected_fixture(&s);
        spool.file.set_len(7).unwrap();
        assert!(stage_selected(&spool, &config, &selected, &s).is_err());
        s.retry_cleanup().unwrap();
        let before = c.memory.current();
        let held = c.memory.reserve(c.memory.limit() - before).unwrap();
        assert!(stage_selected(&spool, &config, &selected, &s).is_err());
        drop(held);
        let held = s
            .acquire_handles(s.policy().max_handles - s.handle_usage().current)
            .unwrap();
        assert!(stage_selected(&spool, &config, &selected, &s).is_err());
        drop(held);
        s.cancel();
        assert!(stage_selected(&spool, &config, &selected, &s).is_err());
        drop(selected);
        drop(spool);
        drop(s);
        assert_eq!(c.temp.current(), 0);
        assert_eq!(c.memory.current(), 0);
    }

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

    #[test]
    fn bounded_writer_temp_distance_and_cleanup_failure_contracts() {
        use crate::offload::session::{CompressionSession, FaultAction, OffloadPolicy, Operation};
        let mut config = m1_config(Layout::Index, Checksum::Blake3);
        let c = ResourceContext::with_resources(&config.resources).unwrap();
        let s = CompressionSession::new(&c, &config.resources, OffloadPolicy::default()).unwrap();
        let spool = spool_input(&vec![0; 2048][..], &config.resources, &c).unwrap();
        let selected = selected_fixture(&s);
        let before = c.temp.current();
        let hold = c.temp.reserve(c.temp.limit() - before).unwrap();
        let error = match stage_selected(&spool, &config, &selected, &s) {
            Err(e) => e,
            Ok(_) => panic!("temp limit must fail"),
        };
        assert_eq!(error.kind(), ErrorKind::TempBudgetExceeded);
        drop(hold);
        assert_eq!(c.temp.current(), before);
        config.max_distance = Some(512);
        let error = match stage_selected(&spool, &config, &selected, &s) {
            Err(e) => e,
            Ok(_) => panic!("selected distance must fail"),
        };
        assert_eq!(error.kind(), ErrorKind::InvalidMatch);
        config.max_distance = None;
        s.inject(Operation::Write, 0, FaultAction::Fail);
        s.inject(Operation::Remove, 0, FaultAction::Fail);
        assert!(stage_selected(&spool, &config, &selected, &s).is_err());
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), before);
        drop(selected);
        drop(spool);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.handle_usage().current, 7);
        drop(s);
        assert_eq!(c.memory.current(), 0);
    }

    #[test]
    fn bounded_input_error_accounting_and_writer_digest_preflight() {
        use crate::offload::session::{CompressionSession, FaultAction, OffloadPolicy, Operation};
        let config = m1_config(Layout::Index, Checksum::Blake3);
        let c = ResourceContext::with_resources(&config.resources).unwrap();
        let s = CompressionSession::new(&c, &config.resources, OffloadPolicy::default()).unwrap();
        let spool = spool_input(&vec![0; 2048][..], &config.resources, &c).unwrap();
        let selected = selected_fixture(&s);
        {
            let mut input = BoundedInput::new(&spool, &s).unwrap();
            let before = s.io_used();
            s.inject(Operation::Read, 0, FaultAction::Fail);
            assert!(input.range(0, 10, "literal", |_| Ok(())).is_err());
            assert_eq!(s.io_used(), before);
            assert_eq!(s.io_reserved(), 0);
            input.range(0, 7, "literal", |_| Ok(())).unwrap();
            assert_eq!(s.io_used() - before, 7);
        }
        let (mut stage, _) = stage_selected(&spool, &config, &selected, &s).unwrap();
        // The expected digest came from appends, not a disk-derived seal hash.
        // Mutating actual stage without changing length must fail preflight.
        {
            let dir = s.temp_directory().unwrap();
            for entry in std::fs::read_dir(&*dir).unwrap() {
                let path = entry.unwrap().path();
                let mut f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .unwrap();
                let mut prefix = [0; 8];
                f.read_exact(&mut prefix).unwrap();
                if &prefix != b"SRPOFF01" {
                    f.seek(SeekFrom::Start(80)).unwrap();
                    f.write_all(&[255]).unwrap();
                    f.flush().unwrap();
                    break;
                }
            }
        }
        assert_eq!(
            crate::codec::prepare_publication(&mut stage, &s)
                .unwrap_err()
                .kind(),
            ErrorKind::CorruptRecord
        );
        assert_eq!(s.io_reserved(), 0);
        drop(stage);
        drop(selected);
        drop(spool);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.handle_usage().current, 7);
    }
}
