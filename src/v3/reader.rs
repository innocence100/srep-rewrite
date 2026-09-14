//! SREP-NG v3 reader.
//!
//! The reader deliberately keeps the wire parser here instead of sharing the
//! removed NGv2 record decoder. In particular, v3 has no frames: block boundaries are
//! derived from the header and the layout grammar, and the fixed tail is the
//! only footer structure.

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};

use tempfile::{Builder, NamedTempFile};

use crate::codec::{ArchiveInfo, CompressionStats, TempSpool, copy_spool};
use crate::config::{Checksum, Layout, ResourceConfig};
use crate::error::{Error, Result};
use crate::format_v3::{
    ArchiveHeader, ArchiveTail, Crc32c, GlobalDigest, MAX_ULEB128_LEN, MAX_WIRE,
    POSITIVE_GAIN_MIN_LEN, TAIL_PREFIX_LEN, block_len_at, parse_archive_header, parse_archive_tail,
    tail_len, validate_tail_layout,
};
use crate::match_ir::{InspectedMatches, Match};
use crate::resource::{BudgetedVec, MemoryBudget, Reservation, ResourceContext};
use crate::v3::{self, V3Decoded};

const IO_BUFFER: usize = 64 * 1024;

/// Decode a complete v3 archive.  `input` must start at byte zero.
pub fn decode<R: Read, W: Write>(
    input: R,
    output: W,
    resources: &ResourceConfig,
    context: &ResourceContext,
    write_output: bool,
) -> Result<CompressionStats> {
    let decoded = decode_archive(input, output, resources, context, write_output, false)?;
    Ok(decoded.stats)
}

/// Validate and inspect a complete archive without publishing plaintext.
pub fn inspect<R: Read>(
    input: R,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<ArchiveInfo> {
    Ok(decode_archive(input, std::io::sink(), resources, context, false, false)?.info)
}

/// Validate an archive and return the actual match IR carried by it.
pub fn inspect_matches<R: Read>(
    input: R,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<InspectedMatches> {
    let decoded = decode_archive(input, std::io::sink(), resources, context, false, true)?;
    decoded
        .matches
        .ok_or_else(|| Error::corrupt_record("v3 match collection was not produced"))
}

/// The single reconstruction path used by all public reader entry points.
pub(crate) fn decode_archive<R: Read, W: Write>(
    mut input: R,
    mut output: W,
    resources: &ResourceConfig,
    context: &ResourceContext,
    write_output: bool,
    collect_matches: bool,
) -> Result<V3Decoded> {
    validate_reader_resources(resources, context)?;

    let mut header_bytes = [0u8; crate::format_v3::HEADER_LEN];
    input
        .read_exact(&mut header_bytes)
        .map_err(|error| Error::map_eof(error, "truncated v3 header"))?;
    let header = parse_archive_header(&header_bytes)?;
    if header.uncompressed_length > resources.output_limit {
        return Err(Error::output_limit(
            "v3 output exceeds configured output limit",
        ));
    }
    let effective_min = header.effective_min_match()?;

    // History is always seekable and is never exposed as part of V3Decoded.
    // TempSpool accounts its physical growth through the shared temp ledger.
    fs::create_dir_all(&resources.temp_dir).map_err(Error::temp_storage)?;
    let mut history = History::new(resources, context)?;
    let mut staged = if write_output {
        Some(TempSpool::new(resources, context)?)
    } else {
        None
    };
    let mut plaintext = GlobalDigest::plaintext(header.checksum);
    let mut encoded = GlobalDigest::encoded(header.checksum);
    encoded.update(&header_bytes);

    let result = if header.layout == Layout::Index {
        // Index-LZ is the one layout for which the tail and index precede
        // reconstruction.  The input is copied only after the header tells us
        // that this is necessary.
        let archive = ArchiveSpool::from_header(input, &header_bytes, resources, context)?;
        decode_index(
            &archive,
            &header,
            effective_min,
            resources,
            context,
            &mut history,
            &mut staged,
            &mut plaintext,
            &mut encoded,
            collect_matches,
        )
    } else {
        let mut stream = StreamInput {
            input: &mut input,
            position: crate::format_v3::HEADER_LEN as u64,
            encoded: &mut encoded,
        };
        decode_sequential(
            &mut stream,
            &header,
            effective_min,
            resources,
            context,
            &mut history,
            &mut staged,
            &mut plaintext,
            collect_matches,
        )
    };

    let mut decoded = match result {
        Ok(value) => value,
        Err(error) => {
            if let Some(spool) = staged.as_mut() {
                return Err(spool.take_budget_error().unwrap_or(error));
            }
            return Err(error);
        }
    };

    if let Some(mut spool) = staged {
        spool.rewind()?;
        copy_spool(&mut spool.file, &mut output)?;
    }
    decoded.info = v3::archive_info_from_header(&header, &decoded.stats);
    Ok(decoded)
}

fn validate_reader_resources(resources: &ResourceConfig, context: &ResourceContext) -> Result<()> {
    if resources.memory == 0 {
        return Err(Error::invalid_config("memory limit must be positive"));
    }
    if resources.temp_limit == 0 {
        return Err(Error::invalid_config("temporary limit must be positive"));
    }
    if resources.output_limit > MAX_WIRE {
        return Err(Error::invalid_config("output limit is outside wire limits"));
    }
    context.validate()
}

fn read_fixed<R: Read>(input: &mut R, len: usize, what: &str) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    input
        .read_exact(&mut bytes)
        .map_err(|error| Error::map_eof(error, format!("truncated {what}")))?;
    Ok(bytes)
}

struct ArchiveSpool {
    _file: File,
    read_file: RefCell<File>,
    temp: NamedTempFile,
    len: u64,
    _reservation: Reservation,
}

impl ArchiveSpool {
    fn from_header<R: Read>(
        mut input: R,
        header: &[u8],
        resources: &ResourceConfig,
        context: &ResourceContext,
    ) -> Result<Self> {
        let temp = crate::codec::temp_builder(Builder::new())
            .prefix("srep-v3-input-")
            .tempfile_in(&resources.temp_dir)
            .map_err(Error::temp_storage)?;
        let mut file = temp.reopen().map_err(Error::temp_storage)?;
        let mut reservation = context.temp.reserve(0)?;
        reservation.grow(header.len() as u64)?;
        file.write_all(header).map_err(Error::temp_storage)?;
        let mut len = header.len() as u64;
        let mut buffer = [0u8; IO_BUFFER];
        loop {
            let count = input.read(&mut buffer).map_err(Error::input_io)?;
            if count == 0 {
                break;
            }
            let new_len = len
                .checked_add(count as u64)
                .ok_or_else(|| Error::output_limit("archive length overflows"))?;
            if new_len > MAX_WIRE {
                return Err(Error::output_limit("archive exceeds wire size limit"));
            }
            reservation.grow(count as u64)?;
            if let Err(error) = file.write_all(&buffer[..count]) {
                reservation.shrink(count as u64);
                return Err(Error::temp_storage(error));
            }
            len = new_len;
        }
        file.flush().map_err(Error::temp_storage)?;
        file.seek(SeekFrom::Start(0)).map_err(Error::temp_storage)?;
        Ok(Self {
            _file: file,
            read_file: RefCell::new(temp.reopen().map_err(Error::temp_storage)?),
            temp,
            len,
            _reservation: reservation,
        })
    }

    fn read_at(&self, position: u64, bytes: &mut [u8]) -> Result<()> {
        let end = position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Error::corrupt_record("archive read offset overflows"))?;
        if end > self.len {
            return Err(Error::truncated("truncated v3 archive"));
        }
        let mut file = self.read_file.borrow_mut();
        file.seek(SeekFrom::Start(position))
            .map_err(Error::temp_storage)?;
        file.read_exact(bytes)
            .map_err(|error| Error::map_eof(error, "truncated v3 archive"))
    }
}

