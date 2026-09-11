use std::io::{Read, Write};
use std::ops::Range;

use crate::checksum::{self, archive_digest, block_checksum, record_checksum};
use crate::config::{
    Checksum, CompressionConfig, Layout, MAX_BLOCK_SIZE, MAX_MATCH_LEN, MAX_UNCOMPRESSED,
    MIN_BLOCK_SIZE, MIN_MATCH_LEN, Method, m5_seed_size,
};
use crate::error::{Error, Result};
use crate::resource::{BudgetedVec, MemoryBudget};

pub const NG_V2_MAGIC: [u8; 8] = *b"SREPNG2\0";
pub const PROTOTYPE_V1_MAGIC: [u8; 8] = *b"SREPNG\0\x01";
pub const LEGACY_SIGNATURE: [u8; 8] = [0x17, 0x18, 0x35, 0x26, 0x53, 0x52, 0x45, 0x50];
pub const TRAILER_MAGIC: [u8; 8] = *b"SREPNGT2";
pub const HEADER_LEN: usize = 80;
pub const TRAILER_LEN: usize = 64;
pub const FRAME_LEN: usize = 12;
pub const METHOD_PARAMETERS_LEN: usize = 64;
pub const LAYOUT_METADATA_LEN: usize = 64;
pub const DATA_BLOCK_HEADER_LEN: usize = 48;
pub const BLOCK_DIRECTORY_HEADER_LEN: usize = 16;
pub const BLOCK_DIRECTORY_ENTRY_LEN: usize = 64;
pub const INDEX_SECTION_HEADER_LEN: usize = 32;
pub const INDEX_MATCH_ENTRY_LEN: usize = 24;
pub const INDEX_RANGE_ENTRY_LEN: usize = 24;
pub const LITERAL_RUN_HEADER_LEN: usize = 16;
pub const IO_LITERAL_HEADER_LEN: usize = 16;
pub const FUTURE_REGISTER_LEN: usize = 40;
pub const IO_MATCH_OP_LEN: usize = 40;
pub const SUMMARY_PREFIX_LEN: usize = 72;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexRanges;

pub const RECORD_METHOD_PARAMETERS: u8 = 0x01;
pub const RECORD_BLOCK_DIRECTORY: u8 = 0x02;
pub const RECORD_DATA_BLOCK: u8 = 0x03;
pub const RECORD_INDEX_SECTION: u8 = 0x04;
pub const RECORD_LAYOUT_METADATA: u8 = 0x05;
pub const RECORD_ARCHIVE_SUMMARY: u8 = 0x06;

pub const SCHEMA_V1: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveHeader {
    pub version: u8,
    pub checksum: Checksum,
    pub layout: Layout,
    pub method: Method,
    pub semantic_flags: u8,
    pub block_size: u64,
    pub min_match: u64,
    pub seed_size: u64,
    pub target_chunk: u64,
    pub max_distance: u64,
}

