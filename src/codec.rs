use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tempfile::{Builder, NamedTempFile};

use crate::config::{Checksum, CompressionConfig, Layout, Method, ResourceConfig};
use crate::dispatch::{ArchiveKind, read_and_classify};
use crate::error::{Error, Result};
use crate::resource::{Reservation, ResourceContext};

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
    crate::v3::writer::compress_with_candidates(input, output, config, candidates)
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
    crate::v3::writer::compress_with_candidates_with_context(
        input, output, config, candidates, context,
    )
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
        Method::M0Rep => crate::match_finder::m0::find_matches_m0_spooled(&spool, config, context)?,
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
            let candidates = crate::match_finder::fixed::find_matches_spooled_for_normalization(
                &spool,
                config,
                context,
                config.method,
            )?;
            return crate::v3::writer::compress_spooled_with_candidates(
                spool, config, candidates, context, output,
            );
        }
        Method::M5Exhaustive => {
            let candidates =
                crate::match_finder::m5::find_matches_spooled(&spool, config, context)?;
            return crate::v3::writer::compress_spooled_with_candidates(
                spool, config, candidates, context, output,
            );
        }
    };
    crate::v3::writer::compress_spooled_with_candidates(spool, config, candidates, context, output)
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
    validate_resources(resources)?;
    let context = ResourceContext::with_resources(resources)?;
    decode(input, std::io::sink(), resources, &context, false)
}

pub fn inspect<R: Read>(input: R) -> Result<ArchiveInfo> {
    inspect_archive_with_resources(input, &ResourceConfig::default())
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
        ArchiveKind::NgV3 => crate::v3::reader::inspect_matches(
            std::io::Cursor::new(header_bytes).chain(input),
            resources,
            context,
        ),
        ArchiveKind::PrototypeV1 => Err(Error::unsupported_version(
            "experimental SREP-NG v1 is not supported",
        )),
        ArchiveKind::Legacy => Err(Error::unsupported_version(
            "match inspection is only defined for SREP-NG v3",
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
    let (kind, header_bytes) = read_and_classify(&mut input)?;
    match kind {
        ArchiveKind::NgV3 => crate::v3::reader::decode(
            std::io::Cursor::new(header_bytes).chain(input),
            output,
            resources,
            context,
            write,
        ),
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
                reference_count: result.match_count,
                semantic_match_count: result.match_count,
                covered_bytes: result.covered_bytes,
                literal_bytes: result.literal_bytes,
                ..CompressionStats::default()
            })
        }
    }
}

fn inspect_archive_with_resources<R: Read>(
    mut input: R,
    resources: &ResourceConfig,
) -> Result<ArchiveInfo> {
    validate_resources(resources)?;
    let context = ResourceContext::with_resources(resources)?;
    let (kind, header_bytes) = read_and_classify(&mut input)?;
    match kind {
        ArchiveKind::NgV3 => crate::v3::reader::inspect(
            std::io::Cursor::new(header_bytes).chain(input),
            resources,
            &context,
        ),
        ArchiveKind::PrototypeV1 => Err(Error::unsupported_version(
            "experimental SREP-NG v1 is not supported",
        )),
        ArchiveKind::Legacy => {
            let result = crate::legacy::decode(
                header_bytes,
                &mut input,
                &mut std::io::sink(),
                resources,
                &context,
                false,
            )?;
            Ok(ArchiveInfo {
                version: result.version,
                legacy_layout: Some(result.layout.name().to_owned()),
                legacy_checksum: Some(result.checksum.name().to_owned()),
                legacy_base_len: Some(result.base_len as u64),
                original_size: result.original_size,
                payload_size: result.archive_size.saturating_sub(result.header_size),
                block_count: result.block_count,
                semantic_match_count: result.match_count,
                covered_bytes: result.covered_bytes,
                literal_bytes: result.literal_bytes,
                ..ArchiveInfo::default()
            })
        }
    }
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
    fs::create_dir_all(&resources.temp_dir).map_err(Error::temp_storage)?;
    let temp = temp_builder(Builder::new())
        .prefix("srep-input-")
        .tempfile_in(&resources.temp_dir)
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
        if new_len > resources.output_limit || new_len > crate::config::MAX_UNCOMPRESSED {
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
        fs::create_dir_all(&resources.temp_dir).map_err(Error::temp_storage)?;
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

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

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
}
