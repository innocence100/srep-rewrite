//! SREP-NG v3 wire primitives.
//!
//! Normative layout is `docs/FORMAT-V3.md` (SHA-256
//! `2f26772be0c1e12325021c0ba99961a882139b6bc9ad60562374055963c7606c`).
//! This module is the shared header/tail/ULEB/CRC32C/index/digest contract.
//! It does not read or write complete archives.

use std::io::{Read, Write};

use crate::checksum::Digest;
use crate::config::{
    Checksum, CompressionConfig, Layout, MAX_BLOCK_SIZE, MAX_MATCH_LEN, MAX_UNCOMPRESSED,
    MIN_BLOCK_SIZE, MIN_MATCH_LEN, Method, RepConfig, ResourceConfig, m5_seed_size,
};
use crate::error::{Error, Result};
use crate::match_ir::Match;

pub const NG_V3_MAGIC: [u8; 8] = *b"SREPNG3\0";
pub const TAIL_MAGIC: [u8; 8] = *b"SREPNGT3";
pub const VERSION: u8 = 3;
pub const HEADER_LEN: usize = 80;
pub const TAIL_PREFIX_LEN: usize = 44;
pub const TAIL_LEN_XXH3: usize = 76;
pub const TAIL_LEN_BLAKE3: usize = 108;
pub const MAX_ULEB128_LEN: usize = 9;
pub const MAX_WIRE: u64 = MAX_UNCOMPRESSED;
pub const POSITIVE_GAIN_MIN_LEN: u64 = 26;
pub const IO_LITERAL_TAG: u8 = 0x00;
pub const IO_MATCH_TAG: u8 = 0x01;
pub const PLAINTEXT_DOMAIN: &[u8] = b"SREPNG3-PLAINTEXT\0";
pub const ENCODED_DOMAIN: &[u8] = b"SREPNG3-ENCODED\0";

