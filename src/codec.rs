use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tempfile::{Builder, NamedTempFile};

use crate::config::{Checksum, CompressionConfig, Layout, Method, ResourceConfig};
use crate::dispatch::{ArchiveKind, read_and_classify};
use crate::error::{Error, Result};
use crate::offload::session::{
    CleanupEntry, CompressionSession, IoReservation, Operation, ResourceLease, SessionId,
    SessionIo, WorkspaceKind,
};
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

/// A completely written archive held in the private staging file.  The
/// digest is computed before the object is handed to a public writer; callers
/// cannot publish an unsealed prefix through the bounded-copy seam.
pub(crate) struct SealedStagedFile {
    spool: TempSpool,
    digest: [u8; 32],
    session: SessionId,
    generation: u64,
}

impl SealedStagedFile {
    pub(crate) fn len(&self) -> u64 {
        self.spool.len
    }

    /// The writer supplies its append-stream digest, never a digest adopted from
    /// reading potentially corrupted disk. Preflight verifies these expectations.
    pub(crate) fn seal(
        mut spool: TempSpool,
        expected_len: u64,
        expected_digest: [u8; 32],
        session: &CompressionSession,
    ) -> Result<Self> {
        session.check_cancelled()?;
        spool.ensure_clean()?;
        if spool.len != expected_len
            || spool.file.metadata().map_err(Error::temp_storage)?.len() != expected_len
        {
            return Err(Error::corrupt_record("staged writer length mismatch"));
        }
        spool.file.flush().map_err(Error::temp_storage)?;
        Ok(Self {
            spool,
            digest: expected_digest,
            session: session.identity(),
            generation: session.next_generation()?,
        })
    }
}

#[derive(Debug)]
pub(crate) struct CopyPermit {
    length: u64,
    digest: [u8; 32],
    io: IoReservation,
    session: SessionId,
    generation: u64,
}

impl CopyPermit {
    pub(crate) fn remaining_io(&self) -> u128 {
        self.io.remaining()
    }
}

/// Validate and reserve the complete private publication envelope before the
/// first byte is sent to an arbitrary caller-owned writer.  The three charges
/// are the uncached integrity read, the copy read, and public output bytes.
pub(crate) fn prepare_publication(
    staged: &mut SealedStagedFile,
    session: &CompressionSession,
) -> Result<CopyPermit> {
    session.check_cancelled()?;
    if staged.session != session.identity() {
        return Err(Error::corrupt_record("cross-session staged archive"));
    }
    let physical_len = staged
        .spool
        .file
        .metadata()
        .map_err(Error::temp_storage)?
        .len();
    if physical_len != staged.len() {
        return Err(Error::corrupt_record(
            "staged archive length changed before publication",
        ));
    }
    let staged_len = staged.len();
    // Admission precedes the uncached integrity read.  If the session cannot
    // afford the complete envelope, no staged bytes are read and no caller
    // output can be attempted.
    let mut io = session.reserve_publication_io(staged_len)?;
    let observed = digest_staged_file(&mut staged.spool.file, staged_len, session, &mut io)?;
    if observed != staged.digest {
        return Err(Error::corrupt_record(
            "staged archive digest changed before publication",
        ));
    }
    session.checkpoint(Operation::PreRewind)?;
    staged.spool.rewind()?;
    Ok(CopyPermit {
        length: staged.len(),
        digest: staged.digest,
        io,
        session: session.identity(),
        generation: staged.generation,
    })
}