struct StreamInput<'a, R> {
    input: &'a mut R,
    position: u64,
    encoded: &'a mut GlobalDigest,
}

impl<R: Read> StreamInput<'_, R> {
    fn read_byte(&mut self) -> Result<u8> {
        let mut byte = [0u8; 1];
        self.input
            .read_exact(&mut byte)
            .map_err(|error| Error::map_eof(error, "truncated v3 record"))?;
        self.position = self.position.saturating_add(1);
        self.encoded.update(&byte);
        Ok(byte[0])
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<()> {
        self.input
            .read_exact(bytes)
            .map_err(|error| Error::map_eof(error, "truncated v3 record"))?;
        self.position = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Error::corrupt_record("archive cursor overflows"))?;
        self.encoded.update(bytes);
        Ok(())
    }

    fn read_uleb(&mut self) -> Result<u64> {
        read_uleb_bytes(|_| self.read_byte(), false)
    }

    fn read_tail(&mut self, checksum: Checksum) -> Result<Vec<u8>> {
        let bytes = read_fixed(self.input, tail_len(checksum), "v3 tail")?;
        self.position = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Error::corrupt_record("archive cursor overflows"))?;
        self.encoded
            .update(&bytes[..TAIL_PREFIX_LEN + checksum.width()]);
        Ok(bytes)
    }
}

fn read_uleb_bytes<F>(mut next: F, as_index: bool) -> Result<u64>
where
    F: FnMut(usize) -> Result<u8>,
{
    let mut result = 0u64;
    let mut shift = 0u32;
    for index in 0..MAX_ULEB128_LEN {
        let byte = next(index)?;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            if byte == 0 && index > 0 {
                return Err(if as_index {
                    Error::corrupt_index("non-shortest index ULEB128")
                } else {
                    Error::corrupt_record("non-shortest ULEB128")
                });
            }
            if result > MAX_WIRE {
                return Err(if as_index {
                    Error::corrupt_index("index ULEB128 exceeds MAX_WIRE")
                } else {
                    Error::corrupt_record("ULEB128 exceeds MAX_WIRE")
                });
            }
            return Ok(result);
        }
        shift += 7;
    }
    Err(if as_index {
        Error::corrupt_index("index ULEB128 continuation after byte 9")
    } else {
        Error::corrupt_record("ULEB128 continuation after byte 9")
    })
}

struct IndexCursor<'a> {
    archive: &'a ArchiveSpool,
    position: u64,
    end: u64,
    buffer: [u8; IO_BUFFER],
    buffer_len: usize,
    buffer_offset: usize,
}

impl IndexCursor<'_> {
    fn read_byte(&mut self) -> Result<u8> {
        if self.position >= self.end {
            return Err(Error::corrupt_index("IndexSection is truncated"));
        }
        if self.buffer_offset == self.buffer_len {
            let amount = usize::try_from((self.end - self.position).min(IO_BUFFER as u64))
                .map_err(|_| Error::corrupt_index("IndexSection buffer length overflows"))?;
            self.archive
                .read_at(self.position, &mut self.buffer[..amount])?;
            self.buffer_len = amount;
            self.buffer_offset = 0;
        }
        let byte = self.buffer[self.buffer_offset];
        self.buffer_offset += 1;
        self.position += 1;
        Ok(byte)
    }

    fn read_uleb(&mut self) -> Result<u64> {
        read_uleb_bytes(|_| self.read_byte(), true)
    }
}

struct ArchiveBodyCursor<'a> {
    archive: &'a ArchiveSpool,
    position: u64,
    end: u64,
    encoded: &'a mut GlobalDigest,
    buffer: [u8; IO_BUFFER],
    buffer_len: usize,
    buffer_offset: usize,
}

impl ArchiveBodyCursor<'_> {
    fn read_byte(&mut self) -> Result<u8> {
        let mut byte = [0u8; 1];
        self.read_exact(&mut byte)?;
        Ok(byte[0])
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<()> {
        let next = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Error::corrupt_record("block body cursor overflows"))?;
        if next > self.end {
            return Err(Error::corrupt_record("block body exceeds body_end"));
        }
        let mut written = 0usize;
        while written < bytes.len() {
            if self.buffer_offset == self.buffer_len {
                let amount = usize::try_from((self.end - self.position).min(IO_BUFFER as u64))
                    .map_err(|_| Error::corrupt_record("body buffer length overflows"))?;
                if amount == 0 {
                    return Err(Error::corrupt_record("block body exceeds body_end"));
                }
                self.archive
                    .read_at(self.position, &mut self.buffer[..amount])?;
                self.buffer_len = amount;
                self.buffer_offset = 0;
            }
            let amount = (self.buffer_len - self.buffer_offset).min(bytes.len() - written);
            bytes[written..written + amount]
                .copy_from_slice(&self.buffer[self.buffer_offset..self.buffer_offset + amount]);
            self.buffer_offset += amount;
            self.position += amount as u64;
            self.encoded.update(&bytes[written..written + amount]);
            written += amount;
        }
        Ok(())
    }
}

struct History {
    spool: TempSpool,
    pending: [u8; IO_BUFFER],
    pending_len: usize,
    read_file: File,
    read_cache: [u8; IO_BUFFER],
    read_cache_start: u64,
    read_cache_len: usize,
}

impl History {
    fn new(resources: &ResourceConfig, context: &ResourceContext) -> Result<Self> {
        let spool = TempSpool::new(resources, context)?;
        let read_file = spool._temp.reopen().map_err(Error::temp_storage)?;
        Ok(Self {
            spool,
            pending: [0; IO_BUFFER],
            pending_len: 0,
            read_file,
            read_cache: [0; IO_BUFFER],
            read_cache_start: 0,
            read_cache_len: 0,
        })
    }

    fn len(&self) -> u64 {
        self.spool.len + self.pending_len as u64
    }

