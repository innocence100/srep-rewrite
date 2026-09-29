//! The private candidate-validation boundary for the bounded pipeline.
//!
//! A finder can only hand the next stage a `CandidateBatch`.  The batch is
//! immutable and remains in canonical triple order.  `ValidatedCandidates`
//! can only be made by this module, after every canonical row has passed the
//! authoritative byte checks.  This deliberately leaves the public RAM
//! normalizer in `match_ir` untouched.

use std::io::{Read, Seek, SeekFrom};

use super::candidate_sink::CandidateStore;
use super::contracts::{
    CandidateRow, CandidateValidator, EnumerationComplete, RandomStore, RecordCursor, ReplayStore,
};
use super::session::{CompressionSession, Operation};
use super::sort::ExternalSorter;
use super::storage::{EndRow, IntervalRow, RecordPageCache, RecordTable};
use crate::codec::InputSpool;
use crate::error::{Error, Result};
use crate::match_ir::MatchCandidate;
use crate::resource::BudgetedVec;

const VALIDATION_BUFFER: usize = 4096;

/// A sealed candidate stream.  External input deliberately has a separate
/// constructor: it has no source-index completion token to counterfeit.
pub(crate) struct CandidateBatch {
    source: CandidateStore,
    enumeration_session: Option<super::session::SessionId>,
}

impl CandidateBatch {
    pub(super) fn from_external(
        source: CandidateStore,
        input_len: u64,
        session: &CompressionSession,
    ) -> Result<Self> {
        check_source_identity(&source, input_len, session)?;
        Ok(Self {
            source,
            enumeration_session: None,
        })
    }

    pub(super) fn from_enumerated(
        source: CandidateStore,
        complete: EnumerationComplete,
        input_len: u64,
        session: &CompressionSession,
    ) -> Result<Self> {
        complete.check_session(session)?;
        check_source_identity(&source, input_len, session)?;
        Ok(Self {
            source,
            enumeration_session: Some(session.identity()),
        })
    }
}

/// The validated view still owns the sealed input store.  Rows of length at
/// most 25 are intentionally retained in that store but are excluded by the
/// read-only schedule-input adapter; they were nevertheless interval-checked
/// and therefore could not hide malformed input.
pub(crate) struct ValidatedCandidates {
    source: CandidateStore,
    input_len: u64,
    session: super::session::SessionId,
}

/// Read-only end-ordered input for W1.  No constructor is exposed outside the
/// validation module; callers receive it only through
/// `ValidatedCandidates::into_schedule_input`.
pub(crate) struct ScheduledCandidates {
    source: RecordTable<EndRow>,
    input_len: u64,
    count: u64,
}

impl ScheduledCandidates {
    pub(super) fn input_len(&self) -> u64 {
        self.input_len
    }

    pub(super) fn len(&self) -> u64 {
        self.count
    }

    pub(super) fn cursor(&self) -> Result<Box<dyn RecordCursor<EndRow> + '_>> {
        self.source.cursor()
    }

    /// Caller-owned bounded cache; no table constructor or mutable table escapes.
    /// Bounds errors do not poison this immutable store, unlike a live builder.
    pub(super) fn read_cached(&self, index: u64, cache: &mut RecordPageCache) -> Result<EndRow> {
        cache.read(&self.source, index)
    }
}

impl ReplayStore<EndRow> for ScheduledCandidates {
    fn len(&self) -> u64 {
        self.count
    }

    fn cursor(&self) -> Result<Box<dyn RecordCursor<EndRow> + '_>> {
        self.source.cursor()
    }
}

impl RandomStore<EndRow> for ScheduledCandidates {
    fn read_at(&self, index: u64) -> Result<EndRow> {
        self.source.read_at(index)
    }
}

pub(crate) type ScheduleInput = ScheduledCandidates;

impl ValidatedCandidates {
    pub(super) fn into_schedule_input(
        self,
        session: &CompressionSession,
    ) -> Result<ScheduledCandidates> {
        if self.session != session.identity() {
            return Err(Error::corrupt_index("cross-session validated candidates"));
        }

        // This is intentionally a second pass.  The first pass above is the
        // semantic/error-priority pass; no interval-sort allocation can mask a
        // later malformed canonical row.
        let source = self.source;
        let chunk = fixed_sort_chunk::<IntervalRow>(session)?;
        let mut intervals =
            ExternalSorter::new(session, chunk, self.input_len, same_destination_interval)?;
        let mut cursor = source.cursor()?;
        while let Some(row) = cursor.next()? {
            session.check_cancelled()?;
            if row.candidate.len > 25 {
                intervals.push(IntervalRow(row))?;
            }
        }
        drop(cursor);
        let interval_store = intervals.finish()?;
        drop(source);

        let chunk = fixed_sort_chunk::<EndRow>(session)?;
        let mut ends = ExternalSorter::new(session, chunk, self.input_len, no_reduction)?;
        let mut interval_cursor = interval_store.cursor()?;
        while let Some(row) = interval_cursor.next()? {
            ends.push(EndRow(row.0))?;
        }
        let end_store = ends.finish()?;
        let count = end_store.len();
        drop(interval_cursor);
        drop(interval_store);
        Ok(ScheduledCandidates {
            source: end_store,
            input_len: self.input_len,
            count,
        })
    }
}