/// Copy a sealed archive with a known length.  A short staged read is an input
/// failure, while a short public write is an output failure; neither is
/// converted into success merely because all bytes happened to be emitted.
pub(crate) fn copy_staged_exact<W: Write>(
    staged: &mut SealedStagedFile,
    output: &mut W,
    mut permit: CopyPermit,
    session: &CompressionSession,
) -> Result<()> {
    session.check_cancelled()?;
    if permit.length != staged.len()
        || permit.digest != staged.digest
        || permit.generation != staged.generation
        || permit.session != staged.session
        || permit.session != session.identity()
    {
        return Err(Error::corrupt_record(
            "publication permit does not match staged archive",
        ));
    }
    staged.spool.rewind()?;
    let mut second_digest = blake3::Hasher::new();
    let mut remaining = permit.length;
    let mut buffer = session.workspace(WorkspaceKind::Publication)?;
    while remaining != 0 {
        publication_checkpoint(session, Operation::CopyRead)?;
        let requested = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| Error::input_io(std::io::Error::other("publication read is too large")))?;
        let count =
            read_retry(&mut staged.spool.file, &mut buffer[..requested]).map_err(|error| {
                Error::input_io_with_context(error, "staged publication read failed")
            })?;
        if count == 0 {
            return Err(Error::input_io_with_context(
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "staged archive ended early",
                ),
                "staged publication read ended before the sealed length",
            ));
        }
        permit.io.consume(count as u64)?;
        second_digest.update(&buffer[..count]);
        write_exact_count(output, &buffer[..count], &mut permit.io, session)?;
        remaining -= count as u64;
    }
    if second_digest.finalize().as_bytes() != &permit.digest {
        return Err(Error::corrupt_record(
            "staged archive digest changed during publication",
        ));
    }
    publication_checkpoint(session, Operation::CopyFlush)?;
    output.flush().map_err(Error::output_io)
}

fn digest_staged_file(
    file: &mut File,
    length: u64,
    session: &CompressionSession,
    io: &mut IoReservation,
) -> Result<[u8; 32]> {
    file.seek(SeekFrom::Start(0)).map_err(Error::temp_storage)?;
    let mut hasher = blake3::Hasher::new();
    let mut remaining = length;
    let mut buffer = session.workspace(WorkspaceKind::Publication)?;
    while remaining != 0 {
        publication_checkpoint(session, Operation::PreRead)?;
        let requested = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| Error::temp_storage_context("staged digest length is too large"))?;
        let count = read_retry(file, &mut buffer[..requested]).map_err(Error::input_io)?;
        if count == 0 {
            return Err(Error::input_io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "staged archive ended before its recorded length",
            )));
        }
        io.consume(count as u64)?;
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let physical_len = file.metadata().map_err(Error::temp_storage)?.len();
    if physical_len != length {
        return Err(Error::temp_storage_context(
            "staged archive has bytes beyond its sealed length",
        ));
    }
    Ok(*hasher.finalize().as_bytes())
}

fn publication_checkpoint(session: &CompressionSession, operation: Operation) -> Result<()> {
    // Cancellation retains its resource/cancel identity; injected read failure
    // follows the same InputIo mapping as an actual staged read error.
    if let Err(error) = session.checkpoint(operation) {
        session.check_cancelled()?;
        let source = std::io::Error::other(error.to_string());
        return Err(
            if matches!(operation, Operation::CopyWrite | Operation::CopyFlush) {
                Error::output_io(source)
            } else {
                Error::input_io_with_context(source, "staged publication read failed")
            },
        );
    }
    Ok(())
}

fn read_retry<R: Read>(reader: &mut R, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn write_exact_count<W: Write>(
    writer: &mut W,
    mut bytes: &[u8],
    io: &mut IoReservation,
    session: &CompressionSession,
) -> Result<()> {
    while !bytes.is_empty() {
        publication_checkpoint(session, Operation::CopyWrite)?;
        let written = loop {
            match writer.write(bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => break result,
            }
        }
        .map_err(Error::output_io)?;
        if written == 0 {
            return Err(Error::output_io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "public publication writer accepted no bytes",
            )));
        }
        io.consume(written as u64)?;
        bytes = &bytes[written..];
    }
    Ok(())
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