    fn append(&mut self, byte: u8) -> Result<()> {
        self.pending[self.pending_len] = byte;
        self.pending_len += 1;
        if self.pending_len == IO_BUFFER {
            self.flush_pending()?;
        }
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<()> {
        if self.pending_len != 0 {
            self.spool.append(&self.pending[..self.pending_len])?;
            self.pending_len = 0;
        }
        Ok(())
    }

    fn read_byte(&mut self, position: u64) -> Result<u8> {
        if position >= self.spool.len {
            let index = position
                .checked_sub(self.spool.len)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| Error::invalid_match("history offset exceeds platform limits"))?;
            return self
                .pending
                .get(index)
                .copied()
                .ok_or_else(|| Error::invalid_match("match source is not yet available"));
        }
        if position < self.read_cache_start
            || position >= self.read_cache_start + self.read_cache_len as u64
        {
            self.read_file
                .seek(SeekFrom::Start(position))
                .map_err(Error::temp_storage)?;
            let amount = usize::try_from((self.spool.len - position).min(IO_BUFFER as u64))
                .map_err(|_| Error::temp_storage_context("history read length overflows"))?;
            self.read_file
                .read_exact(&mut self.read_cache[..amount])
                .map_err(|error| Error::map_eof(error, "truncated v3 history"))?;
            self.read_cache_start = position;
            self.read_cache_len = amount;
        }
        Ok(self.read_cache[(position - self.read_cache_start) as usize])
    }