/// Typed adapter used by the offload parent for ordinary external candidate
/// rows.  It has no finder or source-index construction capability.
pub(crate) struct Validator<'a> {
    spool: &'a InputSpool,
    min_match: u64,
}

impl<'a> Validator<'a> {
    pub(crate) fn new(spool: &'a InputSpool, min_match: u64) -> Self {
        Self { spool, min_match }
    }
}

impl CandidateValidator<CandidateStore> for Validator<'_> {
    type Validated = ValidatedCandidates;

    fn validate(
        self,
        source: CandidateStore,
        session: &CompressionSession,
    ) -> Result<Self::Validated> {
        validate_batch(
            CandidateBatch::from_external(source, self.spool.len, session)?,
            self.spool,
            self.min_match,
            session,
        )
    }
}

fn check_source_identity(
    source: &CandidateStore,
    input_len: u64,
    session: &CompressionSession,
) -> Result<()> {
    source.check_session(session)?;
    if source.input_len() != input_len {
        return Err(Error::corrupt_index(
            "candidate store input length mismatch",
        ));
    }
    Ok(())
}

pub(crate) fn validate_batch(
    batch: CandidateBatch,
    spool: &InputSpool,
    min_match: u64,
    session: &CompressionSession,
) -> Result<ValidatedCandidates> {
    // Check trusted store identity BEFORE cursor creation, file calls, or byte
    // validation. A valid enumeration token cannot launder another session's rows.
    check_source_identity(&batch.source, spool.len, session)?;
    if batch
        .enumeration_session
        .is_some_and(|identity| identity != session.identity())
    {
        return Err(Error::corrupt_index("cross-session candidate batch"));
    }
    session.check_cancelled()?;

    // Sealing is not a proof that disk stayed unchanged. Verify the complete
    // envelope and append-side digest uncached, then ALL canonical keys, before
    // any semantic byte validation. RAM needs the same order/uniqueness prepass.
    batch.source.verify_uncached()?;
    let mut cursor = batch.source.cursor()?;
    let mut previous: Option<CandidateRow> = None;
    while let Some(row) = cursor.next()? {
        session.check_cancelled()?;
        if let Some(old) = previous {
            if canonical_key(old.candidate) > canonical_key(row.candidate) {
                return Err(Error::corrupt_index("candidate triple order changed"));
            }
            if same_triple(old.candidate, row.candidate) {
                return Err(Error::corrupt_index("candidate triple was not reduced"));
            }
        }
        previous = Some(row);
    }
    drop(cursor);

    let _validation_buffers = session.memory().reserve((VALIDATION_BUFFER * 3) as u64)?;
    let _file_lease = session.acquire_handles(1)?;
    let mut file = spool.file.try_clone().map_err(Error::temp_storage)?;
    let mut expected = [0u8; VALIDATION_BUFFER];
    let mut actual = [0u8; VALIDATION_BUFFER];
    let mut period = [0u8; VALIDATION_BUFFER];
    let mut cursor = batch.source.cursor()?;
    while let Some(row) = cursor.next()? {
        session.check_cancelled()?;
        validate_row(
            row.candidate,
            spool.len,
            min_match,
            &mut file,
            session,
            &mut expected,
            &mut actual,
            &mut period,
        )?;
    }

    drop(cursor);
    Ok(ValidatedCandidates {
        source: batch.source,
        input_len: spool.len,
        session: session.identity(),
    })
}

fn fixed_sort_chunk<T: Copy>(session: &CompressionSession) -> Result<BudgetedVec<T>> {
    // Candidate validation intentionally uses a fixed, bounded chunk.  The
    // allocation is charged before any sorter file is opened.
    let count = (session.memory().limit() / 64).clamp(1, 4096) as usize;
    BudgetedVec::with_capacity(count, session.memory())
}

fn canonical_key(c: MatchCandidate) -> (u64, u64, u64, u64) {
    (c.src, c.dst, c.len, c.insertion_ordinal)
}

fn same_triple(a: MatchCandidate, b: MatchCandidate) -> bool {
    (a.src, a.dst, a.len) == (b.src, b.dst, b.len)
}

fn same_destination_interval(a: &IntervalRow, b: &IntervalRow) -> bool {
    (a.0.candidate.dst, a.0.candidate.len) == (b.0.candidate.dst, b.0.candidate.len)
}