impl ArchiveHeader {
    pub fn from_config(config: &CompressionConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            version: 2,
            checksum: config.checksum,
            layout: config.layout,
            method: config.method,
            semantic_flags: config.semantic_flags(),
            block_size: config.block_size,
            min_match: config.min_match,
            seed_size: config.header_seed_size()?,
            target_chunk: config.header_target_chunk(),
            max_distance: config.header_max_distance(),
        })
    }

    pub fn effective_min_match(&self, params: &MethodParameters) -> Result<u64> {
        validate_header(self)?;
        validate_method_parameters(params, self)?;
        Ok(params.effective_min_match(self.min_match))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodParameters {
    pub method: Method,
    pub flags: u8,
    pub rep_distance: u64,
    pub rep_min_match: u64,
    pub rep_region_size: u64,
    pub cdc_window: u64,
    pub cdc_min_chunk: u64,
    pub order1_table_entries: u64,
    pub slice_count: u64,
}

impl MethodParameters {
    pub fn effective_min_match(&self, base_min_match: u64) -> u64 {
        if self.flags & 1 == 1 {
            base_min_match.min(self.rep_min_match)
        } else {
            base_min_match
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutMetadata {
    pub layout: Layout,
    pub uncompressed_len: u64,
    pub block_count: u64,
    pub data_record_count: u64,
    pub semantic_match_count: u64,
    pub covered_bytes: u64,
    pub literal_bytes: u64,
    pub encoded_operation_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockDirectoryEntry {
    pub block_id: u64,
    pub dst_start: u64,
    pub uncompressed_len: u64,
    pub data_record_offset: u64,
    pub data_record_total_len: u64,
    pub literal_run_count: u64,
    pub first_starting_match_index: u64,
    pub starting_match_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataBlockHeader {
    pub layout: Layout,
    pub block_id: u64,
    pub dst_start: u64,
    pub uncompressed_len: u64,
    pub literal_run_count: u64,
    pub operation_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trailer {
    pub summary_offset: u64,
    pub summary_total_len: u64,
    pub index_offset: u64,
    pub index_total_len: u64,
    pub total_archive_len: u64,
    pub body_end: u64,
}

pub fn encode_archive_header(header: &ArchiveHeader) -> Result<[u8; HEADER_LEN]> {
    validate_header(header)?;
    let mut bytes = [0u8; HEADER_LEN];
    bytes[..8].copy_from_slice(&NG_V2_MAGIC);
    bytes[8] = 2;
    bytes[9] = 0;
    bytes[10] = header.checksum.wire_id();
    bytes[11] = header.layout.wire_id();
    bytes[12] = header.method.wire_id();
    bytes[13] = header.semantic_flags;
    bytes[14..16].copy_from_slice(&0u16.to_le_bytes());
    bytes[16..24].copy_from_slice(&header.block_size.to_le_bytes());
    bytes[24..32].copy_from_slice(&header.min_match.to_le_bytes());
    bytes[32..40].copy_from_slice(&header.seed_size.to_le_bytes());
    bytes[40..48].copy_from_slice(&header.target_chunk.to_le_bytes());
    bytes[48..56].copy_from_slice(&header.max_distance.to_le_bytes());
    bytes[56..64].copy_from_slice(&0u64.to_le_bytes());
    bytes[64..72].copy_from_slice(&2u64.to_le_bytes());
    bytes[72..80].copy_from_slice(&80u64.to_le_bytes());
    Ok(bytes)
}

pub fn parse_archive_header(bytes: &[u8]) -> Result<ArchiveHeader> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::truncated("truncated NG v2 archive header"));
    }
    if bytes[..8] != NG_V2_MAGIC {
        return Err(Error::corrupt_header("invalid NG v2 magic"));
    }
    let version = bytes[8];
    if version != 2 {
        return Err(Error::unsupported_version(format!(
            "SREP-NG version {version} is not supported"
        )));
    }
    if bytes[9] != 0 {
        return Err(Error::corrupt_header("nonzero header flags"));
    }
    let checksum = Checksum::from_wire(bytes[10])?;
    let layout = Layout::from_wire(bytes[11])?;
    let method = Method::from_wire(bytes[12])?;
    let semantic_flags = bytes[13];
    if semantic_flags & !1 != 0 {
        return Err(Error::corrupt_header("nonzero reserved semantic flags"));
    }
    if bytes[14] != 0 || bytes[15] != 0 {
        return Err(Error::corrupt_header("nonzero header reserved bytes"));
    }
    let block_size = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let min_match = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    let seed_size = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
    let target_chunk = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
    let max_distance = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
    let checksum_seed = u64::from_le_bytes(bytes[56..64].try_into().unwrap());
    let header_record_count = u64::from_le_bytes(bytes[64..72].try_into().unwrap());
    let header_byte_length = u64::from_le_bytes(bytes[72..80].try_into().unwrap());
    if checksum_seed != 0 {
        return Err(Error::corrupt_header("checksum seed must be zero"));
    }
    if header_record_count != 2 {
        return Err(Error::corrupt_header("header record count must be 2"));
    }
    if header_byte_length != 80 {
        return Err(Error::corrupt_header("header byte length must be 80"));
    }
    let header = ArchiveHeader {
        version,
        checksum,
        layout,
        method,
        semantic_flags,
        block_size,
        min_match,
        seed_size,
        target_chunk,
        max_distance,
    };
    validate_header(&header)?;
    Ok(header)
}

pub fn validate_header(header: &ArchiveHeader) -> Result<()> {
    if header.version != 2 {
        return Err(Error::unsupported_version(format!(
            "SREP-NG version {} is not supported",
            header.version
        )));
    }
    if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&header.block_size) {
        return Err(Error::corrupt_header("block size outside wire limits"));
    }
    if !(MIN_MATCH_LEN..=MAX_MATCH_LEN).contains(&header.min_match) {
        return Err(Error::corrupt_header("minimum match outside wire limits"));
    }
    if header.max_distance > MAX_UNCOMPRESSED {
        return Err(Error::corrupt_header("maximum distance exceeds wire limit"));
    }
    let overlay = header.semantic_flags & 1 == 1;
    if overlay
        && matches!(
            header.method,
            Method::M0Rep | Method::M1RollingCdc | Method::M2Order1Cdc
        )
    {
        return Err(Error::corrupt_header(
            "REP overlay flag is invalid for this method",
        ));
    }
    match header.method {
        Method::M0Rep => {
            if header.seed_size != header.min_match || header.target_chunk != 0 {
                return Err(Error::corrupt_header("invalid m0 seed or target fields"));
            }
        }
        Method::M1RollingCdc => {
            if header.seed_size != 48
                || !(32..=MAX_MATCH_LEN).contains(&header.target_chunk)
                || header.target_chunk < header.min_match
            {
                return Err(Error::corrupt_header("invalid m1 seed or target fields"));
            }
        }
        Method::M2Order1Cdc => {
            if header.seed_size != 0
                || !(32..=MAX_MATCH_LEN).contains(&header.target_chunk)
                || header.target_chunk < header.min_match
            {
                return Err(Error::corrupt_header("invalid m2 seed or target fields"));
            }
        }
        Method::M3FixedDigest | Method::M4Reread => {
            if header.seed_size == 0 || header.seed_size > MAX_MATCH_LEN || header.target_chunk != 0
            {
                return Err(Error::corrupt_header("invalid m3/m4 seed or target fields"));
            }
        }
        Method::M5Exhaustive => {
            let expected = m5_seed_size(header.min_match)
                .map_err(|_| Error::corrupt_header("invalid m5 minimum match"))?;
            if header.seed_size != expected || header.target_chunk != 0 {
                return Err(Error::corrupt_header("invalid m5 seed or target fields"));
            }
        }
    }
    Ok(())
}

pub fn method_parameters_from_config(config: &CompressionConfig) -> Result<MethodParameters> {
    config.validate()?;
    let (cdc_window, cdc_min_chunk, order1, slices) = match config.method {
        Method::M1RollingCdc => (48, config.min_match, 0, 0),
        Method::M2Order1Cdc => (0, config.min_match, 256, 0),
        Method::M5Exhaustive => (0, 0, 0, 8),
        _ => (0, 0, 0, 0),
    };
    let (rep_distance, rep_min_match, rep_region_size) = match &config.rep_overlay {
        Some(overlay) => (
            overlay.distance,
            overlay.min_match,
            overlay.min_match.saturating_div(8).max(1),
        ),
        None => (0, 0, 0),
    };
    Ok(MethodParameters {
        method: config.method,
        flags: config.semantic_flags(),
        rep_distance,
        rep_min_match,
        rep_region_size,
        cdc_window,
        cdc_min_chunk,
        order1_table_entries: order1,
        slice_count: slices,
    })
}

pub fn encode_method_parameters(params: &MethodParameters) -> [u8; METHOD_PARAMETERS_LEN] {
    let mut bytes = [0u8; METHOD_PARAMETERS_LEN];
    bytes[..2].copy_from_slice(&SCHEMA_V1.to_le_bytes());
    bytes[2] = params.method.wire_id();
    bytes[3] = params.flags;
    bytes[8..16].copy_from_slice(&params.rep_distance.to_le_bytes());
    bytes[16..24].copy_from_slice(&params.rep_min_match.to_le_bytes());
    bytes[24..32].copy_from_slice(&params.rep_region_size.to_le_bytes());
    bytes[32..40].copy_from_slice(&params.cdc_window.to_le_bytes());
    bytes[40..48].copy_from_slice(&params.cdc_min_chunk.to_le_bytes());
    bytes[48..56].copy_from_slice(&params.order1_table_entries.to_le_bytes());
    bytes[56..64].copy_from_slice(&params.slice_count.to_le_bytes());
    bytes
}

pub fn parse_method_parameters(bytes: &[u8], header: &ArchiveHeader) -> Result<MethodParameters> {
    if bytes.len() != METHOD_PARAMETERS_LEN {
        return Err(Error::corrupt_header(
            "MethodParameters payload must be 64 bytes",
        ));
    }
    if u16::from_le_bytes(bytes[0..2].try_into().unwrap()) != SCHEMA_V1 {
        return Err(Error::corrupt_header("unsupported MethodParameters schema"));
    }
    let method = Method::from_wire(bytes[2])?;
    if method != header.method {
        return Err(Error::corrupt_header(
            "MethodParameters method does not match header",
        ));
    }
    let flags = bytes[3];
    if flags != header.semantic_flags {
        return Err(Error::corrupt_header(
            "MethodParameters flags do not match header",
        ));
    }
    if bytes[4..8].iter().any(|&b| b != 0) {
        return Err(Error::corrupt_header(
            "nonzero MethodParameters reserved bytes",
        ));
    }
    let params = MethodParameters {
        method,
        flags,
        rep_distance: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        rep_min_match: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        rep_region_size: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        cdc_window: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
        cdc_min_chunk: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
        order1_table_entries: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
        slice_count: u64::from_le_bytes(bytes[56..64].try_into().unwrap()),
    };
    validate_method_parameters(&params, header)?;
    Ok(params)
}

fn validate_method_parameters(params: &MethodParameters, header: &ArchiveHeader) -> Result<()> {
    let overlay = params.flags & 1 == 1;
    match header.method {
        Method::M0Rep => {
            if overlay
                || params.rep_distance != 0
                || params.rep_min_match != 0
                || params.rep_region_size != 0
                || params.cdc_window != 0
                || params.cdc_min_chunk != 0
                || params.order1_table_entries != 0
                || params.slice_count != 0
            {
                return Err(Error::corrupt_header("invalid m0 MethodParameters"));
            }
        }
        Method::M1RollingCdc => {
            if overlay
                || params.rep_distance != 0
                || params.rep_min_match != 0
                || params.rep_region_size != 0
                || params.cdc_window != 48
                || params.cdc_min_chunk != header.min_match
                || params.order1_table_entries != 0
                || params.slice_count != 0
            {
                return Err(Error::corrupt_header("invalid m1 MethodParameters"));
            }
        }
        Method::M2Order1Cdc => {
            if overlay
                || params.rep_distance != 0
                || params.rep_min_match != 0
                || params.rep_region_size != 0
                || params.cdc_window != 0
                || params.cdc_min_chunk != header.min_match
                || params.order1_table_entries != 256
                || params.slice_count != 0
            {
                return Err(Error::corrupt_header("invalid m2 MethodParameters"));
            }
        }
        Method::M3FixedDigest | Method::M4Reread | Method::M5Exhaustive => {
            let expected_slices = u64::from(matches!(header.method, Method::M5Exhaustive)) * 8;
            if params.cdc_window != 0
                || params.cdc_min_chunk != 0
                || params.order1_table_entries != 0
                || params.slice_count != expected_slices
            {
                return Err(Error::corrupt_header(
                    "invalid digest-method MethodParameters",
                ));
            }
            if overlay {
                if params.rep_distance == 0
                    || params.rep_distance > MAX_UNCOMPRESSED
                    || !(MIN_MATCH_LEN..=MAX_MATCH_LEN).contains(&params.rep_min_match)
                    || params.rep_region_size != params.rep_min_match.saturating_div(8).max(1)
                {
                    return Err(Error::corrupt_header("invalid overlay MethodParameters"));
                }
            } else if params.rep_distance != 0
                || params.rep_min_match != 0
                || params.rep_region_size != 0
            {
                return Err(Error::corrupt_header("overlay fields must be zero"));
            }
        }
    }
    Ok(())
}

pub fn encode_layout_metadata(meta: &LayoutMetadata) -> Result<[u8; LAYOUT_METADATA_LEN]> {
    validate_layout_metadata(meta)?;
    let mut bytes = [0u8; LAYOUT_METADATA_LEN];
    bytes[..2].copy_from_slice(&SCHEMA_V1.to_le_bytes());
    bytes[2] = meta.layout.wire_id();
    bytes[8..16].copy_from_slice(&meta.uncompressed_len.to_le_bytes());
    bytes[16..24].copy_from_slice(&meta.block_count.to_le_bytes());
    bytes[24..32].copy_from_slice(&meta.data_record_count.to_le_bytes());
    bytes[32..40].copy_from_slice(&meta.semantic_match_count.to_le_bytes());
    bytes[40..48].copy_from_slice(&meta.covered_bytes.to_le_bytes());
    bytes[48..56].copy_from_slice(&meta.literal_bytes.to_le_bytes());
    bytes[56..64].copy_from_slice(&meta.encoded_operation_count.to_le_bytes());
    Ok(bytes)
}

pub fn parse_layout_metadata(bytes: &[u8], header: &ArchiveHeader) -> Result<LayoutMetadata> {
    if bytes.len() != LAYOUT_METADATA_LEN {
        return Err(Error::corrupt_record(
            "LayoutMetadata payload must be 64 bytes",
        ));
    }
    if u16::from_le_bytes(bytes[0..2].try_into().unwrap()) != SCHEMA_V1 {
        return Err(Error::corrupt_record("unsupported LayoutMetadata schema"));
    }
    let layout = Layout::from_wire(bytes[2])
        .map_err(|_| Error::corrupt_record("LayoutMetadata layout is invalid"))?;
    if layout != header.layout {
        return Err(Error::corrupt_record(
            "LayoutMetadata layout does not match header",
        ));
    }
    if bytes[3] != 0 || bytes[4..8].iter().any(|&b| b != 0) {
        return Err(Error::corrupt_record(
            "nonzero LayoutMetadata flags or reserved bytes",
        ));
    }
    let meta = LayoutMetadata {
        layout,
        uncompressed_len: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        block_count: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        data_record_count: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        semantic_match_count: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
        covered_bytes: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
        literal_bytes: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
        encoded_operation_count: u64::from_le_bytes(bytes[56..64].try_into().unwrap()),
    };
    validate_layout_metadata(&meta)?;
    Ok(meta)
}

fn validate_layout_metadata(meta: &LayoutMetadata) -> Result<()> {
    if meta.data_record_count != meta.block_count {
        return Err(Error::corrupt_record(
            "data_record_count must equal block_count",
        ));
    }
    if meta.uncompressed_len > MAX_UNCOMPRESSED
        || meta.block_count > MAX_UNCOMPRESSED
        || meta.data_record_count > MAX_UNCOMPRESSED
        || meta.semantic_match_count > MAX_UNCOMPRESSED
        || meta.covered_bytes > MAX_UNCOMPRESSED
        || meta.literal_bytes > MAX_UNCOMPRESSED
        || meta.encoded_operation_count > MAX_UNCOMPRESSED
    {
        return Err(Error::corrupt_record("LayoutMetadata exceeds wire limits"));
    }
    let expected_literals = meta
        .uncompressed_len
        .checked_sub(meta.covered_bytes)
        .ok_or_else(|| Error::corrupt_record("covered_bytes exceeds uncompressed_len"))?;
    if expected_literals != meta.literal_bytes {
        return Err(Error::corrupt_record(
            "literal_bytes must equal uncompressed_len - covered_bytes",
        ));
    }
    if meta.layout == Layout::Index && meta.encoded_operation_count != 0 {
        return Err(Error::corrupt_record(
            "Index-LZ encoded_operation_count must be zero",
        ));
    }
    Ok(())
}

pub fn encode_block_directory(entries: &[BlockDirectoryEntry]) -> Result<Vec<u8>> {
    let block_count = u64::try_from(entries.len())
        .map_err(|_| Error::corrupt_index("block count overflows u64"))?;
    let payload_len = directory_payload_len(block_count)?;
    let mut bytes = vec![
        0u8;
        usize::try_from(payload_len).map_err(|_| {
            Error::memory_limit("BlockDirectory payload exceeds platform limits")
        })?
    ];
    bytes[..2].copy_from_slice(&SCHEMA_V1.to_le_bytes());
    bytes[2..4].copy_from_slice(&64u16.to_le_bytes());
    bytes[8..16].copy_from_slice(&block_count.to_le_bytes());
    for (index, entry) in entries.iter().enumerate() {
        let start = BLOCK_DIRECTORY_HEADER_LEN + index * BLOCK_DIRECTORY_ENTRY_LEN;
        let end = start
            .checked_add(BLOCK_DIRECTORY_ENTRY_LEN)
            .ok_or_else(|| Error::memory_limit("BlockDirectory entry range overflows"))?;
        encode_directory_entry(&mut bytes[start..end], entry);
    }
    Ok(bytes)
}

pub fn encode_block_directory_with_budget(
    entries: &[BlockDirectoryEntry],
    budget: &MemoryBudget,
) -> Result<BudgetedVec<u8>> {
    let block_count = u64::try_from(entries.len())
        .map_err(|_| Error::corrupt_index("block count overflows u64"))?;
    let payload_len = directory_payload_len(block_count)?;
    let mut bytes = BudgetedVec::with_capacity(
        usize::try_from(payload_len)
            .map_err(|_| Error::memory_limit("BlockDirectory payload exceeds platform limits"))?,
        budget,
    )?;
    bytes.resize(
        usize::try_from(payload_len)
            .map_err(|_| Error::memory_limit("BlockDirectory payload exceeds platform limits"))?,
        0,
    )?;
    let output = bytes.as_mut_slice();
    output[..2].copy_from_slice(&SCHEMA_V1.to_le_bytes());
    output[2..4].copy_from_slice(&64u16.to_le_bytes());
    output[8..16].copy_from_slice(&block_count.to_le_bytes());
    for (index, entry) in entries.iter().enumerate() {
        let start = BLOCK_DIRECTORY_HEADER_LEN + index * BLOCK_DIRECTORY_ENTRY_LEN;
        let end = start
            .checked_add(BLOCK_DIRECTORY_ENTRY_LEN)
            .ok_or_else(|| Error::memory_limit("BlockDirectory entry range overflows"))?;
        encode_directory_entry(&mut output[start..end], entry);
    }
    Ok(bytes)
}

fn encode_directory_entry(dst: &mut [u8], entry: &BlockDirectoryEntry) {
    dst[0..8].copy_from_slice(&entry.block_id.to_le_bytes());
    dst[8..16].copy_from_slice(&entry.dst_start.to_le_bytes());
    dst[16..24].copy_from_slice(&entry.uncompressed_len.to_le_bytes());
    dst[24..32].copy_from_slice(&entry.data_record_offset.to_le_bytes());
    dst[32..40].copy_from_slice(&entry.data_record_total_len.to_le_bytes());
    dst[40..48].copy_from_slice(&entry.literal_run_count.to_le_bytes());
    dst[48..56].copy_from_slice(&entry.first_starting_match_index.to_le_bytes());
    dst[56..64].copy_from_slice(&entry.starting_match_count.to_le_bytes());
}

pub fn parse_block_directory(
    bytes: &[u8],
    expected_blocks: u64,
) -> Result<Vec<BlockDirectoryEntry>> {
    let expected_len = directory_payload_len(expected_blocks)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected_len {
        return Err(Error::corrupt_index(
            "BlockDirectory payload length mismatch",
        ));
    }
    if u16::from_le_bytes(bytes[0..2].try_into().unwrap()) != SCHEMA_V1 {
        return Err(Error::corrupt_index("unsupported BlockDirectory schema"));
    }
    if u16::from_le_bytes(bytes[2..4].try_into().unwrap()) != 64 {
        return Err(Error::corrupt_index("BlockDirectory entry size must be 64"));
    }
    if bytes[4..8].iter().any(|&b| b != 0) {
        return Err(Error::corrupt_index(
            "nonzero BlockDirectory reserved bytes",
        ));
    }
    let block_count = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if block_count != expected_blocks {
        return Err(Error::corrupt_index(
            "BlockDirectory block_count does not match LayoutMetadata",
        ));
    }
    let entry_count = usize::try_from(expected_blocks)
        .map_err(|_| Error::memory_limit("BlockDirectory has too many entries"))?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(entry_count)
        .map_err(|_| Error::memory_limit("BlockDirectory entries allocation failed"))?;
    for index in 0..entry_count {
        let start = BLOCK_DIRECTORY_HEADER_LEN
            .checked_add(
                index
                    .checked_mul(BLOCK_DIRECTORY_ENTRY_LEN)
                    .ok_or_else(|| Error::corrupt_index("BlockDirectory entry offset overflows"))?,
            )
            .ok_or_else(|| Error::corrupt_index("BlockDirectory entry offset overflows"))?;
        let end = start
            .checked_add(BLOCK_DIRECTORY_ENTRY_LEN)
            .ok_or_else(|| Error::corrupt_index("BlockDirectory entry end overflows"))?;
        let slice = bytes
            .get(start..end)
            .ok_or_else(|| Error::corrupt_index("BlockDirectory entry is truncated"))?;
        entries.push(BlockDirectoryEntry {
            block_id: u64::from_le_bytes(slice[0..8].try_into().unwrap()),
            dst_start: u64::from_le_bytes(slice[8..16].try_into().unwrap()),
            uncompressed_len: u64::from_le_bytes(slice[16..24].try_into().unwrap()),
            data_record_offset: u64::from_le_bytes(slice[24..32].try_into().unwrap()),
            data_record_total_len: u64::from_le_bytes(slice[32..40].try_into().unwrap()),
            literal_run_count: u64::from_le_bytes(slice[40..48].try_into().unwrap()),
            first_starting_match_index: u64::from_le_bytes(slice[48..56].try_into().unwrap()),
            starting_match_count: u64::from_le_bytes(slice[56..64].try_into().unwrap()),
        });
    }
    Ok(entries)
}

pub fn parse_block_directory_with_budget(
    bytes: &[u8],
    expected_blocks: u64,
    budget: &crate::resource::MemoryBudget,
) -> Result<BudgetedVec<BlockDirectoryEntry>> {
    let expected_len = directory_payload_len(expected_blocks)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected_len {
        return Err(Error::corrupt_index(
            "BlockDirectory payload length mismatch",
        ));
    }
    if bytes.get(0..2) != Some(&SCHEMA_V1.to_le_bytes())
        || bytes.get(2..4) != Some(&64u16.to_le_bytes())
        || bytes
            .get(4..8)
            .is_none_or(|reserved| reserved.iter().any(|&byte| byte != 0))
        || bytes
            .get(8..16)
            .is_none_or(|count| u64::from_le_bytes(count.try_into().unwrap()) != expected_blocks)
    {
        return Err(Error::corrupt_index("BlockDirectory header is invalid"));
    }
    let entry_count = usize::try_from(expected_blocks)
        .map_err(|_| Error::memory_limit("BlockDirectory has too many entries"))?;
    let mut entries = BudgetedVec::with_capacity(entry_count, budget)?;
    for index in 0..entry_count {
        let start = BLOCK_DIRECTORY_HEADER_LEN
            .checked_add(
                index
                    .checked_mul(BLOCK_DIRECTORY_ENTRY_LEN)
                    .ok_or_else(|| Error::corrupt_index("BlockDirectory entry offset overflows"))?,
            )
            .ok_or_else(|| Error::corrupt_index("BlockDirectory entry offset overflows"))?;
        let end = start
            .checked_add(BLOCK_DIRECTORY_ENTRY_LEN)
            .ok_or_else(|| Error::corrupt_index("BlockDirectory entry end overflows"))?;
        let slice = bytes
            .get(start..end)
            .ok_or_else(|| Error::corrupt_index("BlockDirectory entry is truncated"))?;
        entries.push(BlockDirectoryEntry {
            block_id: u64::from_le_bytes(slice[0..8].try_into().unwrap()),
            dst_start: u64::from_le_bytes(slice[8..16].try_into().unwrap()),
            uncompressed_len: u64::from_le_bytes(slice[16..24].try_into().unwrap()),
            data_record_offset: u64::from_le_bytes(slice[24..32].try_into().unwrap()),
            data_record_total_len: u64::from_le_bytes(slice[32..40].try_into().unwrap()),
            literal_run_count: u64::from_le_bytes(slice[40..48].try_into().unwrap()),
            first_starting_match_index: u64::from_le_bytes(slice[48..56].try_into().unwrap()),
            starting_match_count: u64::from_le_bytes(slice[56..64].try_into().unwrap()),
        })?;
    }
    Ok(entries)
}

pub fn directory_payload_len(block_count: u64) -> Result<u64> {
    let entries = block_count
        .checked_mul(BLOCK_DIRECTORY_ENTRY_LEN as u64)
        .ok_or_else(|| Error::corrupt_index("BlockDirectory size overflows"))?;
    (BLOCK_DIRECTORY_HEADER_LEN as u64)
        .checked_add(entries)
        .ok_or_else(|| Error::corrupt_index("BlockDirectory size overflows"))
}

pub fn encode_index_section(block_count: u64, match_count: u64) -> Result<Vec<u8>> {
    if match_count != 0 {
        return Err(Error::invalid_match(
            "Stage 1 encoder does not emit IndexSection matches",
        ));
    }
    let payload_len = index_section_payload_len(match_count, block_count)?;
    let mut bytes = vec![
        0u8;
        usize::try_from(payload_len).map_err(|_| {
            Error::memory_limit("IndexSection payload exceeds platform limits")
        })?
    ];
    bytes[..2].copy_from_slice(&SCHEMA_V1.to_le_bytes());
    bytes[2..4].copy_from_slice(&24u16.to_le_bytes());
    bytes[4..6].copy_from_slice(&24u16.to_le_bytes());
    bytes[8..16].copy_from_slice(&match_count.to_le_bytes());
    bytes[16..24].copy_from_slice(&block_count.to_le_bytes());
    let mut cursor = INDEX_SECTION_HEADER_LEN;
    for block_id in 0..block_count {
        bytes[cursor..cursor + 8].copy_from_slice(&block_id.to_le_bytes());
        bytes[cursor + 8..cursor + 16].copy_from_slice(&0u64.to_le_bytes());
        bytes[cursor + 16..cursor + 24].copy_from_slice(&0u64.to_le_bytes());
        cursor += INDEX_RANGE_ENTRY_LEN;
    }
    Ok(bytes)
}

pub fn encode_index_section_with_budget(
    block_count: u64,
    match_count: u64,
    budget: &MemoryBudget,
) -> Result<BudgetedVec<u8>> {
    if match_count != 0 {
        return Err(Error::invalid_match(
            "Stage 1 encoder does not emit IndexSection matches",
        ));
    }
    let payload_len = index_section_payload_len(match_count, block_count)?;
    let size = usize::try_from(payload_len)
        .map_err(|_| Error::memory_limit("IndexSection payload exceeds platform limits"))?;
    let mut bytes = BudgetedVec::with_capacity(size, budget)?;
    bytes.resize(size, 0)?;
    let output = bytes.as_mut_slice();
    output[..2].copy_from_slice(&SCHEMA_V1.to_le_bytes());
    output[2..4].copy_from_slice(&24u16.to_le_bytes());
    output[4..6].copy_from_slice(&24u16.to_le_bytes());
    output[8..16].copy_from_slice(&match_count.to_le_bytes());
    output[16..24].copy_from_slice(&block_count.to_le_bytes());
    let mut cursor = INDEX_SECTION_HEADER_LEN;
    for block_id in 0..block_count {
        output[cursor..cursor + 8].copy_from_slice(&block_id.to_le_bytes());
        cursor += INDEX_RANGE_ENTRY_LEN;
    }
    Ok(bytes)
}

pub fn parse_index_section(bytes: &[u8], expected_blocks: u64) -> Result<(u64, IndexRanges)> {
    if bytes.len() < INDEX_SECTION_HEADER_LEN {
        return Err(Error::corrupt_index("truncated IndexSection header"));
    }
    if u16::from_le_bytes(bytes[0..2].try_into().unwrap()) != SCHEMA_V1 {
        return Err(Error::corrupt_index("unsupported IndexSection schema"));
    }
    if u16::from_le_bytes(bytes[2..4].try_into().unwrap()) != 24
        || u16::from_le_bytes(bytes[4..6].try_into().unwrap()) != 24
    {
        return Err(Error::corrupt_index("IndexSection entry size must be 24"));
    }
    if bytes[6] != 0 || bytes[7] != 0 || bytes[24..32].iter().any(|&b| b != 0) {
        return Err(Error::corrupt_index("nonzero IndexSection reserved bytes"));
    }
    let match_count = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let block_count = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    if block_count != expected_blocks {
        return Err(Error::corrupt_index(
            "IndexSection block_count does not match LayoutMetadata",
        ));
    }
    let expected_len = index_section_payload_len(match_count, block_count)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected_len {
        return Err(Error::corrupt_index("IndexSection payload length mismatch"));
    }
    if match_count != 0 {
        return Err(Error::unsupported_version(
            "Stage 1 decoder does not implement Index-LZ match operations",
        ));
    }
    let mut cursor = INDEX_SECTION_HEADER_LEN;
    for expected_id in 0..block_count {
        let end = cursor
            .checked_add(INDEX_RANGE_ENTRY_LEN)
            .ok_or_else(|| Error::corrupt_index("IndexSection range cursor overflows"))?;
        let entry = bytes
            .get(cursor..end)
            .ok_or_else(|| Error::corrupt_index("IndexSection range is truncated"))?;
        let block_id = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let first = u64::from_le_bytes(entry[8..16].try_into().unwrap());
        let count = u64::from_le_bytes(entry[16..24].try_into().unwrap());
        if block_id != expected_id || first != 0 || count != 0 {
            return Err(Error::corrupt_index(
                "IndexSection range entries are invalid for a literal-only archive",
            ));
        }
        cursor += INDEX_RANGE_ENTRY_LEN;
    }
    Ok((match_count, IndexRanges))
}

pub fn index_section_payload_len(match_count: u64, block_count: u64) -> Result<u64> {
    let matches = match_count
        .checked_mul(INDEX_MATCH_ENTRY_LEN as u64)
        .ok_or_else(|| Error::corrupt_index("IndexSection size overflows"))?;
    let ranges = block_count
        .checked_mul(INDEX_RANGE_ENTRY_LEN as u64)
        .ok_or_else(|| Error::corrupt_index("IndexSection size overflows"))?;
    (INDEX_SECTION_HEADER_LEN as u64)
        .checked_add(matches)
        .and_then(|n| n.checked_add(ranges))
        .ok_or_else(|| Error::corrupt_index("IndexSection size overflows"))
}

pub fn encode_literal_data_block(header: &DataBlockHeader, uncompressed: &[u8]) -> Result<Vec<u8>> {
    if uncompressed.len() as u64 != header.uncompressed_len {
        return Err(Error::corrupt_record(
            "DataBlock uncompressed length mismatch",
        ));
    }
    if header.layout != Layout::Io && header.operation_count != 0 {
        return Err(Error::corrupt_record(
            "literal-only Index/Future DataBlock operation_count must be zero",
        ));
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(
            usize::try_from(
                (DATA_BLOCK_HEADER_LEN as u64)
                    .checked_add(LITERAL_RUN_HEADER_LEN as u64)
                    .and_then(|n| n.checked_add(header.uncompressed_len))
                    .ok_or_else(|| Error::corrupt_record("DataBlock payload length overflows"))?,
            )
            .map_err(|_| Error::memory_limit("DataBlock payload exceeds platform limits"))?,
        )
        .map_err(|_| Error::memory_limit("DataBlock payload allocation failed"))?;
    payload.extend_from_slice(&SCHEMA_V1.to_le_bytes());
    payload.push(header.layout.wire_id());
    payload.push(0);
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&header.block_id.to_le_bytes());
    payload.extend_from_slice(&header.dst_start.to_le_bytes());
    payload.extend_from_slice(&header.uncompressed_len.to_le_bytes());
    payload.extend_from_slice(&header.literal_run_count.to_le_bytes());
    payload.extend_from_slice(&header.operation_count.to_le_bytes());
    match header.layout {
        Layout::Index | Layout::Future => {
            if header.uncompressed_len == 0 {
                return Err(Error::corrupt_record("empty DataBlock is invalid"));
            }
            if header.literal_run_count != 1 {
                return Err(Error::corrupt_record(
                    "literal-only DataBlock must have one maximal LiteralRun",
                ));
            }
            payload.extend_from_slice(&0u64.to_le_bytes());
            payload.extend_from_slice(&header.uncompressed_len.to_le_bytes());
            payload.extend_from_slice(uncompressed);
        }
        Layout::Io => {
            if header.uncompressed_len == 0 {
                return Err(Error::corrupt_record("empty DataBlock is invalid"));
            }
            if header.literal_run_count != 1 || header.operation_count != 1 {
                return Err(Error::corrupt_record(
                    "literal-only I/O DataBlock must have one tag-0 literal operation",
                ));
            }
            let encoded_len = 16u64
                .checked_add(header.uncompressed_len)
                .ok_or_else(|| Error::corrupt_record("I/O literal encoded_len overflows"))?;
            if encoded_len > u64::from(u32::MAX) {
                return Err(Error::corrupt_record("I/O literal encoded_len exceeds u32"));
            }
            payload.push(0);
            payload.push(0);
            payload.extend_from_slice(&0u16.to_le_bytes());
            payload.extend_from_slice(&(encoded_len as u32).to_le_bytes());
            payload.extend_from_slice(&header.uncompressed_len.to_le_bytes());
            payload.extend_from_slice(uncompressed);
        }
    }
    Ok(payload)
}

pub fn encode_literal_data_block_with_budget(
    header: &DataBlockHeader,
    uncompressed: &[u8],
    budget: &MemoryBudget,
) -> Result<BudgetedVec<u8>> {
    if uncompressed.len() as u64 != header.uncompressed_len {
        return Err(Error::corrupt_record(
            "DataBlock uncompressed length mismatch",
        ));
    }
    let prefix_len = if header.layout == Layout::Io {
        IO_LITERAL_HEADER_LEN
    } else {
        LITERAL_RUN_HEADER_LEN
    };
    let size = (DATA_BLOCK_HEADER_LEN as u64)
        .checked_add(prefix_len as u64)
        .and_then(|n| n.checked_add(header.uncompressed_len))
        .ok_or_else(|| Error::corrupt_record("DataBlock payload length overflows"))?;
    let mut payload = BudgetedVec::with_capacity(
        usize::try_from(size)
            .map_err(|_| Error::memory_limit("DataBlock payload exceeds platform limits"))?,
        budget,
    )?;
    payload.push_bytes(&SCHEMA_V1.to_le_bytes())?;
    payload.push(header.layout.wire_id())?;
    payload.push(0)?;
    payload.push_bytes(&0u32.to_le_bytes())?;
    for value in [
        header.block_id,
        header.dst_start,
        header.uncompressed_len,
        header.literal_run_count,
        header.operation_count,
    ] {
        payload.push_bytes(&value.to_le_bytes())?;
    }
    if header.uncompressed_len == 0
        || header.literal_run_count != 1
        || (header.layout == Layout::Io && header.operation_count != 1)
        || (header.layout != Layout::Io && header.operation_count != 0)
    {
        return Err(Error::corrupt_record(
            "invalid literal-only DataBlock counts",
        ));
    }
    if header.layout == Layout::Io {
        let encoded_len = 16u64
            .checked_add(header.uncompressed_len)
            .ok_or_else(|| Error::corrupt_record("I/O literal encoded_len overflows"))?;
        let encoded_len = u32::try_from(encoded_len)
            .map_err(|_| Error::corrupt_record("I/O literal encoded_len exceeds u32"))?;
        payload.push(0)?;
        payload.push_bytes(&[0, 0, 0])?;
        payload.push_bytes(&encoded_len.to_le_bytes())?;
        payload.push_bytes(&header.uncompressed_len.to_le_bytes())?;
        payload.push_bytes(uncompressed)?;
    } else {
        payload.push_bytes(&0u64.to_le_bytes())?;
        payload.push_bytes(&header.uncompressed_len.to_le_bytes())?;
        payload.push_bytes(uncompressed)?;
    }
    Ok(payload)
}

pub fn parse_literal_data_block(
    payload: &[u8],
    header: &ArchiveHeader,
    expected_id: u64,
    expected_start: u64,
    expected_len: u64,
) -> Result<(DataBlockHeader, Vec<u8>)> {
    let (header, range) =
        parse_literal_data_block_range(payload, header, expected_id, expected_start, expected_len)?;
    Ok((header, copy_checked(&payload[range])?))
}

pub fn parse_literal_data_block_range(
    payload: &[u8],
    header: &ArchiveHeader,
    expected_id: u64,
    expected_start: u64,
    expected_len: u64,
) -> Result<(DataBlockHeader, Range<usize>)> {
    if payload.len() < DATA_BLOCK_HEADER_LEN {
        return Err(Error::corrupt_record("truncated DataBlock header"));
    }
    if u16::from_le_bytes(payload[0..2].try_into().unwrap()) != SCHEMA_V1 {
        return Err(Error::corrupt_record("unsupported DataBlock schema"));
    }
    let layout = Layout::from_wire(payload[2])
        .map_err(|_| Error::corrupt_record("invalid DataBlock layout"))?;
    if layout != header.layout {
        return Err(Error::corrupt_record(
            "DataBlock layout does not match header",
        ));
    }
    if payload[3] != 0 || payload[4..8].iter().any(|&b| b != 0) {
        return Err(Error::corrupt_record(
            "nonzero DataBlock flags or reserved bytes",
        ));
    }
    let block_header = DataBlockHeader {
        layout,
        block_id: u64::from_le_bytes(payload[8..16].try_into().unwrap()),
        dst_start: u64::from_le_bytes(payload[16..24].try_into().unwrap()),
        uncompressed_len: u64::from_le_bytes(payload[24..32].try_into().unwrap()),
        literal_run_count: u64::from_le_bytes(payload[32..40].try_into().unwrap()),
        operation_count: u64::from_le_bytes(payload[40..48].try_into().unwrap()),
    };
    if block_header.block_id != expected_id
        || block_header.dst_start != expected_start
        || block_header.uncompressed_len != expected_len
        || expected_len == 0
        || expected_len > header.block_size
    {
        return Err(Error::corrupt_record(
            "DataBlock identity fields are invalid",
        ));
    }
    let rest = &payload[DATA_BLOCK_HEADER_LEN..];
    match layout {
        Layout::Index | Layout::Future => {
            if block_header.operation_count != 0 {
                if layout == Layout::Future {
                    return Err(Error::unsupported_version(
                        "Stage 1 decoder does not implement FutureRegister operations",
                    ));
                }
                return Err(Error::corrupt_record(
                    "Index-LZ DataBlock operation_count must be zero",
                ));
            }
            if block_header.literal_run_count != 1 {
                if block_header.literal_run_count == 0 {
                    return Err(Error::unsupported_version(
                        "Stage 1 decoder does not implement match-covered DataBlocks",
                    ));
                }
                return Err(Error::corrupt_record(
                    "literal-only DataBlock must contain exactly one maximal LiteralRun",
                ));
            }
            if rest.len() < LITERAL_RUN_HEADER_LEN {
                return Err(Error::corrupt_record("truncated LiteralRun"));
            }
            let dst_offset = u64::from_le_bytes(rest[0..8].try_into().unwrap());
            let len = u64::from_le_bytes(rest[8..16].try_into().unwrap());
            if dst_offset != 0 || len != expected_len || len == 0 {
                return Err(Error::corrupt_record("LiteralRun does not cover the block"));
            }
            let bytes = rest
                .get(LITERAL_RUN_HEADER_LEN..)
                .ok_or_else(|| Error::corrupt_record("truncated LiteralRun bytes"))?;
            if bytes.len() as u64 != len {
                return Err(Error::corrupt_record("LiteralRun byte length mismatch"));
            }
            let start = DATA_BLOCK_HEADER_LEN + LITERAL_RUN_HEADER_LEN;
            Ok((block_header, start..start + bytes.len()))
        }
        Layout::Io => {
            if block_header.operation_count == 0 {
                return Err(Error::corrupt_record("I/O-LZ DataBlock has no operations"));
            }
            if block_header.operation_count != 1 || block_header.literal_run_count != 1 {
                return Err(Error::unsupported_version(
                    "Stage 1 decoder does not implement I/O-LZ match fragments",
                ));
            }
            if rest.len() < IO_LITERAL_HEADER_LEN {
                return Err(Error::corrupt_record("truncated I/O literal operation"));
            }
            let tag = rest[0];
            if tag == 1 {
                return Err(Error::unsupported_version(
                    "Stage 1 decoder does not implement I/O-LZ match fragments",
                ));
            }
            if tag != 0 {
                return Err(Error::corrupt_record("unknown I/O operation tag"));
            }
            if rest[1] != 0 || rest[2] != 0 || rest[3] != 0 {
                return Err(Error::corrupt_record(
                    "nonzero I/O operation flags or reserved bytes",
                ));
            }
            let encoded_len = u32::from_le_bytes(rest[4..8].try_into().unwrap()) as u64;
            let literal_len = u64::from_le_bytes(rest[8..16].try_into().unwrap());
            let expected_encoded = 16u64
                .checked_add(literal_len)
                .ok_or_else(|| Error::corrupt_record("I/O encoded_len overflows"))?;
            if encoded_len != expected_encoded
                || literal_len != expected_len
                || literal_len == 0
                || rest.len() as u64 != encoded_len
            {
                return Err(Error::corrupt_record("I/O literal operation is malformed"));
            }
            let start = DATA_BLOCK_HEADER_LEN + IO_LITERAL_HEADER_LEN;
            let end = start
                .checked_add(
                    usize::try_from(literal_len)
                        .map_err(|_| Error::corrupt_record("I/O literal range overflows"))?,
                )
                .ok_or_else(|| Error::corrupt_record("I/O literal range overflows"))?;
            Ok((block_header, start..end))
        }
    }
}

fn copy_checked(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_| Error::memory_limit("decoded block exceeds platform limits"))?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}

#[allow(clippy::too_many_arguments)]
pub fn encode_archive_summary(
    checksum: Checksum,
    uncompressed_len: u64,
    block_count: u64,
    semantic_match_count: u64,
    covered_bytes: u64,
    literal_bytes: u64,
    total_data_record_bytes: u64,
    index_section_total_record_bytes: u64,
    total_record_count: u64,
    digest: &[u8],
) -> Result<Vec<u8>> {
    if digest.len() != checksum.width() {
        return Err(Error::corrupt_record("archive digest width mismatch"));
    }
    let mut bytes = vec![0u8; SUMMARY_PREFIX_LEN + checksum.width()];
    bytes[..2].copy_from_slice(&SCHEMA_V1.to_le_bytes());
    bytes[2] = checksum.wire_id();
    bytes[3] = checksum.digest_len();
    bytes[8..16].copy_from_slice(&uncompressed_len.to_le_bytes());
    bytes[16..24].copy_from_slice(&block_count.to_le_bytes());
    bytes[24..32].copy_from_slice(&semantic_match_count.to_le_bytes());
    bytes[32..40].copy_from_slice(&covered_bytes.to_le_bytes());
    bytes[40..48].copy_from_slice(&literal_bytes.to_le_bytes());
    bytes[48..56].copy_from_slice(&total_data_record_bytes.to_le_bytes());
    bytes[56..64].copy_from_slice(&index_section_total_record_bytes.to_le_bytes());
    bytes[64..72].copy_from_slice(&total_record_count.to_le_bytes());
    bytes[72..].copy_from_slice(digest);
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
pub fn encode_archive_summary_with_budget(
    checksum: Checksum,
    uncompressed_len: u64,
    block_count: u64,
    semantic_match_count: u64,
    covered_bytes: u64,
    literal_bytes: u64,
    total_data_record_bytes: u64,
    index_section_total_record_bytes: u64,
    total_record_count: u64,
    digest: &[u8],
    budget: &MemoryBudget,
) -> Result<BudgetedVec<u8>> {
    if digest.len() != checksum.width() {
        return Err(Error::corrupt_record("archive digest width mismatch"));
    }
    let mut bytes = BudgetedVec::with_capacity(SUMMARY_PREFIX_LEN + checksum.width(), budget)?;
    bytes.resize(SUMMARY_PREFIX_LEN + checksum.width(), 0)?;
    let output = bytes.as_mut_slice();
    output[..2].copy_from_slice(&SCHEMA_V1.to_le_bytes());
    output[2] = checksum.wire_id();
    output[3] = checksum.digest_len();
    for (offset, value) in [
        (8, uncompressed_len),
        (16, block_count),
        (24, semantic_match_count),
        (32, covered_bytes),
        (40, literal_bytes),
        (48, total_data_record_bytes),
        (56, index_section_total_record_bytes),
        (64, total_record_count),
    ] {
        output[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    output[72..].copy_from_slice(digest);
    Ok(bytes)
}

pub fn parse_archive_summary(
    bytes: &[u8],
    header: &ArchiveHeader,
    meta: &LayoutMetadata,
) -> Result<(u64, u64, u64, [u8; 32])> {
    let expected = SUMMARY_PREFIX_LEN + header.checksum.width();
    if bytes.len() != expected {
        return Err(Error::corrupt_record(
            "ArchiveSummary payload length mismatch",
        ));
    }
    if u16::from_le_bytes(bytes[0..2].try_into().unwrap()) != SCHEMA_V1 {
        return Err(Error::corrupt_record("unsupported ArchiveSummary schema"));
    }
    if bytes[2] != header.checksum.wire_id() {
        return Err(Error::corrupt_record(
            "ArchiveSummary checksum id does not match header",
        ));
    }
    if bytes[3] != header.checksum.digest_len() {
        return Err(Error::corrupt_record("ArchiveSummary digest_len mismatch"));
    }
    if bytes[4..8].iter().any(|&b| b != 0) {
        return Err(Error::corrupt_record(
            "nonzero ArchiveSummary reserved bytes",
        ));
    }
    let uncompressed_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let block_count = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let semantic_match_count = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    let covered_bytes = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
    let literal_bytes = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
    let total_data_record_bytes = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
    let index_section_total_record_bytes = u64::from_le_bytes(bytes[56..64].try_into().unwrap());
    let total_record_count = u64::from_le_bytes(bytes[64..72].try_into().unwrap());
    if uncompressed_len != meta.uncompressed_len
        || block_count != meta.block_count
        || semantic_match_count != meta.semantic_match_count
        || covered_bytes != meta.covered_bytes
        || literal_bytes != meta.literal_bytes
    {
        return Err(Error::corrupt_record(
            "ArchiveSummary counters do not match LayoutMetadata",
        ));
    }
    let index_flag = u64::from(header.layout.is_index());
    let expected_records = meta
        .block_count
        .checked_add(3)
        .and_then(|n| n.checked_add(index_flag.checked_mul(2)?))
        .ok_or_else(|| Error::corrupt_record("total_record_count overflows"))?;
    if total_record_count != expected_records {
        return Err(Error::corrupt_record(
            "ArchiveSummary total_record_count mismatch",
        ));
    }
    if !header.layout.is_index() && index_section_total_record_bytes != 0 {
        return Err(Error::corrupt_index(
            "non-Index layout must have zero IndexSection total bytes",
        ));
    }
    let mut digest = [0u8; 32];
    digest[..header.checksum.width()].copy_from_slice(&bytes[72..]);
    Ok((
        total_data_record_bytes,
        index_section_total_record_bytes,
        total_record_count,
        digest,
    ))
}

pub fn encode_trailer(trailer: &Trailer) -> [u8; TRAILER_LEN] {
    let mut bytes = [0u8; TRAILER_LEN];
    bytes[..8].copy_from_slice(&TRAILER_MAGIC);
    bytes[8] = 2;
    bytes[12..20].copy_from_slice(&trailer.summary_offset.to_le_bytes());
    bytes[20..28].copy_from_slice(&trailer.summary_total_len.to_le_bytes());
    bytes[28..36].copy_from_slice(&trailer.index_offset.to_le_bytes());
    bytes[36..44].copy_from_slice(&trailer.index_total_len.to_le_bytes());
    bytes[44..52].copy_from_slice(&trailer.total_archive_len.to_le_bytes());
    bytes[52..60].copy_from_slice(&trailer.body_end.to_le_bytes());
    bytes
}

pub fn parse_trailer(bytes: &[u8], layout: Layout, file_len: u64) -> Result<Trailer> {
    if bytes.len() != TRAILER_LEN {
        return Err(Error::truncated("truncated trailer"));
    }
    if bytes[..8] != TRAILER_MAGIC {
        return Err(Error::corrupt_record("invalid trailer magic"));
    }
    if bytes[8] != 2 {
        return Err(Error::corrupt_record("invalid trailer version"));
    }
    if bytes[9] != 0 || bytes[10] != 0 || bytes[11] != 0 || bytes[60..64].iter().any(|&b| b != 0) {
        return Err(Error::corrupt_record(
            "nonzero trailer flags or reserved bytes",
        ));
    }
    let trailer = Trailer {
        summary_offset: u64::from_le_bytes(bytes[12..20].try_into().unwrap()),
        summary_total_len: u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
        index_offset: u64::from_le_bytes(bytes[28..36].try_into().unwrap()),
        index_total_len: u64::from_le_bytes(bytes[36..44].try_into().unwrap()),
        total_archive_len: u64::from_le_bytes(bytes[44..52].try_into().unwrap()),
        body_end: u64::from_le_bytes(bytes[52..60].try_into().unwrap()),
    };
    if trailer.total_archive_len != file_len {
        return Err(Error::corrupt_record("trailer total length mismatch"));
    }
    let index_present = trailer.index_offset != 0 || trailer.index_total_len != 0;
    if layout.is_index() {
        if trailer.index_offset == 0 || trailer.index_total_len == 0 {
            return Err(Error::corrupt_index(
                "Index-LZ trailer IndexSection fields must be nonzero",
            ));
        }
    } else if index_present {
        return Err(Error::corrupt_index(
            "non-Index layout trailer IndexSection fields must be zero",
        ));
    }
    Ok(trailer)
}

pub fn record_frame(record_type: u8, payload_len: u64) -> Result<[u8; FRAME_LEN]> {
    if payload_len > MAX_UNCOMPRESSED {
        return Err(Error::corrupt_record("record payload exceeds wire limit"));
    }
    let mut frame = [0u8; FRAME_LEN];
    frame[0] = record_type;
    frame[4..12].copy_from_slice(&payload_len.to_le_bytes());
    Ok(frame)
}

pub fn record_total_len(payload_len: u64, checksum: Checksum) -> Result<u64> {
    (FRAME_LEN as u64)
        .checked_add(payload_len)
        .and_then(|n| n.checked_add(checksum.width() as u64))
        .ok_or_else(|| Error::corrupt_record("record total length overflows"))
}

pub fn encode_ordinary_record(
    record_type: u8,
    payload: &[u8],
    checksum: Checksum,
) -> Result<Vec<u8>> {
    let payload_len = u64::try_from(payload.len())
        .map_err(|_| Error::corrupt_record("payload length overflows u64"))?;
    let frame = record_frame(record_type, payload_len)?;
    let digest = record_checksum(checksum, &frame, payload);
    let mut out = Vec::with_capacity(FRAME_LEN + payload.len() + checksum.width());
    out.extend_from_slice(&frame);
    out.extend_from_slice(payload);
    out.extend_from_slice(&digest);
    Ok(out)
}

pub fn encode_ordinary_record_with_budget(
    record_type: u8,
    payload: &[u8],
    checksum: Checksum,
    budget: &MemoryBudget,
) -> Result<BudgetedVec<u8>> {
    let payload_len = u64::try_from(payload.len())
        .map_err(|_| Error::corrupt_record("payload length overflows u64"))?;
    let frame = record_frame(record_type, payload_len)?;
    let digest = record_checksum(checksum, &frame, payload);
    let mut out = BudgetedVec::with_capacity(FRAME_LEN + payload.len() + checksum.width(), budget)?;
    out.push_bytes(&frame)?;
    out.push_bytes(payload)?;
    out.push_bytes(&digest)?;
    Ok(out)
}

pub fn encode_data_block_record(
    payload: &[u8],
    checksum: Checksum,
    block_id: u64,
    dst_start: u64,
    uncompressed: &[u8],
) -> Result<Vec<u8>> {
    let payload_len = u64::try_from(payload.len())
        .map_err(|_| Error::corrupt_record("payload length overflows u64"))?;
    let frame = record_frame(RECORD_DATA_BLOCK, payload_len)?;
    let digest = block_checksum(checksum, &frame, payload, block_id, dst_start, uncompressed)?;
    let mut out = Vec::with_capacity(FRAME_LEN + payload.len() + checksum.width());
    out.extend_from_slice(&frame);
    out.extend_from_slice(payload);
    out.extend_from_slice(&digest);
    Ok(out)
}

pub fn encode_data_block_record_with_budget(
    payload: &[u8],
    checksum: Checksum,
    block_id: u64,
    dst_start: u64,
    uncompressed: &[u8],
    budget: &MemoryBudget,
) -> Result<BudgetedVec<u8>> {
    let payload_len = u64::try_from(payload.len())
        .map_err(|_| Error::corrupt_record("payload length overflows u64"))?;
    let frame = record_frame(RECORD_DATA_BLOCK, payload_len)?;
    let digest = block_checksum(checksum, &frame, payload, block_id, dst_start, uncompressed)?;
    let mut out = BudgetedVec::with_capacity(FRAME_LEN + payload.len() + checksum.width(), budget)?;
    out.push_bytes(&frame)?;
    out.push_bytes(payload)?;
    out.push_bytes(&digest)?;
    Ok(out)
}

pub fn read_record_frame<R: Read>(reader: &mut R) -> Result<[u8; FRAME_LEN]> {
    let mut frame = [0u8; FRAME_LEN];
    reader
        .read_exact(&mut frame)
        .map_err(|error| Error::map_eof(error, "truncated record frame"))?;
    if frame[1] != 0 {
        return Err(Error::corrupt_record("nonzero record flags"));
    }
    if frame[2] != 0 || frame[3] != 0 {
        return Err(Error::corrupt_record("nonzero record reserved bytes"));
    }
    Ok(frame)
}

pub fn parse_frame(frame: &[u8; FRAME_LEN]) -> Result<(u8, u64)> {
    if frame[1] != 0 || frame[2] != 0 || frame[3] != 0 {
        return Err(Error::corrupt_record(
            "nonzero record flags or reserved bytes",
        ));
    }
    let payload_len = u64::from_le_bytes(frame[4..12].try_into().unwrap());
    if payload_len > MAX_UNCOMPRESSED {
        return Err(Error::corrupt_record("record payload exceeds wire limit"));
    }
    Ok((frame[0], payload_len))
}

pub fn read_payload<R: Read>(
    reader: &mut R,
    payload_len: u64,
    alloc_limit: u64,
) -> Result<Vec<u8>> {
    if payload_len > alloc_limit {
        return Err(Error::memory_limit("record payload exceeds memory budget"));
    }
    let mut payload = Vec::new();
    let payload_len_usize = usize::try_from(payload_len)
        .map_err(|_| Error::memory_limit("record payload exceeds platform limits"))?;
    payload
        .try_reserve_exact(payload_len_usize)
        .map_err(|_| Error::memory_limit("record payload allocation failed"))?;
    let mut remaining = payload_len;
    let mut buf = [0u8; 8192];
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buf.len() as u64)).unwrap();
        let count = reader.read(&mut buf[..want]).map_err(Error::input_io)?;
        if count == 0 {
            return Err(Error::truncated("truncated record payload"));
        }
        payload.extend_from_slice(&buf[..count]);
        remaining -= count as u64;
    }
    Ok(payload)
}

pub fn read_digest<R: Read>(reader: &mut R, checksum: Checksum) -> Result<Vec<u8>> {
    let mut digest = vec![0u8; checksum.width()];
    reader
        .read_exact(&mut digest)
        .map_err(|error| Error::map_eof(error, "truncated record checksum"))?;
    Ok(digest)
}

pub fn write_all<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    writer.write_all(bytes).map_err(Error::output_io)
}

pub fn semantic_digest(
    checksum: Checksum,
    header_bytes: &[u8],
    method_payload: &[u8],
    layout_payload: &[u8],
    uncompressed: &[u8],
) -> Vec<u8> {
    archive_digest(
        checksum,
        header_bytes,
        method_payload,
        layout_payload,
        uncompressed,
    )
}

pub fn verify_ordinary_checksum(
    checksum: Checksum,
    frame: &[u8; FRAME_LEN],
    payload: &[u8],
    actual: &[u8],
) -> Result<()> {
    let expected = record_checksum(checksum, frame, payload);
    checksum::verify_bytes(
        checksum,
        &expected,
        actual,
        "ordinary record checksum mismatch",
    )
}

pub fn verify_block_checksum(
    checksum: Checksum,
    frame: &[u8; FRAME_LEN],
    payload: &[u8],
    block_id: u64,
    dst_start: u64,
    uncompressed: &[u8],
    actual: &[u8],
) -> Result<()> {
    let expected = block_checksum(checksum, frame, payload, block_id, dst_start, uncompressed)?;
    checksum::verify_bytes(
        checksum,
        &expected,
        actual,
        "DataBlock representation-plus-semantics checksum mismatch",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_exactly_80_bytes_with_required_constants() {
        let config = CompressionConfig::default();
        let header = ArchiveHeader::from_config(&config).unwrap();
        let bytes = encode_archive_header(&header).unwrap();
        assert_eq!(bytes.len(), 80);
        assert_eq!(&bytes[..8], b"SREPNG2\0");
        assert_eq!(bytes[8], 2);
        assert_eq!(bytes[9], 0);
        assert_eq!(bytes[10], 1);
        assert_eq!(bytes[11], 1);
        assert_eq!(bytes[12], 3);
        assert_eq!(bytes[13], 0);
        assert_eq!(&bytes[16..24], &config.block_size.to_le_bytes());
        assert_eq!(&bytes[64..72], &2u64.to_le_bytes());
        assert_eq!(&bytes[72..80], &80u64.to_le_bytes());
        assert_eq!(parse_archive_header(&bytes).unwrap(), header);
    }

    #[test]
    fn schema_arithmetic_matches_spec() {
        assert_eq!(directory_payload_len(0).unwrap(), 16);
        assert_eq!(directory_payload_len(2).unwrap(), 16 + 128);
        assert_eq!(index_section_payload_len(0, 0).unwrap(), 32);
        assert_eq!(index_section_payload_len(0, 3).unwrap(), 32 + 72);
        assert_eq!(record_total_len(64, Checksum::Xxh3).unwrap(), 12 + 64 + 16);
        assert_eq!(
            record_total_len(64, Checksum::Blake3).unwrap(),
            12 + 64 + 32
        );
    }

    #[test]
    fn trailer_is_exactly_64_bytes() {
        let encoded = encode_trailer(&Trailer {
            summary_offset: 100,
            summary_total_len: 116,
            index_offset: 0,
            index_total_len: 0,
            total_archive_len: 180,
            body_end: 80,
        });
        assert_eq!(encoded.len(), 64);
        assert_eq!(&encoded[..8], b"SREPNGT2");
        assert_eq!(encoded[8], 2);
    }

    #[test]
    fn layout_metadata_rejects_all_wire_limited_counts() {
        let config = CompressionConfig::default();
        let header = ArchiveHeader::from_config(&config).unwrap();
        let cases = [
            (32usize, MAX_UNCOMPRESSED + 1),
            (56usize, MAX_UNCOMPRESSED + 1),
            (16usize, MAX_UNCOMPRESSED + 1),
            (24usize, MAX_UNCOMPRESSED + 1),
        ];
        for (offset, value) in cases {
            let mut bytes = [0u8; LAYOUT_METADATA_LEN];
            bytes[..2].copy_from_slice(&SCHEMA_V1.to_le_bytes());
            bytes[2] = header.layout.wire_id();
            bytes[8..16].copy_from_slice(&1u64.to_le_bytes());
            bytes[16..24].copy_from_slice(&1u64.to_le_bytes());
            bytes[24..32].copy_from_slice(&1u64.to_le_bytes());
            bytes[32..40].copy_from_slice(&0u64.to_le_bytes());
            bytes[40..48].copy_from_slice(&0u64.to_le_bytes());
            bytes[48..56].copy_from_slice(&1u64.to_le_bytes());
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            assert!(
                parse_layout_metadata(&bytes, &header).is_err(),
                "offset {offset}"
            );
        }
    }
}