    fn finish(&mut self) -> Result<()> {
        self.flush_pending()
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_index(
    archive: &ArchiveSpool,
    header: &ArchiveHeader,
    effective_min: u64,
    _resources: &ResourceConfig,
    context: &ResourceContext,
    history: &mut History,
    staged: &mut Option<TempSpool>,
    plaintext: &mut GlobalDigest,
    encoded: &mut GlobalDigest,
    collect_matches: bool,
) -> Result<V3Decoded> {
    let tail_size = tail_len(header.checksum) as u64;
    let tail_start = archive
        .len
        .checked_sub(tail_size)
        .ok_or_else(|| Error::truncated("archive is shorter than its fixed tail"))?;
    let mut tail_storage = [0u8; crate::format_v3::TAIL_LEN_BLAKE3];
    let tail_width = tail_len(header.checksum);
    archive.read_at(tail_start, &mut tail_storage[..tail_width])?;
    let tail_bytes = &tail_storage[..tail_width];
    let tail = parse_archive_tail(tail_bytes, header.checksum)?;
    let derived = validate_tail_layout(&tail, header.layout, header.checksum, archive.len)?;

    let matches = parse_index(
        archive,
        derived.index_offset,
        derived.tail_start,
        header,
        effective_min,
        &context.memory,
    )?;
    let match_count = matches.len() as u64;
    let mut body = ArchiveBodyCursor {
        archive,
        position: crate::format_v3::HEADER_LEN as u64,
        end: derived.body_end,
        encoded,
        buffer: [0; IO_BUFFER],
        buffer_len: 0,
        buffer_offset: 0,
    };
    let mut crc_digest = BlockOutput::new(staged, plaintext, history);
    let mut covered = 0u64;
    for item in matches.iter() {
        covered = covered
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("match coverage overflows"))?;
    }
    let literal_bytes = header
        .uncompressed_length
        .checked_sub(covered)
        .ok_or_else(|| Error::invalid_match("match coverage exceeds plaintext"))?;
    let blocks = header.block_count()?;
    let mut match_cursor = 0usize;
    for block_id in 0..blocks {
        let start = block_id
            .checked_mul(header.block_size)
            .ok_or_else(|| Error::corrupt_record("block start overflows"))?;
        let length = block_len_at(header.uncompressed_length, header.block_size, block_id)?;
        let before = body.position;
        decode_index_block(
            &mut body,
            start,
            length,
            &matches,
            &mut match_cursor,
            &mut crc_digest,
        )?;
        let mut crc = [0u8; 4];
        body.read_exact(&mut crc)?;
        crc_digest.finish_block(&crc, length)?;
        if body.position < before {
            return Err(Error::corrupt_record("block cursor moved backwards"));
        }
        let _ = length;
    }
    if body.position != derived.body_end {
        return Err(Error::corrupt_record("Index-LZ body underconsumed"));
    }
    // The table is hashed exactly once, in physical order, after the body.
    hash_archive_range(archive, derived.index_offset, derived.tail_start, encoded)?;
    encoded.update(&tail_bytes[..TAIL_PREFIX_LEN + header.checksum.width()]);
    verify_global_digests(header.checksum, &tail, plaintext, encoded)?;
    history.finish()?;
    let archive_size = archive.len;
    let payload_size = derived
        .body_end
        .checked_sub(crate::format_v3::HEADER_LEN as u64)
        .ok_or_else(|| Error::corrupt_record("payload size underflows"))?;
    let stats = v3::compression_stats_from_header(
        header,
        archive_size,
        payload_size,
        match_count,
        covered,
        literal_bytes,
    )?;
    let owned = if collect_matches {
        Some(v3::inspected_matches(matches))
    } else {
        None
    };
    Ok(V3Decoded::new(
        v3::archive_info_from_header(header, &stats),
        stats,
        owned,
    ))
}

fn parse_index(
    archive: &ArchiveSpool,
    start: u64,
    end: u64,
    header: &ArchiveHeader,
    effective_min: u64,
    memory: &MemoryBudget,
) -> Result<BudgetedVec<Match>> {
    if start >= end {
        return Err(Error::corrupt_index("IndexSection must not be empty"));
    }
    let mut cursor = IndexCursor {
        archive,
        position: start,
        end,
        buffer: [0; IO_BUFFER],
        buffer_len: 0,
        buffer_offset: 0,
    };
    let count = cursor.read_uleb()?;
    let available = end - cursor.position;
    let max_by_bytes = available / 3;
    let max_by_ir =
        crate::format_v3::max_selected_matches(header.uncompressed_length, effective_min);
    if count > max_by_bytes || count > max_by_ir || count > MAX_WIRE {
        return Err(Error::corrupt_index(
            "IndexSection match count exceeds bounds",
        ));
    }
    let capacity = usize::try_from(count)
        .map_err(|_| Error::memory_limit("IndexSection count exceeds platform limits"))?;
    let mut matches = BudgetedVec::with_capacity(capacity, memory)?;
    let mut previous_end = 0u64;
    let mut previous: Option<Match> = None;
    for origin_id in 0..count {
        let dst_gap = cursor.read_uleb()?;
        let distance = cursor.read_uleb()?;
        let len = cursor.read_uleb()?;
        if distance == 0 {
            return Err(Error::invalid_match("Index-LZ distance must be positive"));
        }
        let dst = previous_end
            .checked_add(dst_gap)
            .ok_or_else(|| Error::invalid_match("Index-LZ destination overflows"))?;
        let src = dst
            .checked_sub(distance)
            .ok_or_else(|| Error::invalid_match("Index-LZ source underflows"))?;
        let finish = dst
            .checked_add(len)
            .ok_or_else(|| Error::invalid_match("Index-LZ match endpoint overflows"))?;
        validate_match(src, dst, len, header, effective_min)?;
        if let Some(previous_match) = previous
            && ((dst, src, std::cmp::Reverse(len))
                < (
                    previous_match.dst,
                    previous_match.src,
                    std::cmp::Reverse(previous_match.len),
                )
                || dst < previous_match.dst + previous_match.len)
        {
            return Err(Error::invalid_match("Index-LZ matches are not canonical"));
        }
        let item = Match {
            src,
            dst,
            len,
            origin_match_id: origin_id,
        };
        matches.push(item)?;
        previous = Some(item);
        previous_end = finish;
    }
    if cursor.position != end {
        return Err(Error::corrupt_index("IndexSection has trailing bytes"));
    }
    Ok(matches)
}

fn hash_archive_range(
    archive: &ArchiveSpool,
    start: u64,
    end: u64,
    digest: &mut GlobalDigest,
) -> Result<()> {
    let mut file = archive.temp.reopen().map_err(Error::temp_storage)?;
    file.seek(SeekFrom::Start(start))
        .map_err(Error::temp_storage)?;
    let mut position = start;
    let mut buffer = [0u8; IO_BUFFER];
    while position < end {
        let amount = usize::try_from((end - position).min(IO_BUFFER as u64))
            .map_err(|_| Error::corrupt_record("archive hash chunk exceeds platform limits"))?;
        file.read_exact(&mut buffer[..amount])
            .map_err(|error| Error::map_eof(error, "truncated encoded archive"))?;
        digest.update(&buffer[..amount]);
        position += amount as u64;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_index_block(
    body: &mut ArchiveBodyCursor<'_>,
    start: u64,
    length: u64,
    matches: &[Match],
    match_cursor: &mut usize,
    out: &mut BlockOutput<'_>,
) -> Result<()> {
    for offset in 0..length {
        let position = start + offset;
        while *match_cursor < matches.len()
            && matches[*match_cursor]
                .dst
                .checked_add(matches[*match_cursor].len)
                .ok_or_else(|| Error::invalid_match("match endpoint overflows"))?
                <= position
        {
            *match_cursor += 1;
        }
        if let Some(item) = matches
            .get(*match_cursor)
            .filter(|item| item.dst <= position)
        {
            let match_offset = position - item.dst;
            let distance = item
                .dst
                .checked_sub(item.src)
                .ok_or_else(|| Error::invalid_match("Index-LZ match distance underflows"))?;
            let source = item
                .src
                .checked_add(match_offset % distance)
                .ok_or_else(|| Error::invalid_match("Index-LZ source overflows"))?;
            let byte = out.history.read_byte(source)?;
            out.emit(byte)?;
        } else {
            let byte = body.read_byte()?;
            out.emit(byte)?;
        }
    }
    Ok(())
}

struct BlockOutput<'a> {
    staged: &'a mut Option<TempSpool>,
    plaintext: &'a mut GlobalDigest,
    history: &'a mut History,
    crc: Crc32c,
    block_len: u64,
    emitted: u64,
    output_pending: [u8; IO_BUFFER],
    output_pending_len: usize,
}

impl<'a> BlockOutput<'a> {
    fn new(
        staged: &'a mut Option<TempSpool>,
        plaintext: &'a mut GlobalDigest,
        history: &'a mut History,
    ) -> Self {
        Self {
            staged,
            plaintext,
            history,
            crc: Crc32c::new(),
            block_len: 0,
            emitted: 0,
            output_pending: [0; IO_BUFFER],
            output_pending_len: 0,
        }
    }

    fn emit(&mut self, byte: u8) -> Result<()> {
        self.history.append(byte)?;
        self.plaintext.update(&[byte]);
        self.crc.update(&[byte]);
        self.emitted = self.emitted.saturating_add(1);
        if self.staged.is_some() {
            self.output_pending[self.output_pending_len] = byte;
            self.output_pending_len += 1;
            if self.output_pending_len == IO_BUFFER {
                self.flush_output_pending()?;
            }
        }
        Ok(())
    }

    fn flush_output_pending(&mut self) -> Result<()> {
        if self.output_pending_len != 0 {
            if let Some(staged) = self.staged.as_mut() {
                staged.append(&self.output_pending[..self.output_pending_len])?;
            }
            self.output_pending_len = 0;
        }
        Ok(())
    }

    fn finish_block(&mut self, expected: &[u8; 4], expected_len: u64) -> Result<()> {
        if self.emitted != expected_len {
            return Err(Error::corrupt_record("decoded block length is incorrect"));
        }
        if self.crc.clone().finalize_le() != *expected {
            return Err(Error::checksum_mismatch("v3 block CRC32C mismatch"));
        }
        self.flush_output_pending()?;
        self.crc = Crc32c::new();
        self.block_len = self.emitted;
        self.emitted = 0;
        Ok(())
    }
}

fn verify_global_digests(
    checksum: Checksum,
    tail: &ArchiveTail,
    plaintext: &mut GlobalDigest,
    encoded: &mut GlobalDigest,
) -> Result<()> {
    if plaintext.clone().finalize() != tail.plaintext_digest {
        return Err(Error::checksum_mismatch("v3 plaintext digest mismatch"));
    }
    if encoded.clone().finalize() != tail.encoded_digest {
        return Err(Error::checksum_mismatch(format!(
            "v3 encoded {} digest mismatch",
            checksum.name()
        )));
    }
    Ok(())
}

fn validate_match(
    src: u64,
    dst: u64,
    len: u64,
    header: &ArchiveHeader,
    effective_min: u64,
) -> Result<()> {
    if src >= dst {
        return Err(Error::invalid_match(
            "match source must precede destination",
        ));
    }
    if len < effective_min || len < POSITIVE_GAIN_MIN_LEN {
        return Err(Error::invalid_match(
            "match is shorter than effective minimum",
        ));
    }
    let end = dst
        .checked_add(len)
        .ok_or_else(|| Error::invalid_match("match endpoint overflows"))?;
    if end > header.uncompressed_length || src >= header.uncompressed_length {
        return Err(Error::invalid_match("match interval exceeds plaintext"));
    }
    if header.max_distance != 0 && dst - src > header.max_distance {
        return Err(Error::invalid_match("match exceeds max_distance"));
    }
    Ok(())
}

struct FutureRegister {
    item: Match,
    period_len: u64,
    period: Option<Box<FuturePeriod>>,
}

#[derive(Clone, Copy)]
struct DestinationEntry {
    index: usize,
    dst: u64,
    end: u64,
}

fn destination_before(left: DestinationEntry, right: DestinationEntry) -> bool {
    (left.dst, left.end, left.index) < (right.dst, right.end, right.index)
}

fn destination_push(
    queue: &mut BudgetedVec<DestinationEntry>,
    entry: DestinationEntry,
) -> Result<()> {
    queue.push(entry)?;
    let mut index = queue.len() - 1;
    while index > 0 {
        let parent = (index - 1) / 2;
        if !destination_before(queue[index], queue[parent]) {
            break;
        }
        queue.as_mut_slice().swap(index, parent);
        index = parent;
    }
    Ok(())
}

fn destination_peek(queue: &BudgetedVec<DestinationEntry>) -> Option<DestinationEntry> {
    queue.get(0).copied()
}

fn destination_pop(queue: &mut BudgetedVec<DestinationEntry>) -> Option<DestinationEntry> {
    if queue.is_empty() {
        return None;
    }
    let last = queue.len() - 1;
    queue.as_mut_slice().swap(0, last);
    let result = queue.remove(last);
    let mut index = 0;
    loop {
        let left = index * 2 + 1;
        if left >= queue.len() {
            break;
        }
        let right = left + 1;
        let child = if right < queue.len() && destination_before(queue[right], queue[left]) {
            right
        } else {
            left
        };
        if !destination_before(queue[child], queue[index]) {
            break;
        }
        queue.as_mut_slice().swap(index, child);
        index = child;
    }
    Some(result)
}

fn prepare_future_capacity<T, F>(
    buffer: &mut BudgetedVec<T>,
    desired: usize,
    mut value: F,
) -> Result<()>
where
    F: FnMut() -> T,
{
    let original_len = buffer.len();
    while buffer.capacity() < desired {
        buffer.push(value())?;
    }
    while buffer.len() > original_len {
        let _ = buffer.remove(buffer.len() - 1);
    }
    Ok(())
}

enum FuturePeriod {
    Memory {
        bytes: BudgetedVec<u8>,
        _memory_reservation: Reservation,
    },
    Spilled(Box<SpilledFuturePeriod>),
}

struct SpilledFuturePeriod {
    spool: TempSpool,
    read_file: File,
    pending: [u8; 256],
    pending_len: usize,
    len: u64,
    _memory_reservation: Reservation,
}

const FUTURE_PERIOD_BOX_BYTES: u64 = std::mem::size_of::<FuturePeriod>() as u64;
const SPILLED_FUTURE_PERIOD_BOX_BYTES: u64 = std::mem::size_of::<SpilledFuturePeriod>() as u64;

impl FuturePeriod {
    fn new(
        period_len: u64,
        resources: &ResourceConfig,
        context: &ResourceContext,
    ) -> Result<Box<Self>> {
        let memory = usize::try_from(period_len)
            .ok()
            .map(|capacity| BudgetedVec::with_capacity(capacity, &context.memory));
        match memory {
            Some(Ok(period)) => match context.memory.reserve(FUTURE_PERIOD_BOX_BYTES) {
                Ok(box_reservation) => Ok(Box::new(Self::Memory {
                    bytes: period,
                    _memory_reservation: box_reservation,
                })),
                Err(error) if error.kind() == crate::error::ErrorKind::MemoryBudgetExceeded => {
                    drop(period);
                    Self::new_spilled(resources, context)
                }
                Err(error) => Err(error),
            },
            Some(Err(error)) if error.kind() == crate::error::ErrorKind::MemoryBudgetExceeded => {
                Self::new_spilled(resources, context)
            }
            Some(Err(error)) => Err(error),
            None => Self::new_spilled(resources, context),
        }
    }

    fn new_spilled(resources: &ResourceConfig, context: &ResourceContext) -> Result<Box<Self>> {
        let memory_reservation = context.memory.reserve(
            FUTURE_PERIOD_BOX_BYTES
                .checked_add(SPILLED_FUTURE_PERIOD_BOX_BYTES)
                .ok_or_else(|| Error::memory_limit("Future spill bookkeeping overflows"))?,
        )?;
        let spool = TempSpool::new(resources, context)?;
        let read_file = spool._temp.reopen().map_err(Error::temp_storage)?;
        Ok(Box::new(Self::Spilled(Box::new(SpilledFuturePeriod {
            spool,
            read_file,
            pending: [0; 256],
            pending_len: 0,
            len: 0,
            _memory_reservation: memory_reservation,
        }))))
    }

    fn len(&self) -> u64 {
        match self {
            Self::Memory { bytes, .. } => bytes.len() as u64,
            Self::Spilled(period) => period.len,
        }
    }

    fn push(&mut self, byte: u8) -> Result<()> {
        match self {
            Self::Memory { bytes, .. } => bytes.push(byte),
            Self::Spilled(period) => {
                period.pending[period.pending_len] = byte;
                period.pending_len += 1;
                period.len += 1;
                if period.pending_len == period.pending.len() {
                    period.flush_pending()?;
                }
                Ok(())
            }
        }
    }

    fn byte(&mut self, offset: u64) -> Result<u8> {
        match self {
            Self::Memory { bytes, .. } => bytes
                .get(usize::try_from(offset).map_err(|_| {
                    Error::invalid_match("Future period offset exceeds platform limits")
                })?)
                .copied()
                .ok_or_else(|| Error::invalid_match("Future period byte is unavailable")),
            Self::Spilled(period) => {
                period.flush_pending()?;
                period
                    .read_file
                    .seek(SeekFrom::Start(offset))
                    .map_err(Error::temp_storage)?;
                let mut byte = [0u8; 1];
                period
                    .read_file
                    .read_exact(&mut byte)
                    .map_err(|error| Error::map_eof(error, "truncated Future period spill"))?;
                Ok(byte[0])
            }
        }
    }
}

impl SpilledFuturePeriod {
    fn flush_pending(&mut self) -> Result<()> {
        if self.pending_len != 0 {
            self.spool.append(&self.pending[..self.pending_len])?;
            self.pending_len = 0;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_sequential<R: Read>(
    stream: &mut StreamInput<'_, R>,
    header: &ArchiveHeader,
    effective_min: u64,
    _resources: &ResourceConfig,
    context: &ResourceContext,
    history: &mut History,
    staged: &mut Option<TempSpool>,
    plaintext: &mut GlobalDigest,
    collect_matches: bool,
) -> Result<V3Decoded> {
    let blocks = header.block_count()?;
    let mut match_metadata = if collect_matches {
        Some(BudgetedVec::new(&context.memory)?)
    } else {
        None
    };
    let mut future_regs = BudgetedVec::new(&context.memory)?;
    let mut active_collectors = BudgetedVec::new(&context.memory)?;
    let mut active_destinations: BudgetedVec<DestinationEntry> = BudgetedVec::new(&context.memory)?;
    let mut free_registers = BudgetedVec::new(&context.memory)?;
    let mut io_carry: Option<(Match, u64)> = None;
    let mut previous_future: Option<(u64, u64, u64, u64)> = None;
    let mut match_count = 0u64;
    let mut covered = 0u64;
    let mut payload_size = 0u64;

    for block_id in 0..blocks {
        let start = block_id
            .checked_mul(header.block_size)
            .ok_or_else(|| Error::corrupt_record("block start overflows"))?;
        let length = block_len_at(header.uncompressed_length, header.block_size, block_id)?;
        let before = stream.position;
        match header.layout {
            Layout::Future => {
                let count = stream.read_uleb()?;
                let max_matches = crate::format_v3::max_selected_matches(
                    header.uncompressed_length,
                    effective_min,
                );
                if count > max_matches.saturating_sub(match_count) {
                    return Err(Error::corrupt_record(
                        "Future register count exceeds bounds",
                    ));
                }
                for _ in 0..count {
                    let source_offset = stream.read_uleb()?;
                    let distance = stream.read_uleb()?;
                    let total_len = stream.read_uleb()?;
                    if source_offset >= length {
                        return Err(Error::invalid_match(
                            "Future source_offset is outside block",
                        ));
                    }
                    let src = start
                        .checked_add(source_offset)
                        .ok_or_else(|| Error::invalid_match("Future source overflows"))?;
                    let dst = src
                        .checked_add(distance)
                        .ok_or_else(|| Error::invalid_match("Future destination overflows"))?;
                    validate_match(src, dst, total_len, header, effective_min)?;
                    let period_len = total_len.min(distance);

                    // Read and validate this register before reserving its
                    // storage. A huge, valid count followed by EOF must report
                    // truncation rather than exhausting memory before the
                    // first malformed/incomplete register is observed.
                    if free_registers.is_empty() {
                        let capacity = future_regs.len().checked_add(1).ok_or_else(|| {
                            Error::memory_limit("Future register capacity overflows")
                        })?;
                        prepare_future_capacity(&mut future_regs, capacity, || FutureRegister {
                            item: Match {
                                src: 0,
                                dst: 0,
                                len: 0,
                                origin_match_id: 0,
                            },
                            period_len: 0,
                            period: None,
                        })?;
                    }
                    let capacity = active_destinations.len().checked_add(1).ok_or_else(|| {
                        Error::memory_limit("Future destination capacity overflows")
                    })?;
                    prepare_future_capacity(&mut active_destinations, capacity, || {
                        DestinationEntry {
                            index: 0,
                            dst: 0,
                            end: 0,
                        }
                    })?;
                    let capacity = active_collectors.len().checked_add(1).ok_or_else(|| {
                        Error::memory_limit("Future collector capacity overflows")
                    })?;
                    prepare_future_capacity(&mut active_collectors, capacity, || 0)?;
                    let capacity = free_registers.len().checked_add(1).ok_or_else(|| {
                        Error::memory_limit("Future free-list capacity overflows")
                    })?;
                    prepare_future_capacity(&mut free_registers, capacity, || 0)?;
                    if let Some(metadata) = match_metadata.as_mut() {
                        let capacity = metadata.len().checked_add(1).ok_or_else(|| {
                            Error::memory_limit("Future metadata capacity overflows")
                        })?;
                        prepare_future_capacity(metadata, capacity, || Match {
                            src: 0,
                            dst: 0,
                            len: 0,
                            origin_match_id: 0,
                        })?;
                    }

                    let mut period = FuturePeriod::new(period_len, _resources, context)?;
                    let available = history.len();
                    let existing = available.saturating_sub(src).min(period_len);
                    for offset in 0..existing {
                        let byte = history.read_byte(src + offset)?;
                        period.push(byte)?;
                    }
                    let key = (block_id, source_offset, dst, total_len);
                    if let Some(previous) = previous_future
                        && (key.0 < previous.0
                            || (key.0 == previous.0 && key.1 < previous.1)
                            || (key.0 == previous.0 && key.1 == previous.1 && key.2 < previous.2)
                            || (key.0 == previous.0
                                && key.1 == previous.1
                                && key.2 == previous.2
                                && key.3 > previous.3))
                    {
                        return Err(Error::invalid_match("Future registers are not canonical"));
                    }
                    previous_future = Some(key);
                    let item = Match {
                        src,
                        dst,
                        len: total_len,
                        origin_match_id: 0,
                    };
                    match_count = match_count
                        .checked_add(1)
                        .ok_or_else(|| Error::invalid_match("match count overflows"))?;
                    covered = covered
                        .checked_add(total_len)
                        .ok_or_else(|| Error::invalid_match("match coverage overflows"))?;
                    if let Some(metadata) = match_metadata.as_mut() {
                        metadata.push(item)?;
                    }
                    let new_register = FutureRegister {
                        item,
                        period_len,
                        period: Some(period),
                    };
                    let needs_collector = new_register
                        .period
                        .as_ref()
                        .is_some_and(|period| period.len() < period_len);
                    let register_index = if free_registers.is_empty() {
                        let index = future_regs.len();
                        future_regs.push(new_register)?;
                        index
                    } else {
                        let index = free_registers.remove(free_registers.len() - 1);
                        future_regs[index] = new_register;
                        index
                    };
                    if needs_collector {
                        active_collectors.push(register_index)?;
                    }
                    let end = item
                        .dst
                        .checked_add(item.len)
                        .ok_or_else(|| Error::invalid_match("Future match endpoint overflows"))?;
                    for &existing in active_destinations.iter() {
                        if item.dst < existing.end && existing.dst < end {
                            return Err(Error::invalid_match(
                                "Future destination intervals overlap",
                            ));
                        }
                    }
                    destination_push(
                        &mut active_destinations,
                        DestinationEntry {
                            index: register_index,
                            dst: item.dst,
                            end,
                        },
                    )?;
                }
                let mut output = BlockOutput::new(staged, plaintext, history);
                decode_future_block(
                    stream,
                    start,
                    length,
                    &mut future_regs,
                    &mut active_destinations,
                    &mut active_collectors,
                    &mut free_registers,
                    &mut output,
                )?;
                let crc = read_stream_crc(stream)?;
                output.finish_block(&crc, length)?;
            }
            Layout::Io => {
                let mut output = BlockOutput::new(staged, plaintext, history);
                decode_io_block(
                    stream,
                    start,
                    length,
                    header,
                    effective_min,
                    &mut io_carry,
                    match_metadata.as_mut(),
                    &mut match_count,
                    &mut covered,
                    &mut output,
                )?;
                let crc = read_stream_crc(stream)?;
                output.finish_block(&crc, length)?;
            }
            Layout::Index => unreachable!(),
        }
        payload_size = payload_size
            .checked_add(stream.position - before)
            .ok_or_else(|| Error::corrupt_record("v3 payload size overflows"))?;
    }
    if io_carry.is_some() {
        return Err(Error::invalid_match("I/O match carry is unresolved at EOF"));
    }
    let tail_start = stream.position;
    let tail_bytes = stream.read_tail(header.checksum)?;
    let tail = parse_archive_tail(&tail_bytes, header.checksum)?;
    validate_tail_layout(&tail, header.layout, header.checksum, stream.position)?;
    if stream.input.read(&mut [0u8; 1]).map_err(Error::input_io)? != 0 {
        return Err(Error::corrupt_record("trailing bytes after v3 tail"));
    }
    if tail.body_end != tail_start {
        return Err(Error::corrupt_record(
            "v3 body_end does not match observed body",
        ));
    }
    verify_global_digests(header.checksum, &tail, plaintext, stream.encoded)?;
    history.finish()?;

    if header.layout == Layout::Future {
        if let Some(metadata) = match_metadata.as_mut() {
            validate_future_metadata(metadata, header)?;
        }
    } else {
        if let Some(metadata) = match_metadata.as_mut() {
            canonicalize_metadata(metadata)?;
        }
    }
    let literal_bytes = header
        .uncompressed_length
        .checked_sub(covered)
        .ok_or_else(|| Error::invalid_match("match coverage exceeds plaintext"))?;
    let stats = v3::compression_stats_from_header(
        header,
        tail.archive_length,
        payload_size,
        match_count,
        covered,
        literal_bytes,
    )?;
    let owned = match_metadata.map(v3::inspected_matches);
    Ok(V3Decoded::new(
        v3::archive_info_from_header(header, &stats),
        stats,
        owned,
    ))
}

fn read_stream_crc<R: Read>(stream: &mut StreamInput<'_, R>) -> Result<[u8; 4]> {
    let mut crc = [0u8; 4];
    stream.read_exact(&mut crc)?;
    Ok(crc)
}

#[allow(clippy::too_many_arguments)]
fn decode_future_block<R: Read>(
    stream: &mut StreamInput<'_, R>,
    start: u64,
    length: u64,
    regs: &mut BudgetedVec<FutureRegister>,
    active_destinations: &mut BudgetedVec<DestinationEntry>,
    active_collectors: &mut BudgetedVec<usize>,
    free_registers: &mut BudgetedVec<usize>,
    output: &mut BlockOutput<'_>,
) -> Result<()> {
    for offset in 0..length {
        let position = start + offset;
        while destination_peek(active_destinations).is_some_and(|entry| entry.end <= position) {
            let entry = destination_pop(active_destinations)
                .ok_or_else(|| Error::invalid_match("Future destination queue underflow"))?;
            regs[entry.index].period = None;
        }
        let destination = destination_peek(active_destinations)
            .filter(|entry| entry.dst <= position && position < entry.end);
        if let Some(entry) = destination {
            let index = entry.index;
            let (match_offset, complete) = {
                let register = &regs[index];
                (
                    position - register.item.dst,
                    register
                        .period
                        .as_ref()
                        .is_some_and(|period| period.len() == register.period_len),
                )
            };
            if !complete {
                return Err(Error::invalid_match(
                    "Future collector was not ready at destination",
                ));
            }
            let period_offset = match_offset % regs[index].period_len;
            let byte = regs[index]
                .period
                .as_mut()
                .ok_or_else(|| Error::invalid_match("Future collector was released too early"))?
                .byte(period_offset)?;
            output.emit(byte)?;
        } else {
            output.emit(stream.read_byte()?)?;
        }
        feed_future_collectors(
            regs,
            position,
            active_collectors,
            output.history_pending_last()?,
        )?;
        while destination_peek(active_destinations).is_some_and(|entry| entry.end <= position + 1) {
            let entry = destination_pop(active_destinations)
                .ok_or_else(|| Error::invalid_match("Future destination queue underflow"))?;
            regs[entry.index].period = None;
            free_registers.push(entry.index)?;
        }
    }
    Ok(())
}

fn feed_future_collectors(
    regs: &mut BudgetedVec<FutureRegister>,
    position: u64,
    active_collectors: &mut BudgetedVec<usize>,
    byte: u8,
) -> Result<()> {
    for active in active_collectors.iter().copied() {
        let index = active;
        let register = &regs[index];
        if register.period.is_none() {
            continue;
        }
        let source_end = register
            .item
            .src
            .checked_add(register.period_len)
            .ok_or_else(|| Error::invalid_match("Future collector endpoint overflows"))?;
        if register.item.src <= position
            && position < source_end
            && register
                .period
                .as_ref()
                .is_some_and(|period| period.len() < register.period_len)
        {
            let source_start = regs[index].item.src;
            let period = regs[index]
                .period
                .as_mut()
                .ok_or_else(|| Error::invalid_match("Future collector was released early"))?;
            let expected = source_start + period.len();
            if expected != position {
                return Err(Error::invalid_match(
                    "Future collector received bytes out of order",
                ));
            }
            period.push(byte)?;
        }
    }
    for index in (0..active_collectors.len()).rev() {
        let register_index = active_collectors[index];
        let complete = regs[register_index]
            .period
            .as_ref()
            .is_none_or(|period| period.len() >= regs[register_index].period_len);
        if complete {
            let _ = active_collectors.remove(index);
        }
    }
    Ok(())
}

// A tiny helper keeps the production order explicit and avoids exposing the
// collector implementation outside this reader module.
impl BlockOutput<'_> {
    fn history_pending_last(&mut self) -> Result<u8> {
        let position =
            self.history.len().checked_sub(1).ok_or_else(|| {
                Error::corrupt_record("plaintext history is empty after emission")
            })?;
        self.history.read_byte(position)
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_io_block<R: Read>(
    stream: &mut StreamInput<'_, R>,
    start: u64,
    length: u64,
    header: &ArchiveHeader,
    effective_min: u64,
    carry: &mut Option<(Match, u64)>,
    mut metadata: Option<&mut BudgetedVec<Match>>,
    match_count: &mut u64,
    covered: &mut u64,
    output: &mut BlockOutput<'_>,
) -> Result<()> {
    let end = start + length;
    let mut cursor = start;
    let mut previous_literal = false;
    if let Some((item, progress)) = carry.as_mut() {
        while cursor < end && *progress < item.len {
            let source = item.src + (*progress % (item.dst - item.src));
            let byte = output.history.read_byte(source)?;
            output.emit(byte)?;
            cursor += 1;
            *progress += 1;
        }
        if *progress == item.len {
            *carry = None;
        }
    }
    while cursor < end {
        let tag = stream.read_byte()?;
        match tag {
            crate::format_v3::IO_LITERAL_TAG => {
                if previous_literal {
                    return Err(Error::corrupt_record("adjacent I/O literal operations"));
                }
                let literal_len = stream.read_uleb()?;
                if literal_len == 0 || literal_len > end - cursor {
                    return Err(Error::corrupt_record("I/O literal does not fit block"));
                }
                for _ in 0..literal_len {
                    output.emit(stream.read_byte()?)?;
                }
                cursor += literal_len;
                previous_literal = true;
            }
            crate::format_v3::IO_MATCH_TAG => {
                let distance = stream.read_uleb()?;
                let total_len = stream.read_uleb()?;
                let dst = cursor;
                let src = dst
                    .checked_sub(distance)
                    .ok_or_else(|| Error::invalid_match("I/O match source underflows"))?;
                validate_match(src, dst, total_len, header, effective_min)?;
                let item = Match {
                    src,
                    dst,
                    len: total_len,
                    origin_match_id: 0,
                };
                *match_count = match_count
                    .checked_add(1)
                    .ok_or_else(|| Error::invalid_match("match count overflows"))?;
                *covered = covered
                    .checked_add(total_len)
                    .ok_or_else(|| Error::invalid_match("match coverage overflows"))?;
                if let Some(metadata) = metadata.as_deref_mut() {
                    metadata.push(item)?;
                }
                let remaining = end - cursor;
                let take = total_len.min(remaining);
                let mut progress = 0u64;
                while progress < take {
                    let source = src + (progress % distance);
                    let byte = output.history.read_byte(source)?;
                    output.emit(byte)?;
                    progress += 1;
                }
                cursor += take;
                previous_literal = false;
                if take < total_len {
                    *carry = Some((item, take));
                    break;
                }
            }
            _ => return Err(Error::corrupt_record("unknown I/O operation tag")),
        }
    }
    Ok(())
}

fn canonicalize_metadata(metadata: &mut BudgetedVec<Match>) -> Result<()> {
    metadata.sort_unstable_by(|a, b| {
        a.dst
            .cmp(&b.dst)
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| b.len.cmp(&a.len))
    });
    let mut previous_end = 0u64;
    for id in 0..metadata.len() {
        let item = metadata[id];
        if item.dst < previous_end {
            return Err(Error::invalid_match("destination intervals overlap"));
        }
        if id > 0
            && metadata[id - 1].src == item.src
            && metadata[id - 1].dst == item.dst
            && metadata[id - 1].len == item.len
        {
            return Err(Error::invalid_match("duplicate match origin"));
        }
        metadata[id].origin_match_id = id as u64;
        previous_end = item
            .dst
            .checked_add(item.len)
            .ok_or_else(|| Error::invalid_match("match endpoint overflows"))?;
    }
    Ok(())
}

fn validate_future_metadata(
    metadata: &mut BudgetedVec<Match>,
    header: &ArchiveHeader,
) -> Result<()> {
    canonicalize_metadata(metadata)?;
    for item in metadata.iter() {
        validate_match(
            item.src,
            item.dst,
            item.len,
            header,
            header.effective_min_match()?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CompressionConfig, Method};
    use std::io::Cursor;

    fn resources() -> ResourceConfig {
        let path = std::path::PathBuf::from("/tmp/opencode/v3-reader");
        let _ = std::fs::create_dir_all(&path);
        ResourceConfig {
            temp_dir: path,
            ..ResourceConfig::default()
        }
    }

    #[test]
    fn reader_roundtrips_literal_archives_from_writer() {
        for layout in [Layout::Index, Layout::Future, Layout::Io] {
            for checksum in [Checksum::Xxh3, Checksum::Blake3] {
                let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
                config.layout = layout;
                config.checksum = checksum;
                config.block_size = 1024;
                config.resources = resources();
                let input = b"reader-owned v3 literal test";
                let mut archive = Vec::new();
                crate::v3::writer::compress_with_candidates(
                    Cursor::new(input),
                    &mut archive,
                    &config,
                    std::iter::empty(),
                )
                .unwrap();
                let context = ResourceContext::with_resources(&config.resources).unwrap();
                let mut restored = Vec::new();
                let stats = decode(
                    Cursor::new(&archive),
                    &mut restored,
                    &config.resources,
                    &context,
                    true,
                )
                .unwrap();
                assert_eq!(restored, input);
                assert_eq!(stats.original_size, input.len() as u64);
                assert_eq!(stats.semantic_match_count, 0);
                assert_eq!(context.memory.current(), 0);
                assert_eq!(context.temp.current(), 0);
            }
        }
    }

    #[test]
    fn malformed_tail_and_trailing_bytes_are_rejected() {
        let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
        config.layout = Layout::Future;
        config.resources = resources();
        let mut archive = Vec::new();
        crate::v3::writer::compress_with_candidates(
            Cursor::new(b"abc"),
            &mut archive,
            &config,
            std::iter::empty(),
        )
        .unwrap();
        archive.push(0);
        let context = ResourceContext::with_resources(&config.resources).unwrap();
        assert_eq!(
            decode(
                Cursor::new(archive),
                Vec::new(),
                &config.resources,
                &context,
                true
            )
            .unwrap_err()
            .kind(),
            crate::error::ErrorKind::CorruptRecord
        );
    }

    fn materialize(len: usize, origins: &[(usize, usize, usize)]) -> Vec<u8> {
        let mut bytes: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(37) + 11) as u8).collect();
        let mut ordered = origins.to_vec();
        ordered.sort_unstable_by_key(|origin| origin.1);
        for &(src, dst, count) in &ordered {
            let distance = dst - src;
            for offset in 0..count {
                bytes[dst + offset] = bytes[src + (offset % distance)];
            }
        }
        bytes
    }

    #[test]
    fn reader_preserves_cross_block_match_and_match_ir() {
        let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
        config.layout = Layout::Io;
        config.block_size = 1024;
        config.resources = resources();
        let input = materialize(3300, &[(0, 1000, 2200)]);
        let candidate = crate::match_ir::MatchCandidate {
            src: 0,
            dst: 1000,
            len: 2200,
            insertion_ordinal: 0,
        };
        let mut archive = Vec::new();
        crate::v3::writer::compress_with_candidates(
            Cursor::new(&input),
            &mut archive,
            &config,
            [candidate],
        )
        .unwrap();
        let context = ResourceContext::with_resources(&config.resources).unwrap();
        let mut restored = Vec::new();
        let stats = decode(
            Cursor::new(&archive),
            &mut restored,
            &config.resources,
            &context,
            true,
        )
        .unwrap();
        assert_eq!(restored, input);
        assert_eq!(stats.semantic_match_count, 1);
        assert_eq!(stats.covered_bytes, 2200);

        let context = ResourceContext::with_resources(&config.resources).unwrap();
        let inspected =
            inspect_matches(Cursor::new(&archive), &config.resources, &context).unwrap();
        assert_eq!(inspected.as_slice().len(), 1);
        assert_eq!(inspected.as_slice()[0].src, 0);
        assert_eq!(inspected.as_slice()[0].dst, 1000);
        assert_eq!(inspected.as_slice()[0].len, 2200);
    }

    #[test]
    fn reader_reconstructs_index_and_future_origins() {
        let origins = [
            (0usize, 1000usize, 1100usize),
            (1000, 2100, 64),
            (100, 120, 64),
        ];
        let input = materialize(2164, &origins);
        let candidates = origins
            .into_iter()
            .enumerate()
            .map(
                |(ordinal, (src, dst, len))| crate::match_ir::MatchCandidate {
                    src: src as u64,
                    dst: dst as u64,
                    len: len as u64,
                    insertion_ordinal: ordinal as u64,
                },
            );
        for layout in [Layout::Index, Layout::Future] {
            let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
            config.layout = layout;
            config.block_size = 1024;
            config.resources = resources();
            let mut archive = Vec::new();
            crate::v3::writer::compress_with_candidates(
                Cursor::new(&input),
                &mut archive,
                &config,
                candidates.clone(),
            )
            .unwrap();
            let context = ResourceContext::with_resources(&config.resources).unwrap();
            let mut restored = Vec::new();
            let stats = decode(
                Cursor::new(&archive),
                &mut restored,
                &config.resources,
                &context,
                true,
            )
            .unwrap();
            assert_eq!(restored, input);
            assert_eq!(stats.semantic_match_count, 3);
            let context = ResourceContext::with_resources(&config.resources).unwrap();
            let inspected =
                inspect_matches(Cursor::new(&archive), &config.resources, &context).unwrap();
            assert_eq!(inspected.as_slice().len(), 3);
            assert_eq!(
                inspected.as_slice()[0],
                Match {
                    src: 100,
                    dst: 120,
                    len: 64,
                    origin_match_id: 0
                }
            );
            assert_eq!(
                inspected.as_slice()[1],
                Match {
                    src: 0,
                    dst: 1000,
                    len: 1100,
                    origin_match_id: 1
                }
            );
            assert_eq!(
                inspected.as_slice()[2],
                Match {
                    src: 1000,
                    dst: 2100,
                    len: 64,
                    origin_match_id: 2
                }
            );
        }
    }

    #[test]
    fn reader_validates_metadata_digest_after_plaintext_is_unchanged() {
        let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
        config.layout = Layout::Future;
        config.block_size = 1024;
        config.resources = resources();
        let mut archive = Vec::new();
        crate::v3::writer::compress_with_candidates(
            Cursor::new(b"abc"),
            &mut archive,
            &config,
            std::iter::empty(),
        )
        .unwrap();
        archive[40..48].copy_from_slice(&8192u64.to_le_bytes());
        let context = ResourceContext::with_resources(&config.resources).unwrap();
        let error = decode(
            Cursor::new(archive),
            Vec::new(),
            &config.resources,
            &context,
            true,
        )
        .unwrap_err();
        assert_eq!(error.kind(), crate::error::ErrorKind::ChecksumMismatch);
    }
}