fn no_reduction(_: &EndRow, _: &EndRow) -> bool {
    false
}

#[allow(clippy::too_many_arguments)]
fn validate_row<R: Read + Seek>(
    candidate: MatchCandidate,
    input_len: u64,
    min_match: u64,
    file: &mut R,
    session: &CompressionSession,
    expected: &mut [u8; VALIDATION_BUFFER],
    actual: &mut [u8; VALIDATION_BUFFER],
    period: &mut [u8; VALIDATION_BUFFER],
) -> Result<()> {
    let end = candidate
        .dst
        .checked_add(candidate.len)
        .ok_or_else(|| Error::invalid_match("candidate destination overflows"))?;
    if candidate.src >= candidate.dst || end > input_len || candidate.src >= input_len {
        return Err(Error::invalid_match("candidate interval is invalid"));
    }
    let distance = candidate.dst - candidate.src;
    let source_span = candidate.len.min(distance);
    candidate
        .src
        .checked_add(source_span)
        .ok_or_else(|| Error::invalid_match("candidate source overflows"))?;
    if candidate.len <= 25 {
        return Ok(());
    }
    if candidate.len < min_match {
        return Err(Error::invalid_match(
            "candidate is shorter than minimum match",
        ));
    }

    let period_len = if distance <= period.len() as u64 {
        let length = usize::try_from(distance)
            .map_err(|_| Error::invalid_match("candidate distance exceeds platform limits"))?;
        read_exact_accounted(
            file,
            candidate.src,
            &mut period[..length],
            session,
            "candidate source is unavailable",
        )?;
        length
    } else {
        0
    };
    let mut offset = 0u64;
    while offset < candidate.len {
        let count =
            usize::try_from((candidate.len - offset).min(expected.len() as u64)).map_err(|_| {
                Error::invalid_match("candidate validation length exceeds platform limits")
            })?;
        if period_len == 0 {
            read_periodic_accounted(
                file,
                candidate.src,
                offset % distance,
                distance,
                &mut expected[..count],
                session,
            )?;
        } else {
            let phase = usize::try_from(offset % distance)
                .map_err(|_| Error::invalid_match("candidate periodic phase overflows"))?;
            for (index, byte) in expected[..count].iter_mut().enumerate() {
                *byte = period[(phase + index) % period_len];
            }
        }
        let target = candidate
            .dst
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("candidate destination overflows"))?;
        read_exact_accounted(
            file,
            target,
            &mut actual[..count],
            session,
            "candidate destination is unavailable",
        )?;
        if expected[..count] != actual[..count] {
            return Err(Error::invalid_match("candidate bytes do not match"));
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("candidate validation offset overflows"))?;
    }
    Ok(())
}

fn read_periodic_accounted<R: Read + Seek>(
    file: &mut R,
    base: u64,
    mut offset: u64,
    period: u64,
    destination: &mut [u8],
    session: &CompressionSession,
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
        read_exact_accounted(
            file,
            position,
            &mut destination[written..written + count],
            session,
            "candidate source is unavailable",
        )?;
        written += count;
        offset = 0;
    }
    Ok(())
}