#[derive(Debug)]
pub(crate) struct TempSpool {
    pub(crate) file: File,
    pub(crate) _temp: NamedTempFile,
    pub(crate) reservation: Reservation,
    pub(crate) len: u64,
    budget_error: Option<Error>,
    // Private offload constructor counts both NamedTempFile and reopened File.
    _session_leases: Option<(ResourceLease, ResourceLease)>,
    _directory: Option<std::sync::Arc<tempfile::TempDir>>,
    single_file_limit: u64,
    session_io: Option<SessionIo>,
    contaminated: bool,
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
        if self.contaminated {
            return Err(std::io::Error::other("contaminated temporary spool"));
        }
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
            _session_leases: None,
            _directory: None,
            single_file_limit: u64::MAX,
            session_io: None,
            contaminated: false,
        })
    }

    pub(crate) fn new_in_session(session: &CompressionSession) -> Result<Self> {
        let handles = session.acquire_handles(2)?;
        let file_slot = session.acquire_file_slot()?;
        let reservation = session.temp().reserve(0)?;
        let directory = session.directory_owner()?;
        session.checkpoint(Operation::Create)?;
        let mut builder = Builder::new();
        builder.prefix("staged-").disable_cleanup(true);
        let temp = temp_builder(builder)
            .tempfile_in(directory.path())
            .map_err(Error::temp_storage)?;
        let opened = session
            .checkpoint(Operation::Open)
            .and_then(|_| temp.reopen().map_err(Error::temp_storage));
        let file = match opened {
            Ok(file) => file,
            Err(error) => {
                let path = temp.path().to_path_buf();
                drop(temp);
                drop(handles);
                if fs::remove_file(&path).is_err() {
                    session.io_access().defer_cleanup(CleanupEntry {
                        path,
                        bytes: reservation,
                        file: file_slot,
                        directory,
                    });
                }
                return Err(error);
            }
        };
        Ok(Self {
            file,
            _temp: temp,
            reservation,
            len: 0,
            budget_error: None,
            _session_leases: Some((handles, file_slot)),
            _directory: Some(directory),
            single_file_limit: session.policy().max_file_bytes.min(session.temp().limit()),
            session_io: Some(session.io_access()),
            contaminated: false,
        })
    }

    pub(crate) fn write_at(&mut self, position: u64, bytes: &[u8]) -> Result<()> {
        self.ensure_clean()?;
        if let Some(io) = self.session_io.clone() {
            return self.write_at_session(position, bytes, &io);
        }
        let byte_len = u64::try_from(bytes.len())
            .map_err(|_| Error::temp_limit("temporary write length overflows"))?;
        let end = position
            .checked_add(byte_len)
            .ok_or_else(|| Error::temp_limit("temporary write offset overflows"))?;
        let growth = end.saturating_sub(self.len);
        if end > self.single_file_limit {
            return Err(Error::temp_limit("offload single-file limit exceeded"));
        }
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

    fn ensure_clean(&self) -> Result<()> {
        if self.contaminated {
            Err(Error::temp_storage_context(
                "contaminated temporary spool cannot be reused or sealed",
            ))
        } else {
            Ok(())
        }
    }
    fn write_at_session(&mut self, position: u64, bytes: &[u8], io: &SessionIo) -> Result<()> {
        let byte_len = u64::try_from(bytes.len())
            .map_err(|_| Error::temp_limit("temporary length overflow"))?;
        let end = position
            .checked_add(byte_len)
            .ok_or_else(|| Error::temp_limit("temporary offset overflow"))?;
        if end > self.single_file_limit {
            return Err(Error::temp_limit("offload single-file limit exceeded"));
        }
        let mut permit = io.reserve_io(u128::from(byte_len))?;
        let growth = end.saturating_sub(self.len);
        self.reservation.grow(growth)?;
        let result = (|| {
            io.checkpoint(Operation::Seek)?;
            self.file
                .seek(SeekFrom::Start(position))
                .map_err(Error::temp_storage)?;
            let mut written = 0;
            while written < bytes.len() {
                io.checkpoint(Operation::Write)?;
                // Explicit bounded syscall loop exposes every successful partial
                // write to accounting and scoped fault injection.
                let end = (written + 64 * 1024).min(bytes.len());
                let n = match self.file.write(&bytes[written..end]) {
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    other => other.map_err(Error::temp_storage)?,
                };
                if n == 0 {
                    return Err(Error::temp_storage(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "temporary write made no progress",
                    )));
                }
                permit.consume(n as u64)?;
                written += n;
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.contaminated = true;
            if io.checkpoint(Operation::Truncate).is_ok() && self.file.set_len(self.len).is_ok() {
                self.reservation.shrink(growth);
            }
            // Failed truncation retains the complete pre-write growth charge.
            // Overwrites also remain contaminated even if length was restored.
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
        self.ensure_clean()?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(Error::temp_storage)
            .map(|_| ())
    }

    pub(crate) fn take_budget_error(&mut self) -> Option<Error> {
        self.budget_error.take()
    }
}

impl Drop for TempSpool {
    fn drop(&mut self) {
        let Some(io) = self.session_io.as_ref() else {
            return;
        };
        // On Windows open handles may prevent removal: transfer the charge to
        // deferred cleanup, then normal field drops close both handles. Never
        // refund before remove/truncate is proven. NamedTempFile may itself
        // remove later; retry treats NotFound as proven cleanup.
        let removed = io
            .checkpoint(Operation::Remove)
            .and_then(|_| fs::remove_file(self._temp.path()).map_err(Error::temp_storage))
            .is_ok();
        if !removed {
            let (handles, mut file) = self._session_leases.take().expect("session leases");
            // Keep handle charge through normal field drops; only file/bytes
            // belong to the cleanup entry.
            self._session_leases = Some((handles, file.split(0).expect("zero split")));
            let bytes = std::mem::replace(&mut self.reservation, io.empty_temp_reservation());
            io.defer_cleanup(CleanupEntry {
                path: self._temp.path().to_path_buf(),
                bytes,
                file,
                directory: self._directory.take().expect("session directory"),
            });
        }
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
    use super::*;
    use crate::offload::session::{CompressionSession, IoLimit, OffloadPolicy};
    use std::fs::File;

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

    #[test]
    fn publication_preflight_charges_three_passes_and_copies_exactly() {
        let resources = ResourceConfig::default();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let session = CompressionSession::new(
            &context,
            &resources,
            OffloadPolicy {
                io_limit: IoLimit::Finite(3 * 70_000),
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        let mut spool = TempSpool::new(&resources, &context).unwrap();
        let payload = vec![0xA5; 70_000];
        spool.append(&payload).unwrap();
        let mut staged = SealedStagedFile::seal(
            spool,
            payload.len() as u64,
            *blake3::hash(&payload).as_bytes(),
            &session,
        )
        .unwrap();
        let permit = prepare_publication(&mut staged, &session).unwrap();
        assert_eq!(permit.remaining_io(), 2 * 70_000);
        let mut output = Vec::new();
        copy_staged_exact(&mut staged, &mut output, permit, &session).unwrap();
        assert_eq!(output, payload);
        assert_eq!(session.io_used(), 3 * 70_000);
        assert_eq!(session.io_reserved(), 0);
    }

    #[test]
    fn publication_budget_failure_happens_before_public_write() {
        let resources = ResourceConfig::default();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let session = CompressionSession::new(
            &context,
            &resources,
            OffloadPolicy {
                io_limit: IoLimit::Finite(2),
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        let mut spool = TempSpool::new(&resources, &context).unwrap();
        spool.append(b"publication").unwrap();
        let mut staged = SealedStagedFile::seal(
            spool,
            11,
            *blake3::hash(b"publication").as_bytes(),
            &session,
        )
        .unwrap();
        assert!(prepare_publication(&mut staged, &session).is_err());
        assert_eq!(session.io_reserved(), 0);
    }

    #[test]
    fn publication_detects_digest_change_before_copy() {
        let resources = ResourceConfig::default();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let session =
            CompressionSession::new(&context, &resources, OffloadPolicy::default()).unwrap();
        let mut spool = TempSpool::new(&resources, &context).unwrap();
        spool.append(b"stable").unwrap();
        let mut staged =
            SealedStagedFile::seal(spool, 6, *blake3::hash(b"stable").as_bytes(), &session)
                .unwrap();
        staged.spool.file.seek(SeekFrom::Start(0)).unwrap();
        staged.spool.file.write_all(b"mutant").unwrap();
        staged.spool.file.flush().unwrap();
        let result = prepare_publication(&mut staged, &session);
        assert_eq!(
            result.unwrap_err().kind(),
            crate::error::ErrorKind::CorruptRecord
        );
    }

    fn publication_fixture() -> (ResourceContext, CompressionSession, SealedStagedFile) {
        let r = ResourceConfig {
            memory: 2 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let bytes = vec![42; 150_000];
        let mut spool = TempSpool::new_in_session(&s).unwrap();
        spool.append(&bytes).unwrap();
        let staged = SealedStagedFile::seal(
            spool,
            bytes.len() as u64,
            *blake3::hash(&bytes).as_bytes(),
            &s,
        )
        .unwrap();
        (c, s, staged)
    }
    #[test]
    fn publication_accounts_partial_preread_and_failed_rewind() {
        use crate::offload::session::FaultAction;
        let (c, s, mut staged) = publication_fixture();
        s.inject(Operation::PreRead, 1, FaultAction::Fail);
        assert_eq!(
            prepare_publication(&mut staged, &s).unwrap_err().kind(),
            crate::ErrorKind::InputIo
        );
        assert_eq!(s.io_used(), 150_000 + 65536);
        assert_eq!(s.io_reserved(), 0);
        s.inject(Operation::PreRewind, 0, FaultAction::Fail);
        assert!(prepare_publication(&mut staged, &s).is_err());
        assert_eq!(s.io_used(), 150_000 + 65536 + 150_000);
        assert_eq!(s.io_reserved(), 0);
        drop(staged);
        drop(s);
        assert_eq!(c.temp.current(), 0);
        assert_eq!(c.memory.current(), 0);
    }
    #[test]
    fn publication_rejects_preseal_mutation_and_wrong_identity() {
        let (c, s, mut staged) = publication_fixture();
        // Re-seal with trusted writer expectation after same-length disk mutation.
        staged.spool.file.seek(SeekFrom::Start(0)).unwrap();
        staged.spool.file.write_all(&[99]).unwrap();
        let expected = staged.digest;
        let mut resealed = SealedStagedFile::seal(staged.spool, 150_000, expected, &s).unwrap();
        assert_eq!(
            prepare_publication(&mut resealed, &s).unwrap_err().kind(),
            crate::ErrorKind::CorruptRecord
        );
        assert_eq!(s.io_used(), 300_000);
        assert_eq!(s.io_reserved(), 0);
        let payload = vec![42; 150_000];
        let mut other = TempSpool::new(s.resources(), &c).unwrap();
        other.append(&payload).unwrap();
        let mut a =
            SealedStagedFile::seal(other, 150_000, *blake3::hash(&payload).as_bytes(), &s).unwrap();
        let permit = prepare_publication(&mut a, &s).unwrap();
        let mut out = Vec::new();
        assert!(copy_staged_exact(&mut resealed, &mut out, permit, &s).is_err());
        assert!(out.is_empty());
        assert_eq!(s.io_reserved(), 0);
    }
    #[test]
    fn publication_late_read_and_cancellation_leave_prefix() {
        use crate::offload::session::FaultAction;
        for cancel in [false, true] {
            let (c, s, mut staged) = publication_fixture();
            let permit = prepare_publication(&mut staged, &s).unwrap();
            s.inject(
                Operation::CopyRead,
                2,
                if cancel {
                    FaultAction::Cancel
                } else {
                    FaultAction::Fail
                },
            );
            let mut out = Vec::new();
            let e = copy_staged_exact(&mut staged, &mut out, permit, &s).unwrap_err();
            assert_eq!(out, vec![42; 131072]);
            assert_eq!(
                e.kind(),
                if cancel {
                    crate::ErrorKind::TemporaryStorageFailure
                } else {
                    crate::ErrorKind::InputIo
                }
            );
            assert_eq!(s.io_used(), 300_000 + 2 * 131072);
            assert_eq!(s.io_reserved(), 0);
            drop(staged);
            drop(s);
            assert_eq!(c.temp.current(), 0);
            assert_eq!(c.memory.current(), 0);
        }
        let (_, s, mut staged) = publication_fixture();
        let permit = prepare_publication(&mut staged, &s).unwrap();
        s.cancel();
        let mut out = Vec::new();
        assert!(copy_staged_exact(&mut staged, &mut out, permit, &s).is_err());
        assert!(out.is_empty());
        assert_eq!(s.io_used(), 300_000);
        assert_eq!(s.io_reserved(), 0);
    }
    #[test]
    fn publication_after_preflight_corruption_and_output_errors() {
        let (_, s, mut staged) = publication_fixture();
        let permit = prepare_publication(&mut staged, &s).unwrap();
        staged.spool.file.seek(SeekFrom::Start(140_000)).unwrap();
        staged.spool.file.write_all(&[99]).unwrap();
        let mut out = Vec::new();
        assert_eq!(
            copy_staged_exact(&mut staged, &mut out, permit, &s)
                .unwrap_err()
                .kind(),
            crate::ErrorKind::CorruptRecord
        );
        assert_eq!(out.len(), 150_000);
        assert_eq!(s.io_used(), 600_000);
        assert_eq!(s.io_reserved(), 0);
        struct Writer {
            accepted: usize,
            fail_write: bool,
            interrupted: bool,
        }
        impl Write for Writer {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                if self.fail_write && self.accepted >= 100 {
                    return Err(std::io::Error::other("write fault"));
                }
                let n = b.len().min(100);
                self.accepted += n;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("flush fault"))
            }
        }
        for fail_write in [true, false] {
            let (_, s, mut staged) = publication_fixture();
            let permit = prepare_publication(&mut staged, &s).unwrap();
            let mut w = Writer {
                accepted: 0,
                fail_write,
                interrupted: false,
            };
            assert_eq!(
                copy_staged_exact(&mut staged, &mut w, permit, &s)
                    .unwrap_err()
                    .kind(),
                crate::ErrorKind::OutputIo
            );
            assert_eq!(
                s.io_used(),
                if fail_write {
                    300_000 + 65536 + 100
                } else {
                    600_000
                }
            );
            assert_eq!(s.io_reserved(), 0);
        }
    }

    #[test]
    fn publication_read_retry_handles_short_reads_and_interrupted() {
        struct Short {
            interrupted: bool,
            remaining: usize,
        }
        impl Read for Short {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let n = b.len().min(self.remaining).min(3);
                b[..n].fill(5);
                self.remaining -= n;
                Ok(n)
            }
        }
        let mut source = Short {
            interrupted: false,
            remaining: 7,
        };
        let mut b = [0; 16];
        assert_eq!(read_retry(&mut source, &mut b).unwrap(), 3);
        assert_eq!(read_retry(&mut source, &mut b).unwrap(), 3);
        assert_eq!(read_retry(&mut source, &mut b).unwrap(), 1);
        assert_eq!(read_retry(&mut source, &mut b).unwrap(), 0);
    }

    #[test]
    fn session_spool_partial_write_accounts_bytes_and_never_reuses_contamination() {
        use crate::offload::session::FaultAction;
        for truncate_fails in [false, true] {
            let r = ResourceConfig {
                memory: 2 * 1024 * 1024,
                ..ResourceConfig::default()
            };
            let c = ResourceContext::with_resources(&r).unwrap();
            let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
            let mut spool = TempSpool::new_in_session(&s).unwrap();
            spool.append(b"old").unwrap();
            s.inject(Operation::Write, 1, FaultAction::Fail);
            if truncate_fails {
                s.inject(Operation::Truncate, 0, FaultAction::Fail);
            }
            assert!(spool.append(&vec![9; 100_000]).is_err());
            assert_eq!(s.io_used(), 3 + 65536);
            assert_eq!(s.io_reserved(), 0);
            assert_eq!(c.temp.current(), if truncate_fails { 100_003 } else { 3 });
            assert_eq!(
                spool.file.metadata().unwrap().len(),
                if truncate_fails { 65539 } else { 3 }
            );
            assert!(spool.append(b"reuse").is_err());
            assert!(spool.rewind().is_err());
            // Even claiming the observed physical length/digest cannot bless it.
            let len = spool.file.metadata().unwrap().len();
            assert!(SealedStagedFile::seal(spool, len, [0; 32], &s).is_err());
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), 0);
            assert_eq!(s.file_usage().current, 0);
        }
    }

    #[test]
    fn failed_unlink_retains_shared_budget_until_verified_cleanup() {
        use crate::offload::session::FaultAction;
        let r = ResourceConfig {
            memory: 2 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let clone = c.clone();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let mut spool = TempSpool::new_in_session(&s).unwrap();
        spool.append(&[7; 1024]).unwrap();
        let path = spool._temp.path().to_path_buf();
        s.inject(Operation::Remove, 0, FaultAction::Fail);
        drop(spool);
        assert!(path.exists());
        assert_eq!(clone.temp.current(), 1024);
        assert_eq!(s.file_usage().current, 1);
        s.retry_cleanup().unwrap();
        assert!(!path.exists());
        assert_eq!(clone.temp.current(), 0);
        assert_eq!(s.file_usage().current, 0);
    }

    #[test]
    fn partial_write_failed_truncate_and_remove_keep_all_charges() {
        use crate::offload::session::FaultAction;
        let r = ResourceConfig {
            memory: 2 * 1024 * 1024,
            temp_limit: 200_000,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let clone = c.clone();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let mut spool = TempSpool::new_in_session(&s).unwrap();
        let path = spool._temp.path().to_path_buf();
        s.inject(Operation::Write, 1, FaultAction::Fail);
        s.inject(Operation::Truncate, 0, FaultAction::Fail);
        s.inject(Operation::Remove, 0, FaultAction::Fail);
        assert!(spool.append(&vec![1; 100_000]).is_err());
        drop(spool);
        assert_eq!(fs::metadata(&path).unwrap().len(), 65536);
        assert_eq!(s.io_used(), 65536);
        assert_eq!(s.io_reserved(), 0);
        assert_eq!(clone.temp.current(), 100_000);
        assert!(clone.temp.reserve(100_001).is_err());
        assert_eq!(s.file_usage().current, 1);
        assert_eq!(s.handle_usage().current, 7);
        // A real retry failure (directory at path) must not refund either ledger.
        let saved = path.with_extension("saved");
        fs::rename(&path, &saved).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(s.retry_cleanup().is_err());
        assert_eq!(clone.temp.current(), 100_000);
        fs::remove_dir(&path).unwrap();
        fs::rename(&saved, &path).unwrap();
        s.retry_cleanup().unwrap();
        assert_eq!(clone.temp.current(), 0);
        assert_eq!(s.file_usage().current, 0);
        drop(s);
        assert_eq!(c.memory.current(), 0);
    }
}