const CRC32C_TABLE: [u32; 256] = [
    0x00000000, 0xF26B8303, 0xE13B70F7, 0x1350F3F4, 0xC79A971F, 0x35F1141C, 0x26A1E7E8, 0xD4CA64EB,
    0x8AD958CF, 0x78B2DBCC, 0x6BE22838, 0x9989AB3B, 0x4D43CFD0, 0xBF284CD3, 0xAC78BF27, 0x5E133C24,
    0x105EC76F, 0xE235446C, 0xF165B798, 0x030E349B, 0xD7C45070, 0x25AFD373, 0x36FF2087, 0xC494A384,
    0x9A879FA0, 0x68EC1CA3, 0x7BBCEF57, 0x89D76C54, 0x5D1D08BF, 0xAF768BBC, 0xBC267848, 0x4E4DFB4B,
    0x20BD8EDE, 0xD2D60DDD, 0xC186FE29, 0x33ED7D2A, 0xE72719C1, 0x154C9AC2, 0x061C6936, 0xF477EA35,
    0xAA64D611, 0x580F5512, 0x4B5FA6E6, 0xB93425E5, 0x6DFE410E, 0x9F95C20D, 0x8CC531F9, 0x7EAEB2FA,
    0x30E349B1, 0xC288CAB2, 0xD1D83946, 0x23B3BA45, 0xF779DEAE, 0x05125DAD, 0x1642AE59, 0xE4292D5A,
    0xBA3A117E, 0x4851927D, 0x5B016189, 0xA96AE28A, 0x7DA08661, 0x8FCB0562, 0x9C9BF696, 0x6EF07595,
    0x417B1DBC, 0xB3109EBF, 0xA0406D4B, 0x522BEE48, 0x86E18AA3, 0x748A09A0, 0x67DAFA54, 0x95B17957,
    0xCBA24573, 0x39C9C670, 0x2A993584, 0xD8F2B687, 0x0C38D26C, 0xFE53516F, 0xED03A29B, 0x1F682198,
    0x5125DAD3, 0xA34E59D0, 0xB01EAA24, 0x42752927, 0x96BF4DCC, 0x64D4CECF, 0x77843D3B, 0x85EFBE38,
    0xDBFC821C, 0x2997011F, 0x3AC7F2EB, 0xC8AC71E8, 0x1C661503, 0xEE0D9600, 0xFD5D65F4, 0x0F36E6F7,
    0x61C69362, 0x93AD1061, 0x80FDE395, 0x72966096, 0xA65C047D, 0x5437877E, 0x4767748A, 0xB50CF789,
    0xEB1FCBAD, 0x197448AE, 0x0A24BB5A, 0xF84F3859, 0x2C855CB2, 0xDEEEDFB1, 0xCDBE2C45, 0x3FD5AF46,
    0x7198540D, 0x83F3D70E, 0x90A324FA, 0x62C8A7F9, 0xB602C312, 0x44694011, 0x5739B3E5, 0xA55230E6,
    0xFB410CC2, 0x092A8FC1, 0x1A7A7C35, 0xE811FF36, 0x3CDB9BDD, 0xCEB018DE, 0xDDE0EB2A, 0x2F8B6829,
    0x82F63B78, 0x709DB87B, 0x63CD4B8F, 0x91A6C88C, 0x456CAC67, 0xB7072F64, 0xA457DC90, 0x563C5F93,
    0x082F63B7, 0xFA44E0B4, 0xE9141340, 0x1B7F9043, 0xCFB5F4A8, 0x3DDE77AB, 0x2E8E845F, 0xDCE5075C,
    0x92A8FC17, 0x60C37F14, 0x73938CE0, 0x81F80FE3, 0x55326B08, 0xA759E80B, 0xB4091BFF, 0x466298FC,
    0x1871A4D8, 0xEA1A27DB, 0xF94AD42F, 0x0B21572C, 0xDFEB33C7, 0x2D80B0C4, 0x3ED04330, 0xCCBBC033,
    0xA24BB5A6, 0x502036A5, 0x4370C551, 0xB11B4652, 0x65D122B9, 0x97BAA1BA, 0x84EA524E, 0x7681D14D,
    0x2892ED69, 0xDAF96E6A, 0xC9A99D9E, 0x3BC21E9D, 0xEF087A76, 0x1D63F975, 0x0E330A81, 0xFC588982,
    0xB21572C9, 0x407EF1CA, 0x532E023E, 0xA145813D, 0x758FE5D6, 0x87E466D5, 0x94B49521, 0x66DF1622,
    0x38CC2A06, 0xCAA7A905, 0xD9F75AF1, 0x2B9CD9F2, 0xFF56BD19, 0x0D3D3E1A, 0x1E6DCDEE, 0xEC064EED,
    0xC38D26C4, 0x31E6A5C7, 0x22B65633, 0xD0DDD530, 0x0417B1DB, 0xF67C32D8, 0xE52CC12C, 0x1747422F,
    0x49547E0B, 0xBB3FFD08, 0xA86F0EFC, 0x5A048DFF, 0x8ECEE914, 0x7CA56A17, 0x6FF599E3, 0x9D9E1AE0,
    0xD3D3E1AB, 0x21B862A8, 0x32E8915C, 0xC083125F, 0x144976B4, 0xE622F5B7, 0xF5720643, 0x07198540,
    0x590AB964, 0xAB613A67, 0xB831C993, 0x4A5A4A90, 0x9E902E7B, 0x6CFBAD78, 0x7FAB5E8C, 0x8DC0DD8F,
    0xE330A81A, 0x115B2B19, 0x020BD8ED, 0xF0605BEE, 0x24AA3F05, 0xD6C1BC06, 0xC5914FF2, 0x37FACCF1,
    0x69E9F0D5, 0x9B8273D6, 0x88D28022, 0x7AB90321, 0xAE7367CA, 0x5C18E4C9, 0x4F48173D, 0xBD23943E,
    0xF36E6F75, 0x0105EC76, 0x12551F82, 0xE03E9C81, 0x34F4F86A, 0xC69F7B69, 0xD5CF889D, 0x27A40B9E,
    0x79B737BA, 0x8BDCB4B9, 0x988C474D, 0x6AE7C44E, 0xBE2DA0A5, 0x4C4623A6, 0x5F16D052, 0xAD7D5351,
];

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
    pub rep_distance: u64,
    pub rep_min_match: u64,
    pub uncompressed_length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveTail {
    pub archive_length: u64,
    pub index_offset: u64,
    pub index_total_length: u64,
    pub body_end: u64,
    pub plaintext_digest: Vec<u8>,
    pub encoded_digest: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DerivedLayout {
    pub tail_len: u64,
    pub tail_start: u64,
    pub body_end: u64,
    pub index_offset: u64,
    pub index_total_length: u64,
    pub encoded_domain_end: u64,
}

#[derive(Clone)]
pub struct GlobalDigest {
    inner: Digest,
}

#[derive(Clone, Debug)]
pub struct Crc32c {
    state: u32,
}

impl ArchiveHeader {
    pub fn from_config(config: &CompressionConfig, uncompressed_length: u64) -> Result<Self> {
        config.validate()?;
        if uncompressed_length > MAX_WIRE {
            return Err(Error::invalid_config(
                "uncompressed length exceeds MAX_WIRE",
            ));
        }
        let (rep_distance, rep_min_match) = match &config.rep_overlay {
            Some(overlay) => (overlay.distance, overlay.min_match),
            None => (0, 0),
        };
        let header = Self {
            version: VERSION,
            checksum: config.checksum,
            layout: config.layout,
            method: config.method,
            semantic_flags: config.semantic_flags(),
            block_size: config.block_size,
            min_match: config.min_match,
            seed_size: config.header_seed_size()?,
            target_chunk: config.header_target_chunk(),
            max_distance: config.header_max_distance(),
            rep_distance,
            rep_min_match,
            uncompressed_length,
        };
        header.validate()?;
        Ok(header)
    }

    /// Convert a validated header into [`CompressionConfig`].
    ///
    /// `resources` is caller-supplied and is never replaced by a hidden
    /// default. Method/layout/min/seed/target/distance/REP mapping follows
    /// the v3 header representation so the decoder can compute effective
    /// minimum and global distance without dropping fields.
    pub fn to_compression_config(&self, resources: ResourceConfig) -> Result<CompressionConfig> {
        self.validate()?;
        let seed_size = match self.method {
            Method::M3FixedDigest | Method::M4Reread => Some(self.seed_size),
            _ => None,
        };
        let target_chunk = match self.method {
            Method::M1RollingCdc | Method::M2Order1Cdc => Some(self.target_chunk),
            _ => None,
        };
        let max_distance = (self.max_distance != 0).then_some(self.max_distance);
        let rep_overlay = (self.semantic_flags & 1 == 1).then_some(RepConfig {
            distance: self.rep_distance,
            min_match: self.rep_min_match,
        });
        Ok(CompressionConfig {
            method: self.method,
            layout: self.layout,
            checksum: self.checksum,
            block_size: self.block_size,
            min_match: self.min_match,
            seed_size,
            target_chunk,
            max_distance,
            rep_overlay,
            resources,
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            return Err(Error::unsupported_version(format!(
                "SREP-NG version {} is not supported",
                self.version
            )));
        }
        if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&self.block_size) {
            return Err(Error::corrupt_header("block size outside wire limits"));
        }
        if !(MIN_MATCH_LEN..=MAX_MATCH_LEN).contains(&self.min_match) {
            return Err(Error::corrupt_header("minimum match outside wire limits"));
        }
        if self.max_distance > MAX_WIRE || self.uncompressed_length > MAX_WIRE {
            return Err(Error::corrupt_header("length or distance exceeds MAX_WIRE"));
        }
        if self.semantic_flags & !1 != 0 {
            return Err(Error::corrupt_header("nonzero reserved semantic flags"));
        }
        let overlay = self.semantic_flags & 1 == 1;
        if overlay
            && matches!(
                self.method,
                Method::M0Rep | Method::M1RollingCdc | Method::M2Order1Cdc
            )
        {
            return Err(Error::corrupt_header(
                "REP overlay flag is invalid for this method",
            ));
        }
        match self.method {
            Method::M0Rep => {
                if self.seed_size != self.min_match || self.target_chunk != 0 {
                    return Err(Error::corrupt_header("invalid m0 seed or target fields"));
                }
            }
            Method::M1RollingCdc => {
                if self.seed_size != 48
                    || !(32..=MAX_MATCH_LEN).contains(&self.target_chunk)
                    || self.target_chunk < self.min_match
                {
                    return Err(Error::corrupt_header("invalid m1 seed or target fields"));
                }
            }
            Method::M2Order1Cdc => {
                if self.seed_size != 0
                    || !(32..=MAX_MATCH_LEN).contains(&self.target_chunk)
                    || self.target_chunk < self.min_match
                {
                    return Err(Error::corrupt_header("invalid m2 seed or target fields"));
                }
            }
            Method::M3FixedDigest | Method::M4Reread => {
                if self.seed_size == 0 || self.seed_size > MAX_MATCH_LEN || self.target_chunk != 0 {
                    return Err(Error::corrupt_header("invalid m3/m4 seed or target fields"));
                }
            }
            Method::M5Exhaustive => {
                let expected = m5_seed_size(self.min_match)
                    .map_err(|_| Error::corrupt_header("invalid m5 minimum match"))?;
                if self.seed_size != expected || self.target_chunk != 0 {
                    return Err(Error::corrupt_header("invalid m5 seed or target fields"));
                }
            }
        }
        if overlay {
            if self.rep_distance == 0 || self.rep_distance > MAX_WIRE {
                return Err(Error::corrupt_header("invalid overlay rep_distance"));
            }
            if !(MIN_MATCH_LEN..=MAX_MATCH_LEN).contains(&self.rep_min_match) {
                return Err(Error::corrupt_header("invalid overlay rep_min_match"));
            }
        } else if self.rep_distance != 0 || self.rep_min_match != 0 {
            return Err(Error::corrupt_header("overlay fields must be zero"));
        }
        Ok(())
    }

    pub fn effective_min_match(&self) -> Result<u64> {
        self.validate()?;
        Ok(if self.semantic_flags & 1 == 1 {
            self.min_match.min(self.rep_min_match)
        } else {
            self.min_match
        })
    }

    pub fn rep_region_size(&self) -> Option<u64> {
        (self.semantic_flags & 1 == 1).then_some(self.rep_min_match.saturating_div(8).max(1))
    }

    pub fn block_count(&self) -> Result<u64> {
        block_count(self.uncompressed_length, self.block_size)
    }
}

pub fn tail_len(checksum: Checksum) -> usize {
    TAIL_PREFIX_LEN + 2 * checksum.width()
}

pub fn block_count(uncompressed_length: u64, block_size: u64) -> Result<u64> {
    if uncompressed_length == 0 {
        return Ok(0);
    }
    if block_size == 0 {
        return Err(Error::corrupt_header("block size is zero"));
    }
    Ok(uncompressed_length.div_ceil(block_size))
}

pub fn block_len_at(uncompressed_length: u64, block_size: u64, block_id: u64) -> Result<u64> {
    let count = block_count(uncompressed_length, block_size)?;
    if block_id >= count {
        return Err(Error::corrupt_record("block is outside input"));
    }
    let start = block_id
        .checked_mul(block_size)
        .ok_or_else(|| Error::corrupt_record("block start overflows"))?;
    let remaining = uncompressed_length
        .checked_sub(start)
        .ok_or_else(|| Error::corrupt_record("block start exceeds uncompressed length"))?;
    Ok(remaining.min(block_size))
}

pub fn max_selected_matches(uncompressed_length: u64, effective_min_match: u64) -> u64 {
    if uncompressed_length == 0 || effective_min_match == 0 {
        return 0;
    }
    (uncompressed_length / effective_min_match).min(uncompressed_length / POSITIVE_GAIN_MIN_LEN)
}

pub fn empty_archive_len(layout: Layout, checksum: Checksum) -> u64 {
    let tail = tail_len(checksum) as u64;
    let index = u64::from(layout.is_index());
    HEADER_LEN as u64 + index + tail
}

pub fn encode_archive_header(header: &ArchiveHeader) -> Result<[u8; HEADER_LEN]> {
    header.validate()?;
    let mut bytes = [0u8; HEADER_LEN];
    bytes[..8].copy_from_slice(&NG_V3_MAGIC);
    bytes[8] = VERSION;
    bytes[9] = 0;
    bytes[10] = header.checksum.wire_id();
    bytes[11] = header.layout.wire_id();
    bytes[12] = header.method.wire_id();
    bytes[13] = header.semantic_flags;
    bytes[16..24].copy_from_slice(&header.block_size.to_le_bytes());
    bytes[24..32].copy_from_slice(&header.min_match.to_le_bytes());
    bytes[32..40].copy_from_slice(&header.seed_size.to_le_bytes());
    bytes[40..48].copy_from_slice(&header.target_chunk.to_le_bytes());
    bytes[48..56].copy_from_slice(&header.max_distance.to_le_bytes());
    bytes[56..64].copy_from_slice(&header.rep_distance.to_le_bytes());
    bytes[64..72].copy_from_slice(&header.rep_min_match.to_le_bytes());
    bytes[72..80].copy_from_slice(&header.uncompressed_length.to_le_bytes());
    Ok(bytes)
}

pub fn parse_archive_header(bytes: &[u8]) -> Result<ArchiveHeader> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::truncated("truncated NG v3 archive header"));
    }
    if bytes[..8] != NG_V3_MAGIC {
        return Err(Error::corrupt_header("invalid NG v3 magic"));
    }
    let version = bytes[8];
    if version != VERSION {
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
    if bytes[14] != 0 || bytes[15] != 0 {
        return Err(Error::corrupt_header("nonzero header reserved bytes"));
    }
    let header = ArchiveHeader {
        version,
        checksum,
        layout,
        method,
        semantic_flags,
        block_size: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        min_match: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        seed_size: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
        target_chunk: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
        max_distance: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
        rep_distance: u64::from_le_bytes(bytes[56..64].try_into().unwrap()),
        rep_min_match: u64::from_le_bytes(bytes[64..72].try_into().unwrap()),
        uncompressed_length: u64::from_le_bytes(bytes[72..80].try_into().unwrap()),
    };
    header.validate()?;
    Ok(header)
}