fn read_exact_accounted<R: Read + Seek>(
    file: &mut R,
    position: u64,
    destination: &mut [u8],
    session: &CompressionSession,
    context: &'static str,
) -> Result<()> {
    session.checkpoint(Operation::Seek)?;
    file.seek(SeekFrom::Start(position))
        .map_err(Error::temp_storage)?;
    let mut read = 0usize;
    while read < destination.len() {
        session.checkpoint(Operation::Read)?;
        let mut permit = session.reserve_io((destination.len() - read) as u128)?;
        match file.read(&mut destination[read..]) {
            Ok(0) => {
                return Err(Error::map_eof(
                    std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
                    context,
                ));
            }
            Ok(n) => {
                permit.consume(n as u64)?;
                read += n;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::map_eof(error, context)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResourceConfig;
    use crate::offload::candidate_sink::ExternalCandidateSink;
    use crate::offload::contracts::{CandidateRunSink, CandidateSink, Provenance};
    use crate::offload::session::OffloadPolicy;
    use crate::resource::ResourceContext;
    use std::io::Cursor;

    fn setup() -> (ResourceContext, CompressionSession, InputSpool) {
        let resources = ResourceConfig {
            memory: 16 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let context = ResourceContext::with_resources(&resources).unwrap();
        let session =
            CompressionSession::new(&context, &resources, OffloadPolicy::default()).unwrap();
        let spool = crate::codec::spool_input(
            Cursor::new(b"abcdefghijklmnop".repeat(256)),
            &resources,
            &context,
        )
        .unwrap();
        (context, session, spool)
    }

    fn row(src: u64, dst: u64, len: u64, ordinal: u64) -> CandidateRow {
        CandidateRow {
            candidate: MatchCandidate {
                src,
                dst,
                len,
                insertion_ordinal: ordinal,
            },
            provenance: Provenance::EXTERNAL,
        }
    }

    #[test]
    fn validation_precedes_identical_destination_reduction() {
        let (_context, session, spool) = setup();
        let mut sink = ExternalCandidateSink::with_input_len(&session, spool.len, 16).unwrap();
        sink.emit(row(1, 256, 30, 1)).unwrap(); // invalid bytes, smaller source
        sink.emit(row(0, 256, 30, 2)).unwrap(); // valid periodic bytes
        let batch =
            CandidateBatch::from_external(sink.finish().unwrap(), spool.len, &session).unwrap();
        match validate_batch(batch, &spool, 2, &session) {
            Ok(_) => panic!("invalid canonical candidate was accepted"),
            Err(error) => assert_eq!(error.kind(), crate::error::ErrorKind::InvalidMatch),
        }
    }

    #[test]
    fn duplicate_minimum_ordinal_is_retained_before_validation() {
        let (_context, session, spool) = setup();
        let mut sink = ExternalCandidateSink::with_input_len(&session, spool.len, 1).unwrap();
        sink.emit(row(0, 256, 30, 9)).unwrap();
        sink.emit(row(0, 256, 30, 3)).unwrap();
        let batch =
            CandidateBatch::from_external(sink.finish().unwrap(), spool.len, &session).unwrap();
        let validated = validate_batch(batch, &spool, 2, &session).unwrap();
        let mut cursor = validated.source.cursor().unwrap();
        assert_eq!(
            cursor.next().unwrap().unwrap().candidate.insertion_ordinal,
            3
        );
    }

    fn fixture(
        mode: super::super::session::SpillMode,
        input: &[u8],
    ) -> (ResourceContext, CompressionSession, InputSpool) {
        let resources = ResourceConfig {
            memory: 16 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let context = ResourceContext::with_resources(&resources).unwrap();
        let session = CompressionSession::new(
            &context,
            &resources,
            OffloadPolicy {
                spill_mode: mode,
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        let spool = crate::codec::spool_input(Cursor::new(input), &resources, &context).unwrap();
        (context, session, spool)
    }

    fn store(
        session: &CompressionSession,
        input_len: u64,
        chunk: usize,
        rows: &[CandidateRow],
    ) -> CandidateStore {
        let mut sink = ExternalCandidateSink::with_input_len(session, input_len, chunk).unwrap();
        for &row in rows {
            sink.emit(row).unwrap();
        }
        sink.finish().unwrap()
    }

    fn err<T>(result: Result<T>) -> Error {
        match result {
            Ok(_) => panic!("expected failure"),
            Err(e) => e,
        }
    }

    #[test]
    fn complete_store_prepass_beats_early_semantic_error() {
        use super::super::session::SpillMode;
        use super::super::storage::{FixedRecord, RecordMeta};
        use std::io::Write;
        for damage in ["checksum", "truncate", "digest", "order", "duplicate"] {
            let (_context, session, spool) = fixture(SpillMode::ForceSpill, &[0; 4096]);
            // First row is semantically invalid; later store failure MUST win.
            let source = store(
                &session,
                spool.len,
                2,
                &[row(0, 0, 30, 0), row(1, 256, 30, 1), row(2, 256, 30, 2)],
            );
            let path = std::fs::read_dir(&*session.temp_directory().unwrap())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            if damage == "truncate" {
                file.set_len(128 + 3 * 64 - 1).unwrap();
            } else {
                let mut header = [0; 128];
                file.read_exact(&mut header).unwrap();
                let CandidateStore::Disk(table) = &source else {
                    panic!("disk required")
                };
                let meta = RecordMeta {
                    session: session.identity(),
                    generation: table.identity().generation,
                    file_id: header[64..80].try_into().unwrap(),
                    kind: 1,
                    order: 1,
                };
                let replacement = match damage {
                    "order" => row(0, 256, 30, 2),
                    "duplicate" => row(1, 256, 30, 2),
                    _ => row(2, 256, 30, 3),
                };
                let mut bytes = [0; 64];
                replacement.encode(meta, 2, &mut bytes).unwrap();
                if damage == "checksum" {
                    bytes[48] ^= 1;
                }
                file.seek(SeekFrom::Start(128 + 2 * 64)).unwrap();
                file.write_all(&bytes).unwrap();
            }
            let batch = CandidateBatch::from_external(source, spool.len, &session).unwrap();
            let error = err(validate_batch(batch, &spool, 2, &session));
            assert!(
                matches!(
                    error.kind(),
                    crate::error::ErrorKind::CorruptIndex | crate::error::ErrorKind::CorruptRecord
                ),
                "{damage}: {error}"
            );
            if damage == "digest" {
                assert_eq!(error.context(), Some("table differs from writer digest"));
            }
            assert_eq!(session.io_reserved(), 0);
        }
        for duplicate in [false, true] {
            let (_context, session, spool) = setup();
            let mut source = store(
                &session,
                spool.len,
                8,
                &[row(0, 0, 30, 0), row(1, 256, 30, 1), row(2, 256, 30, 2)],
            );
            source.replace_ram_row_for_test(
                2,
                if duplicate {
                    row(1, 256, 30, 2)
                } else {
                    row(0, 256, 30, 2)
                },
            );
            let batch = CandidateBatch::from_external(source, spool.len, &session).unwrap();
            assert_eq!(
                err(validate_batch(batch, &spool, 2, &session)).kind(),
                crate::error::ErrorKind::CorruptIndex
            );
        }
        // A valid-checksum, originally sealed duplicate bypasses S1's monotonic
        // (not unique) table check; V1 must still reject before semantics.
        let (_context, session, spool) = setup();
        use super::super::contracts::TableBuilder;
        let mut builder =
            super::super::storage::CandidateTableBuilder::new(&session, spool.len).unwrap();
        for r in [row(0, 0, 30, 0), row(1, 256, 30, 1), row(1, 256, 30, 2)] {
            builder.append(r).unwrap();
        }
        let batch = CandidateBatch::from_external(
            CandidateStore::Disk(builder.finish().unwrap()),
            spool.len,
            &session,
        )
        .unwrap();
        assert_eq!(
            err(validate_batch(batch, &spool, 2, &session)).context(),
            Some("candidate triple was not reduced")
        );
    }

    struct TestVerifier;
    impl super::super::contracts::SourceVerifier for TestVerifier {
        fn finish_verified(&mut self, _: &CompressionSession) -> Result<()> {
            Ok(())
        }
    }
    fn completion(s: &CompressionSession) -> EnumerationComplete {
        use super::super::contracts::{EnumerationRequirement, SourceBarrier, SourcePhase};
        let base = SourceBarrier::new(TestVerifier, s, SourcePhase::Base)
            .finish(s)
            .unwrap();
        EnumerationRequirement::new(s, false)
            .complete(base, None)
            .unwrap()
    }

    #[test]
    fn actual_store_identity_precedes_reads_even_with_current_completion_token() {
        use super::super::session::{FaultAction, SpillMode};
        for mode in [SpillMode::ForceRam, SpillMode::ForceSpill] {
            for route in 0..6 {
                let (_a, a, mut spool) = fixture(mode, &[0; 512]);
                let (_b, b, _) = fixture(mode, &[0; 512]);
                let source = store(&a, spool.len, 8, &[row(0, 128, 30, 0)]);
                let token = completion(&b);
                let used_a = a.io_used();
                let used_b = b.io_used();
                a.inject(Operation::Read, 0, FaultAction::Fail);
                b.inject(Operation::Read, 0, FaultAction::Fail);
                let error = match route {
                    0 => err(CandidateBatch::from_enumerated(
                        source, token, spool.len, &b,
                    )),
                    1 => err(CandidateBatch::from_external(source, spool.len, &b)),
                    2 => err(CandidateBatch::from_external(source, spool.len + 1, &a)),
                    3 => err(CandidateBatch::from_enumerated(
                        source,
                        completion(&a),
                        spool.len + 1,
                        &a,
                    )),
                    4 => {
                        let batch = CandidateBatch::from_external(source, spool.len, &a).unwrap();
                        err(validate_batch(batch, &spool, 2, &b))
                    }
                    _ => {
                        let batch = CandidateBatch::from_external(source, spool.len, &a).unwrap();
                        spool.len += 1;
                        err(validate_batch(batch, &spool, 2, &a))
                    }
                };
                assert_eq!(error.kind(), crate::error::ErrorKind::CorruptIndex);
                assert_eq!(a.io_used(), used_a);
                assert_eq!(b.io_used(), used_b);
            }
            let (_context, session, spool) = fixture(mode, &[0; 512]);
            let source = store(&session, spool.len, 8, &[row(0, 128, 30, 0)]);
            let batch =
                CandidateBatch::from_enumerated(source, completion(&session), spool.len, &session)
                    .unwrap();
            assert_eq!(
                validate_batch(batch, &spool, 2, &session)
                    .unwrap()
                    .into_schedule_input(&session)
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[test]
    fn semantic_matrix_all_permutations_matches_raw_reference() {
        use super::super::session::SpillMode;
        let cases = [
            [row(0, 128, 0, 0), row(0, 128, 25, 1), row(0, 256, 26, 2)],
            [row(0, 128, 0, 0), row(1, 1, 0, 1), row(2, u64::MAX, 1, 2)],
            [
                row(0, 128, 26, 0),
                row(1, 256, 30, 1),
                row(u64::MAX, 256, 30, 2),
            ],
            [
                row(0, 128, 30, 0),
                row(1, 256, 30, 1),
                row(2, u64::MAX, 1, 2),
            ],
            [
                row(0, u64::MAX, 1, 0),
                row(u64::MAX, 0, 30, 1),
                row(2, 256, 30, 2),
            ],
            [row(0, 128, 25, 0), row(0, 128, 0, 1), row(0, 512, 26, 2)],
        ];
        for rows in cases {
            for order in [
                [0, 1, 2],
                [0, 2, 1],
                [1, 0, 2],
                [1, 2, 0],
                [2, 0, 1],
                [2, 1, 0],
            ] {
                for (mode, chunk) in [
                    (SpillMode::ForceRam, 8),
                    (SpillMode::Adaptive, 1),
                    (SpillMode::Adaptive, 2),
                    (SpillMode::ForceSpill, 3),
                ] {
                    let (context, session, spool) = fixture(mode, &b"abcdefghijklmnop".repeat(32));
                    let permuted = order.map(|i| rows[i]);
                    let mut raw = crate::candidate_validation::collect_candidates(
                        permuted.map(|r| r.candidate),
                        &context.memory,
                    )
                    .unwrap();
                    let reference =
                        crate::candidate_validation::validate_candidates(&spool, &mut raw, 30);
                    let source = store(&session, spool.len, chunk, &permuted);
                    let result = Validator::new(&spool, 30).validate(source, &session);
                    match reference {
                        Err(expected) => {
                            let actual = err(result);
                            assert_eq!(
                                (actual.kind(), actual.context()),
                                (expected.kind(), expected.context())
                            );
                        }
                        Ok(()) => {
                            result.unwrap().into_schedule_input(&session).unwrap();
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn schedule_representatives_distinct_lengths_and_end_order_across_partitions() {
        use super::super::session::SpillMode;
        let rows = [
            row(16, 128, 40, 0),
            row(0, 128, 40, 9),
            row(0, 128, 40, 2),
            row(0, 128, 30, 8),
            row(0, 144, 30, 4),
            row(0, 64, 0, 0),
            row(0, 80, 25, 0),
            row(0, 112, 46, 3),
        ];
        let expected = [
            row(0, 112, 46, 3),
            row(0, 128, 30, 8),
            row(0, 128, 40, 2),
            row(0, 144, 30, 4),
        ];
        for (mode, chunk) in [
            (SpillMode::ForceRam, 16),
            (SpillMode::Adaptive, 1),
            (SpillMode::Adaptive, 2),
            (SpillMode::Adaptive, 3),
            (SpillMode::ForceSpill, 7),
        ] {
            let (_context, session, spool) = fixture(mode, &[0; 512]);
            let source = store(&session, spool.len, chunk, &rows);
            let scheduled = Validator::new(&spool, 26)
                .validate(source, &session)
                .unwrap()
                .into_schedule_input(&session)
                .unwrap();
            assert_eq!(scheduled.input_len(), spool.len);
            assert_eq!(scheduled.len(), expected.len() as u64);
            let mut cursor = scheduled.cursor().unwrap();
            for r in expected {
                assert_eq!(cursor.next().unwrap().unwrap().0, r);
            }
            assert!(cursor.next().unwrap().is_none());
        }
        for rows in [&[][..], &[row(0, 128, 0, 0), row(0, 128, 25, 1)][..]] {
            let (_context, session, spool) = setup();
            let source = store(&session, spool.len, 8, rows);
            let scheduled = Validator::new(&spool, 999)
                .validate(source, &session)
                .unwrap()
                .into_schedule_input(&session)
                .unwrap();
            assert_eq!(scheduled.len(), 0);
            assert!(scheduled.cursor().unwrap().next().unwrap().is_none());
        }
    }

    #[test]
    fn validated_schedule_random_cached_reads_span_pages_without_materialization() {
        use super::super::session::SpillMode;
        let (context, session, spool) = fixture(SpillMode::Adaptive, &[0; 8192]);
        let mut sink = ExternalCandidateSink::with_input_len(&session, spool.len, 7).unwrap();
        for i in (0..5200).rev() {
            sink.emit(row(0, 256 + i, 26, i)).unwrap();
            // Equal intervals from different sources fall in different triple
            // regions and interval-sort chunks; smaller source must still win.
            sink.emit(row(1, 256 + i, 26, 0)).unwrap();
        }
        let scheduled = Validator::new(&spool, 26)
            .validate(sink.finish().unwrap(), &session)
            .unwrap()
            .into_schedule_input(&session)
            .unwrap();
        assert_eq!(scheduled.len(), 5200);
        fn consumer(table: &impl RandomStore<EndRow>, index: u64) -> CandidateRow {
            table.read_at(index).unwrap().0
        }
        let mut cache = RecordPageCache::new(&context.memory, 2).unwrap();
        for i in [1023, 1024, 2047, 2048, 4095, 4096, 5199, 0, 1024] {
            let expected = row(0, 256 + i, 26, i);
            assert_eq!(consumer(&scheduled, i), expected);
            assert_eq!(scheduled.read_cached(i, &mut cache).unwrap().0, expected);
            let used = session.io_used();
            assert_eq!(scheduled.read_cached(i, &mut cache).unwrap().0, expected);
            assert_eq!(used, session.io_used());
        }
        assert_eq!(
            err(scheduled.read_at(5200)).kind(),
            crate::error::ErrorKind::CorruptIndex
        );
        assert_eq!(
            err(scheduled.read_cached(u64::MAX, &mut cache)).kind(),
            crate::error::ErrorKind::CorruptIndex
        );
        assert_eq!(scheduled.read_at(0).unwrap().0, row(0, 256, 26, 0));
        assert_eq!(
            scheduled.read_cached(0, &mut cache).unwrap().0,
            row(0, 256, 26, 0)
        );
    }

    #[test]
    fn full_semantic_pass_precedes_denied_interval_allocation() {
        for bad in [true, false] {
            let (context, session, spool) = setup();
            let source = store(
                &session,
                spool.len,
                8,
                &[
                    row(0, 256, 30, 0),
                    row(if bad { 1 } else { 16 }, 512, 30, 1),
                ],
            );
            // A real parent-budget reservation leaves enough for all three
            // validation buffers/cursor, but not the fixed interval sort chunk.
            let hold = context
                .memory
                .reserve(context.memory.limit() - context.memory.current() - 16 * 1024)
                .unwrap();
            let result = Validator::new(&spool, 26).validate(source, &session);
            if bad {
                assert_eq!(err(result).context(), Some("candidate bytes do not match"));
            } else {
                assert_eq!(
                    err(result.unwrap().into_schedule_input(&session)).kind(),
                    crate::error::ErrorKind::MemoryBudgetExceeded
                );
            }
            assert_eq!(session.io_reserved(), 0);
            drop(hold);
        }
    }

    #[test]
    fn sort_transition_failure_cleans_owned_files_and_preserves_other_readable_owner() {
        use super::super::contracts::TableBuilder;
        use super::super::session::{FaultAction, SpillMode};
        use super::super::storage::CandidateTableBuilder;
        // Fail first interval header, interval row, and first end-table header.
        for writes_before_failure in [0, 1, 4] {
            let (context, session, spool) = fixture(SpillMode::ForceSpill, &[0; 4096]);
            let mut other = CandidateTableBuilder::new(&session, spool.len).unwrap();
            other.append(row(0, 128, 26, 99)).unwrap();
            let other = other.finish().unwrap();
            let temp_before = context.temp.current();
            let handles_before = session.handle_usage().current;
            let source = store(
                &session,
                spool.len,
                2,
                &[row(0, 256, 30, 0), row(16, 512, 30, 1)],
            );
            let validated = Validator::new(&spool, 26)
                .validate(source, &session)
                .unwrap();
            session.inject(Operation::Write, writes_before_failure, FaultAction::Fail);
            assert_eq!(
                err(validated.into_schedule_input(&session)).kind(),
                crate::error::ErrorKind::TemporaryStorageFailure
            );
            session.retry_cleanup().unwrap();
            assert_eq!(context.temp.current(), temp_before);
            assert_eq!(session.handle_usage().current, handles_before);
            assert_eq!(session.file_usage().current, 1);
            assert_eq!(other.read_at(0).unwrap(), row(0, 128, 26, 99));
            assert_eq!(session.io_reserved(), 0);
            drop(other);
            session.retry_cleanup().unwrap();
            assert_eq!(session.file_usage().current, 0);
        }
    }

    struct ShortReader {
        bytes: Cursor<Vec<u8>>,
        actions: std::collections::VecDeque<std::io::Result<usize>>,
        calls: usize,
    }
    impl Read for ShortReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.calls += 1;
            let max = self.actions.pop_front().unwrap_or(Ok(buffer.len()))?;
            let n = max.min(buffer.len());
            self.bytes.read(&mut buffer[..n])
        }
    }
    impl Seek for ShortReader {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.bytes.seek(position)
        }
    }
    fn reader(
        actions: impl IntoIterator<Item = std::io::Result<usize>>,
        bytes: &[u8],
    ) -> ShortReader {
        ShortReader {
            bytes: Cursor::new(bytes.to_vec()),
            actions: actions.into_iter().collect(),
            calls: 0,
        }
    }

    #[test]
    fn accounted_exact_reader_retries_interrupts_and_charges_only_actual_prefix() {
        use super::super::session::{FaultAction, IoLimit};
        use std::io::ErrorKind;
        let (_context, session, _) = setup();
        let mut r = reader(
            [
                Err(ErrorKind::Interrupted.into()),
                Err(ErrorKind::Interrupted.into()),
                Ok(2),
                Err(ErrorKind::Interrupted.into()),
                Ok(1),
            ],
            b"abcdef",
        );
        let mut output = [0; 6];
        read_exact_accounted(
            &mut r,
            0,
            &mut output,
            &session,
            "candidate source is unavailable",
        )
        .unwrap();
        assert_eq!(&output, b"abcdef");
        assert_eq!(session.io_used(), 6);
        assert_eq!(session.io_reserved(), 0);
        assert_eq!(r.calls, 6);

        for (kind, bytes) in [
            (ErrorKind::UnexpectedEof, &b"ab"[..]),
            (ErrorKind::Other, &b"abcdef"[..]),
        ] {
            let (_context, session, _) = setup();
            let actions = if kind == ErrorKind::Other {
                vec![Ok(2), Err(kind.into())]
            } else {
                vec![Ok(2)]
            };
            let mut r = reader(actions, bytes);
            let error = err(read_exact_accounted(
                &mut r,
                0,
                &mut output,
                &session,
                "candidate destination is unavailable",
            ));
            let reference = Error::map_eof(kind.into(), "candidate destination is unavailable");
            assert_eq!(
                (error.kind(), error.context()),
                (reference.kind(), reference.context())
            );
            assert_eq!(session.io_used(), 2);
            assert_eq!(session.io_reserved(), 0);
        }

        let (_context, session, _) = setup();
        session.inject(Operation::Read, 1, FaultAction::Cancel);
        let mut r = reader([Ok(2), Err(ErrorKind::Interrupted.into())], b"abcdef");
        assert_eq!(
            err(read_exact_accounted(
                &mut r,
                0,
                &mut output,
                &session,
                "source"
            ))
            .kind(),
            crate::error::ErrorKind::TemporaryStorageFailure
        );
        assert_eq!(r.calls, 1);
        assert_eq!(session.io_used(), 2);
        assert_eq!(session.io_reserved(), 0);

        // Check cancellation on retries too, with no successful read at all.
        let (_context, session, _) = setup();
        session.inject(Operation::Read, 2, FaultAction::Cancel);
        let mut r = reader(
            [
                Err(ErrorKind::Interrupted.into()),
                Err(ErrorKind::Interrupted.into()),
            ],
            b"abcdef",
        );
        assert!(read_exact_accounted(&mut r, 0, &mut output, &session, "source").is_err());
        assert_eq!(r.calls, 2);
        assert_eq!(session.io_used(), 0);
        assert_eq!(session.io_reserved(), 0);

        let resources = ResourceConfig {
            memory: 16 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let context = ResourceContext::with_resources(&resources).unwrap();
        let session = CompressionSession::new(
            &context,
            &resources,
            OffloadPolicy {
                io_limit: IoLimit::Finite(5),
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        let mut r = reader([], b"abcdef");
        assert!(read_exact_accounted(&mut r, 0, &mut output, &session, "source").is_err());
        assert_eq!(r.calls, 0);
        assert_eq!(session.io_used(), 0);
        assert_eq!(session.io_reserved(), 0);
    }

    #[test]
    fn real_periodic_reads_cross_4k_wraps_and_partial_tail_match_reference() {
        use super::super::session::SpillMode;
        for distance in [17usize, 4095, 4096, 4097, 8193] {
            let period: Vec<u8> = (0..distance)
                .map(|i| ((i * 31 + i / 17) % 251) as u8)
                .collect();
            let len = distance * 2 + 4133;
            let original: Vec<u8> = period
                .iter()
                .copied()
                .cycle()
                .take(distance + len)
                .collect();
            for corrupt in [false, true] {
                let mut input = original.clone();
                if corrupt {
                    *input.last_mut().unwrap() ^= 1;
                }
                let (context, session, spool) = fixture(SpillMode::Adaptive, &input);
                let r = row(0, distance as u64, len as u64, 0);
                let mut raw =
                    crate::candidate_validation::collect_candidates([r.candidate], &context.memory)
                        .unwrap();
                let reference =
                    crate::candidate_validation::validate_candidates(&spool, &mut raw, 26);
                let source = store(&session, spool.len, 8, &[r]);
                let result = Validator::new(&spool, 26).validate(source, &session);
                if corrupt {
                    let expected = err(reference);
                    let actual = err(result);
                    assert_eq!(
                        (actual.kind(), actual.context()),
                        (expected.kind(), expected.context())
                    );
                } else {
                    reference.unwrap();
                    assert_eq!(
                        session.io_used(),
                        (len + if distance <= 4096 { distance } else { len }) as u128
                    );
                    result.unwrap().into_schedule_input(&session).unwrap();
                }
                assert_eq!(session.io_reserved(), 0);
            }
        }
    }
}
