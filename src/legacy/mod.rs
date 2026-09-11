//! Strict, read-only decoder for embedded SREP legacy versions 1 through 4.
//!
//! The reader is deliberately independent from the NG v2 writer.  The archive,
//! reconstructed output, and Future-LZ metadata are seekable private stores;
//! untrusted archive-sized data is never accumulated in process memory.

mod checksum;
mod future;
pub(crate) mod header;
mod io_lz;
mod storage;

use std::io::{Read, Seek, SeekFrom, Write};

use crate::config::ResourceConfig;
use crate::error::Result;
use crate::resource::ResourceContext;

pub use checksum::{ChecksumKind, LegacyHasher};
pub use header::{Header, Layout, parse_header};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyResult {
    pub version: u8,
    pub layout: Layout,
    pub checksum: ChecksumKind,
    pub base_len: u32,
    pub archive_size: u64,
    pub header_size: u64,
    pub original_size: u64,
    pub block_count: u64,
    pub match_count: u64,
    pub covered_bytes: u64,
    pub literal_bytes: u64,
}

pub(crate) struct Decoded {
    pub output: storage::OutputStore,
    pub original_size: u64,
    pub block_count: u64,
    pub match_count: u64,
    pub covered_bytes: u64,
    pub literal_bytes: u64,
}

pub(super) enum LegacyBoundary {
    Block,
    End,
}

/// Classify the bytes remaining where a v1-v3 block may begin.  The eight-byte
/// zero marker is only a terminator when it is the complete remainder.
pub(super) fn legacy_block_boundary(
    file: &mut std::fs::File,
    position: u64,
    file_len: u64,
) -> Result<LegacyBoundary> {
    let remaining = file_len.checked_sub(position).ok_or_else(|| {
        crate::error::Error::corrupt_record("legacy block offset is outside archive")
    })?;
    if remaining == 0 {
        return Ok(LegacyBoundary::End);
    }
    let sample_len = remaining.min(12) as usize;
    file.seek(SeekFrom::Start(position))
        .map_err(crate::error::Error::temp_storage)?;
    let mut sample = [0u8; 12];
    file.read_exact(&mut sample[..sample_len])
        .map_err(|error| crate::error::Error::map_eof(error, "truncated legacy block boundary"))?;
    if remaining >= 8 && sample[..8] == [0; 8] {
        if remaining == 8 {
            return Ok(LegacyBoundary::End);
        }
        return Err(crate::error::Error::corrupt_record(
            "legacy terminator has trailing bytes",
        ));
    }
    if remaining < 12 {
        return Err(crate::error::Error::truncated(
            "truncated legacy block header",
        ));
    }
    Ok(LegacyBoundary::Block)
}

/// Decode a legacy archive after the dispatcher has consumed and validated its
/// fixed header plus exact seed.  `header` is written to the archive spool once;
/// bytes from `input` are then copied once and only once.
pub fn decode<R: Read, W: Write>(
    header: Vec<u8>,
    input: &mut R,
    output: &mut W,
    resources: &ResourceConfig,
    context: &ResourceContext,
    write_output: bool,
) -> Result<LegacyResult> {
    let mut archive = storage::spool_archive(header, input, resources, context)?;
    let parsed = header::parse_seekable(&mut archive.file)?;
    let decoded = match parsed.layout {
        Layout::IoRounded | Layout::Io => {
            io_lz::decode(&mut archive.file, &parsed, resources, context)?
        }
        Layout::Future | Layout::Index => {
            future::decode(&mut archive.file, &parsed, resources, context)?
        }
    };
    let result = LegacyResult {
        version: parsed.version,
        layout: parsed.layout,
        checksum: parsed.checksum,
        base_len: parsed.base_len,
        archive_size: archive.len,
        header_size: parsed.header_end,
        original_size: decoded.original_size,
        block_count: decoded.block_count,
        match_count: decoded.match_count,
        covered_bytes: decoded.covered_bytes,
        literal_bytes: decoded.literal_bytes,
    };
    if write_output {
        decoded.output.copy_to(output)?;
    }
    Ok(result)
}

pub fn layout_name(layout: Layout) -> &'static str {
    layout.name()
}

pub fn checksum_name(kind: ChecksumKind) -> &'static str {
    kind.name()
}