pub fn encode_archive_tail(tail: &ArchiveTail, checksum: Checksum) -> Result<Vec<u8>> {
    let width = checksum.width();
    if tail.plaintext_digest.len() != width || tail.encoded_digest.len() != width {
        return Err(Error::invalid_config("v3 tail digest width mismatch"));
    }
    let mut bytes = vec![0u8; tail_len(checksum)];
    bytes[..8].copy_from_slice(&TAIL_MAGIC);
    bytes[8] = VERSION;
    bytes[12..20].copy_from_slice(&tail.archive_length.to_le_bytes());
    bytes[20..28].copy_from_slice(&tail.index_offset.to_le_bytes());
    bytes[28..36].copy_from_slice(&tail.index_total_length.to_le_bytes());
    bytes[36..44].copy_from_slice(&tail.body_end.to_le_bytes());
    bytes[44..44 + width].copy_from_slice(&tail.plaintext_digest);
    bytes[44 + width..].copy_from_slice(&tail.encoded_digest);
    Ok(bytes)
}

pub fn parse_archive_tail(bytes: &[u8], checksum: Checksum) -> Result<ArchiveTail> {
    let expected = tail_len(checksum);
    if bytes.len() < expected {
        return Err(Error::truncated("truncated NG v3 archive tail"));
    }
    if bytes.len() != expected {
        return Err(Error::corrupt_record(
            "NG v3 archive tail has the wrong length",
        ));
    }
    if bytes[..8] != TAIL_MAGIC {
        return Err(Error::corrupt_record("invalid NG v3 tail magic"));
    }
    if bytes[8] != VERSION {
        return Err(Error::corrupt_record("invalid NG v3 tail version"));
    }
    if bytes[9] != 0 {
        return Err(Error::corrupt_record("nonzero tail flags"));
    }
    if bytes[10] != 0 || bytes[11] != 0 {
        return Err(Error::corrupt_record("nonzero tail reserved bytes"));
    }
    let width = checksum.width();
    Ok(ArchiveTail {
        archive_length: u64::from_le_bytes(bytes[12..20].try_into().unwrap()),
        index_offset: u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
        index_total_length: u64::from_le_bytes(bytes[28..36].try_into().unwrap()),
        body_end: u64::from_le_bytes(bytes[36..44].try_into().unwrap()),
        plaintext_digest: bytes[44..44 + width].to_vec(),
        encoded_digest: bytes[44 + width..].to_vec(),
    })
}

