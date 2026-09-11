use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::error::{Error, Result};

use super::checksum::ChecksumKind;

pub const SIGNATURE: [u8; 8] = [0x17, 0x18, 0x35, 0x26, 0x53, 0x52, 0x45, 0x50];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    IoRounded,
    Io,
    Future,
    Index,
}

impl Layout {
    pub const fn name(self) -> &'static str {
        match self {
            Self::IoRounded => "io-rounded",
            Self::Io => "io",
            Self::Future => "future",
            Self::Index => "index",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Header {
    pub version: u8,
    pub checksum: ChecksumKind,
    pub layout: Layout,
    pub base_len: u32,
    pub seed: Vec<u8>,
    pub header_end: u64,
}

pub fn parse_header(bytes: &[u8]) -> Result<Header> {
    if bytes.len() < 16 {
        return Err(Error::truncated("truncated legacy archive header"));
    }
    if bytes[..8] != SIGNATURE {
        return Err(Error::corrupt_header("invalid legacy signature words"));
    }
    let packed = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let version = (packed & 0xff) as u8;
    if !(1..=4).contains(&version) {
        return Err(Error::unsupported_version(format!(
            "legacy SREP version {version} is not supported"
        )));
    }
    let checksum = descriptor(packed)?;
    let seed_len = checksum.seed_len();
    let base_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    if matches!(version, 1 | 2) && base_len == 0 {
        return Err(Error::corrupt_header("legacy BASE_LEN must be positive"));
    }
    let end = 16usize
        .checked_add(seed_len)
        .ok_or_else(|| Error::corrupt_header("legacy seed length overflows"))?;
    if bytes.len() < end {
        return Err(Error::truncated("truncated legacy archive seed"));
    }
    Ok(Header {
        version,
        checksum,
        layout: match version {
            1 => Layout::IoRounded,
            2 => Layout::Io,
            3 => Layout::Future,
            _ => Layout::Index,
        },
        base_len,
        seed: bytes[16..end].to_vec(),
        header_end: end as u64,
    })
}

pub fn descriptor(packed: u32) -> Result<ChecksumKind> {
    let id = ((packed >> 8) & 0xff) as u8;
    let seed_len = ((packed >> 16) & 0xff) as u8;
    let bias = (packed >> 24) as u8;
    let width = bias.wrapping_add(16);
    match (id, seed_len, bias, width) {
        (0, 0, 0, 16) => Ok(ChecksumKind::Md5),
        (1, 0, 0, 16) => Ok(ChecksumKind::None),
        (2, 0, 4, 20) => Ok(ChecksumKind::Sha1),
        (3, 0, 0x30, 64) => Ok(ChecksumKind::Sha512),
        (4, 32, 0, 16) => Ok(ChecksumKind::Vhash),
        (5, 16, 0xf8, 8) => Ok(ChecksumKind::SipHash),
        _ => Err(Error::unknown_checksum(
            "legacy checksum descriptor is invalid",
        )),
    }
}

pub fn parse_seekable(file: &mut File) -> Result<Header> {
    file.seek(SeekFrom::Start(0)).map_err(Error::temp_storage)?;
    let mut fixed = [0u8; 16];
    file.read_exact(&mut fixed)
        .map_err(|e| Error::map_eof(e, "truncated legacy archive header"))?;
    let packed = u32::from_le_bytes(fixed[8..12].try_into().unwrap());
    let checksum = descriptor(packed)?;
    let mut seed = vec![0u8; checksum.seed_len()];
    file.read_exact(&mut seed)
        .map_err(|e| Error::map_eof(e, "truncated legacy archive seed"))?;
    let mut bytes = fixed.to_vec();
    bytes.extend_from_slice(&seed);
    parse_header(&bytes)
}