pub fn validate_tail_layout(
    tail: &ArchiveTail,
    layout: Layout,
    checksum: Checksum,
    physical_len: u64,
) -> Result<DerivedLayout> {
    let tail_len_u64 = u64::try_from(tail_len(checksum))
        .map_err(|_| Error::corrupt_record("v3 tail length overflows u64"))?;
    if tail.archive_length != physical_len {
        return Err(Error::corrupt_record(
            "tail archive_length does not equal physical length",
        ));
    }
    let tail_start = tail
        .archive_length
        .checked_sub(tail_len_u64)
        .ok_or_else(|| Error::corrupt_record("archive_length is shorter than the fixed tail"))?;
    let width = u64::try_from(checksum.width())
        .map_err(|_| Error::corrupt_record("digest width overflows u64"))?;
    let encoded_domain_end = tail_start
        .checked_add(TAIL_PREFIX_LEN as u64)
        .and_then(|offset| offset.checked_add(width))
        .ok_or_else(|| Error::corrupt_record("encoded digest domain end overflows"))?;
    if layout.is_index() {
        if tail.index_offset < HEADER_LEN as u64
            || tail.body_end != tail.index_offset
            || tail.index_total_length < 1
        {
            return Err(Error::corrupt_index("invalid Index-LZ tail locators"));
        }
        let index_end = tail
            .index_offset
            .checked_add(tail.index_total_length)
            .ok_or_else(|| Error::corrupt_index("Index-LZ locator overflow"))?;
        if index_end != tail_start {
            return Err(Error::corrupt_index(
                "Index-LZ index_offset + index_total_length != tail_start",
            ));
        }
    } else {
        if tail.index_offset != 0 || tail.index_total_length != 0 {
            return Err(Error::corrupt_index(
                "nonzero Index locators on a non-Index layout",
            ));
        }
        if tail.body_end != tail_start {
            return Err(Error::corrupt_record(
                "body_end must equal tail_start for Future-LZ and I/O-LZ",
            ));
        }
    }
    Ok(DerivedLayout {
        tail_len: tail_len_u64,
        tail_start,
        body_end: tail.body_end,
        index_offset: tail.index_offset,
        index_total_length: tail.index_total_length,
        encoded_domain_end,
    })
}

pub fn encode_uleb128_to(buf: &mut [u8], value: u64) -> Result<usize> {
    if value > MAX_WIRE {
        return Err(Error::invalid_config("value exceeds MAX_WIRE"));
    }
    let mut value = value;
    let mut written = 0usize;
    while value >= 0x80 {
        if written >= buf.len() {
            return Err(Error::invalid_config("ULEB128 buffer too small"));
        }
        buf[written] = (value as u8) | 0x80;
        value >>= 7;
        written += 1;
    }
    if written >= buf.len() {
        return Err(Error::invalid_config("ULEB128 buffer too small"));
    }
    buf[written] = value as u8;
    Ok(written + 1)
}

pub fn encode_uleb128_into(value: u64, out: &mut Vec<u8>) -> Result<usize> {
    let mut buf = [0u8; MAX_ULEB128_LEN];
    let n = encode_uleb128_to(&mut buf, value)?;
    out.extend_from_slice(&buf[..n]);
    Ok(n)
}

pub fn write_uleb128<W: Write>(writer: &mut W, value: u64) -> Result<usize> {
    let mut buf = [0u8; MAX_ULEB128_LEN];
    let n = encode_uleb128_to(&mut buf, value)?;
    writer.write_all(&buf[..n]).map_err(Error::output_io)?;
    Ok(n)
}

pub fn decode_uleb128(bytes: &[u8]) -> Result<(u64, usize)> {
    decode_uleb128_with(bytes, false)
}

pub fn decode_uleb128_index(bytes: &[u8]) -> Result<(u64, usize)> {
    decode_uleb128_with(bytes, true)
}

pub fn read_uleb128<R: Read>(reader: &mut R) -> Result<u64> {
    read_uleb128_with(reader, "truncated ULEB128", false)
}

pub fn read_uleb128_index<R: Read>(reader: &mut R) -> Result<u64> {
    read_uleb128_with(reader, "truncated index ULEB128", true)
}

fn uleb_error(as_index: bool, context: &str) -> Error {
    if as_index {
        Error::corrupt_index(context)
    } else {
        Error::corrupt_record(context)
    }
}

fn decode_uleb128_with(bytes: &[u8], as_index: bool) -> Result<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for i in 0..MAX_ULEB128_LEN {
        let byte = *bytes
            .get(i)
            .ok_or_else(|| uleb_error(as_index, "truncated ULEB128"))?;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            if byte == 0 && i > 0 {
                return Err(uleb_error(as_index, "non-shortest ULEB128"));
            }
            if result > MAX_WIRE {
                return Err(uleb_error(as_index, "ULEB128 exceeds MAX_WIRE"));
            }
            return Ok((result, i + 1));
        }
        shift += 7;
    }
    Err(uleb_error(as_index, "ULEB128 continuation after byte 9"))
}

fn read_uleb128_with<R: Read>(reader: &mut R, eof: &str, as_index: bool) -> Result<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for i in 0..MAX_ULEB128_LEN {
        let mut byte = [0u8; 1];
        reader
            .read_exact(&mut byte)
            .map_err(|error| Error::map_eof(error, eof))?;
        result |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            if byte[0] == 0 && i > 0 {
                return Err(uleb_error(as_index, "non-shortest ULEB128"));
            }
            if result > MAX_WIRE {
                return Err(uleb_error(as_index, "ULEB128 exceeds MAX_WIRE"));
            }
            return Ok(result);
        }
        shift += 7;
    }
    Err(uleb_error(as_index, "ULEB128 continuation after byte 9"))
}

pub fn encode_triple_into(a: u64, b: u64, c: u64, out: &mut Vec<u8>) -> Result<usize> {
    let mut n = encode_uleb128_into(a, out)?;
    n += encode_uleb128_into(b, out)?;
    n += encode_uleb128_into(c, out)?;
    Ok(n)
}

pub fn decode_triple(bytes: &[u8]) -> Result<(u64, u64, u64, usize)> {
    decode_triple_with(bytes, decode_uleb128)
}

pub fn decode_triple_index(bytes: &[u8]) -> Result<(u64, u64, u64, usize)> {
    decode_triple_with(bytes, decode_uleb128_index)
}

fn decode_triple_with(
    bytes: &[u8],
    decode: fn(&[u8]) -> Result<(u64, usize)>,
) -> Result<(u64, u64, u64, usize)> {
    let (a, n0) = decode(bytes)?;
    let (b, n1) = decode(&bytes[n0..])?;
    let (c, n2) = decode(&bytes[n0 + n1..])?;
    Ok((a, b, c, n0 + n1 + n2))
}

pub fn encode_compact_index(matches: &[Match]) -> Result<Vec<u8>> {
    let count = u64::try_from(matches.len())
        .map_err(|_| Error::invalid_match("match count exceeds MAX_WIRE"))?;
    if count > MAX_WIRE {
        return Err(Error::invalid_match("match count exceeds MAX_WIRE"));
    }
    let mut out = Vec::new();
    encode_uleb128_into(count, &mut out)?;
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
        encode_triple_into(dst_gap, distance, item.len, &mut out)?;
        previous_end = end;
    }
    Ok(out)
}

pub fn decode_compact_index(
    bytes: &[u8],
    uncompressed_length: u64,
    effective_min_match: u64,
    max_distance: u64,
) -> Result<Vec<Match>> {
    if bytes.is_empty() {
        return Err(Error::corrupt_index("IndexSection must not be empty"));
    }
    let (count, mut cursor) = decode_uleb128_index(bytes)?;
    if count > MAX_WIRE {
        return Err(Error::corrupt_index("match_count exceeds MAX_WIRE"));
    }
    let remaining = bytes.len() - cursor;
    let max_by_bytes = remaining / 3;
    let max_by_ir = max_selected_matches(uncompressed_length, effective_min_match);
    if count > max_by_bytes as u64 || count > max_by_ir {
        return Err(Error::corrupt_index("match_count exceeds section bounds"));
    }
    let mut matches = Vec::new();
    let mut previous_end = 0u64;
    let mut previous: Option<Match> = None;
    for origin_match_id in 0..count {
        let (dst_gap, distance, total_len, n) = decode_triple_index(&bytes[cursor..])?;
        cursor += n;
        if distance == 0 {
            return Err(Error::invalid_match(
                "compact index distance must be positive",
            ));
        }
        let dst = previous_end
            .checked_add(dst_gap)
            .ok_or_else(|| Error::invalid_match("compact index destination overflows"))?;
        let src = dst
            .checked_sub(distance)
            .ok_or_else(|| Error::invalid_match("compact index source underflows"))?;
        let end = dst
            .checked_add(total_len)
            .ok_or_else(|| Error::invalid_match("compact index endpoint overflows"))?;
        if end > uncompressed_length || src >= uncompressed_length {
            return Err(Error::invalid_match("compact index interval exceeds input"));
        }
        if total_len < effective_min_match || total_len < POSITIVE_GAIN_MIN_LEN {
            return Err(Error::invalid_match(
                "compact index length is below the effective minimum",
            ));
        }
        if max_distance != 0 && distance > max_distance {
            return Err(Error::invalid_match(
                "compact index distance exceeds max_distance",
            ));
        }
        let item = Match {
            src,
            dst,
            len: total_len,
            origin_match_id,
        };
        if let Some(prev) = previous {
            if dst < prev.dst
                || (dst == prev.dst && src < prev.src)
                || (dst == prev.dst && src == prev.src && total_len > prev.len)
                || (dst == prev.dst && src == prev.src && total_len == prev.len)
            {
                return Err(Error::invalid_match(
                    "compact index triples are not in canonical order",
                ));
            }
            if prev
                .dst
                .checked_add(prev.len)
                .is_some_and(|prev_end| dst < prev_end)
            {
                return Err(Error::invalid_match("compact index destinations overlap"));
            }
        }
        previous = Some(item);
        previous_end = end;
        matches.push(item);
    }
    if cursor != bytes.len() {
        return Err(Error::corrupt_index(
            "IndexSection underconsumed or has trailing bytes",
        ));
    }
    Ok(matches)
}

impl Crc32c {
    pub fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.state;
        for &byte in bytes {
            let index = ((crc ^ u32::from(byte)) & 0xff) as usize;
            crc = CRC32C_TABLE[index] ^ (crc >> 8);
        }
        self.state = crc;
    }

    pub fn finalize(self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }

    pub fn finalize_le(self) -> [u8; 4] {
        self.finalize().to_le_bytes()
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut hasher = Crc32c::new();
    hasher.update(bytes);
    hasher.finalize()
}

pub fn crc32c_le(bytes: &[u8]) -> [u8; 4] {
    crc32c(bytes).to_le_bytes()
}

impl GlobalDigest {
    pub fn plaintext(kind: Checksum) -> Self {
        let mut inner = Digest::new(kind);
        inner.update(PLAINTEXT_DOMAIN);
        Self { inner }
    }

    pub fn encoded(kind: Checksum) -> Self {
        let mut inner = Digest::new(kind);
        inner.update(ENCODED_DOMAIN);
        Self { inner }
    }

    pub fn update(&mut self, physical_bytes: &[u8]) {
        self.inner.update(physical_bytes);
    }

    pub fn finalize(self) -> Vec<u8> {
        self.inner.finalize()
    }
}

pub fn plaintext_digest(kind: Checksum, plaintext: &[u8]) -> Vec<u8> {
    let mut digest = GlobalDigest::plaintext(kind);
    digest.update(plaintext);
    digest.finalize()
}

pub fn encoded_digest(kind: Checksum, physical_prefix: &[u8]) -> Vec<u8> {
    let mut digest = GlobalDigest::encoded(kind);
    digest.update(physical_prefix);
    digest.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn m1_header(layout: Layout, checksum: Checksum, uncompressed_length: u64) -> ArchiveHeader {
        ArchiveHeader {
            version: VERSION,
            checksum,
            layout,
            method: Method::M1RollingCdc,
            semantic_flags: 0,
            block_size: 1024,
            min_match: 32,
            seed_size: 48,
            target_chunk: 4096,
            max_distance: 0,
            rep_distance: 0,
            rep_min_match: 0,
            uncompressed_length,
        }
    }

    #[test]
    fn uleb128_shortest_roundtrip_and_limits() {
        let cases = [0u64, 1, 127, 128, (1 << 14) - 1, 1 << 14, MAX_WIRE];
        for value in cases {
            let mut buf = [0u8; MAX_ULEB128_LEN];
            let n = encode_uleb128_to(&mut buf, value).unwrap();
            assert!(n <= MAX_ULEB128_LEN);
            let (decoded, used) = decode_uleb128(&buf[..n]).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(used, n);
            if value == 0 {
                assert_eq!(&buf[..n], &[0x00]);
            }
        }
        let mut max = [0u8; MAX_ULEB128_LEN];
        let n = encode_uleb128_to(&mut max, MAX_WIRE).unwrap();
        assert_eq!(n, 9);
        assert_eq!(&max[..8], &[0xFF; 8]);
        assert_eq!(max[8], 0x7F);
        assert_eq!(
            encode_uleb128_to(&mut [0u8; 9], MAX_WIRE + 1)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidConfiguration
        );
        assert_eq!(
            decode_uleb128(&[0x80, 0x00]).unwrap_err().kind(),
            ErrorKind::CorruptRecord
        );
        assert_eq!(
            decode_uleb128_index(&[0x80; 9]).unwrap_err().kind(),
            ErrorKind::CorruptIndex
        );
        let mut cur = Cursor::new([0x80u8, 0x01]);
        assert_eq!(read_uleb128(&mut cur).unwrap(), 128);
        let mut short = Cursor::new([0x80u8]);
        assert_eq!(
            read_uleb128(&mut short).unwrap_err().kind(),
            ErrorKind::TruncatedArchive
        );
    }

    #[test]
    fn crc32c_known_vector_and_streaming() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_le(b"123456789"), [0x83, 0x92, 0x06, 0xE3]);
        assert_eq!(crc32c(b""), 0);
        let mut hasher = Crc32c::new();
        hasher.update(b"123");
        hasher.update(b"456789");
        assert_eq!(hasher.finalize(), 0xE306_9283);
        assert_eq!(crc32c(b"abc"), crc32c(b"abc"));
    }

    #[test]
    fn header_roundtrip_and_error_kinds() {
        let header = m1_header(Layout::Index, Checksum::Xxh3, 3);
        let bytes = encode_archive_header(&header).unwrap();
        assert_eq!(bytes.len(), 80);
        assert_eq!(&bytes[..8], &NG_V3_MAGIC);
        assert_eq!(parse_archive_header(&bytes).unwrap(), header);
        let mut version = bytes;
        version[8] = 4;
        assert_eq!(
            parse_archive_header(&version).unwrap_err().kind(),
            ErrorKind::UnsupportedVersion
        );
        let mut flags = bytes;
        flags[9] = 1;
        assert_eq!(
            parse_archive_header(&flags).unwrap_err().kind(),
            ErrorKind::CorruptHeader
        );
        let mut checksum = bytes;
        checksum[10] = 9;
        assert_eq!(
            parse_archive_header(&checksum).unwrap_err().kind(),
            ErrorKind::UnknownChecksum
        );
        let mut reserved = bytes;
        reserved[14] = 1;
        assert_eq!(
            parse_archive_header(&reserved).unwrap_err().kind(),
            ErrorKind::CorruptHeader
        );
        assert_eq!(
            parse_archive_header(&bytes[..20]).unwrap_err().kind(),
            ErrorKind::TruncatedArchive
        );
    }

    #[test]
    fn header_config_conversion_preserves_resources() {
        let resources = ResourceConfig {
            memory: 7 * 1024 * 1024,
            temp_dir: PathBuf::from("/tmp/opencode/v3-header-resources"),
            temp_limit: 11 * 1024 * 1024,
            output_limit: 12345,
        };
        let config = CompressionConfig {
            method: Method::M1RollingCdc,
            layout: Layout::Io,
            checksum: Checksum::Blake3,
            block_size: 1024,
            min_match: 32,
            seed_size: None,
            target_chunk: Some(4096),
            max_distance: Some(1000),
            rep_overlay: None,
            resources: resources.clone(),
        };
        let header = ArchiveHeader::from_config(&config, 3).unwrap();
        assert_eq!(header.seed_size, 48);
        assert_eq!(header.target_chunk, 4096);
        assert_eq!(header.max_distance, 1000);
        let restored = header.to_compression_config(resources.clone()).unwrap();
        assert_eq!(restored, config);
        assert_eq!(restored.resources.temp_dir, resources.temp_dir);
        assert_eq!(restored.effective_min_match().unwrap(), 32);
    }

    #[test]
    fn overlay_header_maps_effective_min_and_derived_region() {
        let config = CompressionConfig {
            method: Method::M3FixedDigest,
            min_match: 512,
            seed_size: Some(512),
            target_chunk: None,
            rep_overlay: Some(RepConfig {
                distance: 4096,
                min_match: 64,
            }),
            ..CompressionConfig::default()
        };
        let header = ArchiveHeader::from_config(&config, 2048).unwrap();
        assert_eq!(header.semantic_flags, 1);
        assert_eq!(header.effective_min_match().unwrap(), 64);
        assert_eq!(header.rep_region_size(), Some(8));
        let restored = header
            .to_compression_config(config.resources.clone())
            .unwrap();
        assert_eq!(restored.rep_overlay, config.rep_overlay);
        assert_eq!(restored.seed_size, Some(512));
    }

    #[test]
    fn tail_layout_equations_empty_and_literal_index() {
        let checksum = Checksum::Xxh3;
        assert_eq!(empty_archive_len(Layout::Index, checksum), 157);
        assert_eq!(empty_archive_len(Layout::Future, checksum), 156);
        assert_eq!(empty_archive_len(Layout::Io, Checksum::Blake3), 188);
        let empty_index = ArchiveTail {
            archive_length: 157,
            index_offset: 80,
            index_total_length: 1,
            body_end: 80,
            plaintext_digest: vec![0; 16],
            encoded_digest: vec![0; 16],
        };
        let derived = validate_tail_layout(&empty_index, Layout::Index, checksum, 157).unwrap();
        assert_eq!(derived.tail_start, 81);
        assert_eq!(derived.encoded_domain_end, 157 - 16);
        let encoded = encode_archive_tail(&empty_index, checksum).unwrap();
        assert_eq!(encoded.len(), 76);
        assert_eq!(parse_archive_tail(&encoded, checksum).unwrap(), empty_index);

        let abc_index = ArchiveTail {
            archive_length: 1190,
            index_offset: 1113,
            index_total_length: 1,
            body_end: 1113,
            plaintext_digest: vec![0; 16],
            encoded_digest: vec![0; 16],
        };
        let derived = validate_tail_layout(&abc_index, Layout::Index, checksum, 1190).unwrap();
        assert_eq!(derived.tail_start, 1114);

        let mut future = empty_index.clone();
        future.archive_length = 156;
        future.index_offset = 0;
        future.index_total_length = 0;
        future.body_end = 80;
        validate_tail_layout(&future, Layout::Future, checksum, 156).unwrap();
        future.index_offset = 80;
        assert_eq!(
            validate_tail_layout(&future, Layout::Future, checksum, 156)
                .unwrap_err()
                .kind(),
            ErrorKind::CorruptIndex
        );
        let mut bad_body = empty_index.clone();
        bad_body.body_end = 79;
        assert_eq!(
            validate_tail_layout(&bad_body, Layout::Index, checksum, 157)
                .unwrap_err()
                .kind(),
            ErrorKind::CorruptIndex
        );
        let mut tail_version = encode_archive_tail(&empty_index, checksum).unwrap();
        tail_version[8] = 4;
        assert_eq!(
            parse_archive_tail(&tail_version, checksum)
                .unwrap_err()
                .kind(),
            ErrorKind::CorruptRecord
        );
    }

    #[test]
    fn compact_index_empty_and_touching_origins() {
        assert_eq!(encode_compact_index(&[]).unwrap(), vec![0x00]);
        assert_eq!(
            decode_compact_index(&[0x00], 0, 32, 0).unwrap(),
            Vec::<Match>::new()
        );
        let matches = [
            Match {
                src: 0,
                dst: 1024,
                len: 64,
                origin_match_id: 0,
            },
            Match {
                src: 64,
                dst: 1088,
                len: 64,
                origin_match_id: 1,
            },
        ];
        let bytes = encode_compact_index(&matches).unwrap();
        let mut expected = Vec::new();
        encode_uleb128_into(2, &mut expected).unwrap();
        encode_triple_into(1024, 1024, 64, &mut expected).unwrap();
        encode_triple_into(0, 1024, 64, &mut expected).unwrap();
        assert_eq!(bytes, expected);
        let decoded = decode_compact_index(&bytes, 1152, 32, 0).unwrap();
        assert_eq!(decoded, matches);
        assert_eq!(
            decode_compact_index(&bytes, 1152, 32, 1023)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidMatch
        );
        let mut trailing = bytes.clone();
        trailing.push(0x00);
        assert_eq!(
            decode_compact_index(&trailing, 1152, 32, 0)
                .unwrap_err()
                .kind(),
            ErrorKind::CorruptIndex
        );
    }

    #[test]
    fn block_count_and_digest_domains() {
        assert_eq!(block_count(0, 1024).unwrap(), 0);
        assert_eq!(block_count(1024, 1024).unwrap(), 1);
        assert_eq!(block_count(1025, 1024).unwrap(), 2);
        assert_eq!(block_len_at(1025, 1024, 0).unwrap(), 1024);
        assert_eq!(block_len_at(1025, 1024, 1).unwrap(), 1);
        assert_eq!(PLAINTEXT_DOMAIN.len(), 18);
        assert_eq!(ENCODED_DOMAIN.len(), 16);
        assert_eq!(PLAINTEXT_DOMAIN, b"SREPNG3-PLAINTEXT\0");
        assert_eq!(ENCODED_DOMAIN, b"SREPNG3-ENCODED\0");
        for kind in [Checksum::Xxh3, Checksum::Blake3] {
            let mut expected = Digest::new(kind);
            expected.update(PLAINTEXT_DOMAIN);
            expected.update(b"abc");
            assert_eq!(plaintext_digest(kind, b"abc"), expected.finalize());
            let mut encoded = Digest::new(kind);
            encoded.update(ENCODED_DOMAIN);
            encoded.update(&[1, 2, 3]);
            assert_eq!(encoded_digest(kind, &[1, 2, 3]), encoded.finalize());
            let mut streaming = GlobalDigest::encoded(kind);
            streaming.update(&[1]);
            streaming.update(&[2, 3]);
            assert_eq!(streaming.finalize(), encoded_digest(kind, &[1, 2, 3]));
            assert_eq!(plaintext_digest(kind, b"").len(), kind.width());
        }
        assert_eq!(max_selected_matches(0, 32), 0);
        assert_eq!(max_selected_matches(100, 32), 100 / 32);
    }
}
