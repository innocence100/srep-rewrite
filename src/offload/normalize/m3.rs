//! Approved m3-only authoritative two-pass validation. All certificate fields
//! and schedule-input construction remain inside the validation boundary.
use super::*;
use crate::codec::InputIdentity;
use crate::config::{CompressionConfig, Method};
use crate::match_finder::snapshot::ExactLce;
use crate::offload::contracts::{CandidateSink, EnumerationSummary};
use crate::offload::session::{ResourceLease, SessionId};
use crate::resource::Reservation;
use std::fs::File;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Binding([u8; 128]);
#[inline(always)]
fn put(b: &mut [u8], at: usize, n: u64) {
    b[at..at + 8].copy_from_slice(&n.to_le_bytes());
}
impl Binding {
    fn new(c: &CompressionConfig) -> Result<Self> {
        c.validate()?;
        if c.method != Method::M3FixedDigest {
            return Err(Error::invalid_config(
                "two-pass validation is internal m3 only",
            ));
        }
        let mut b = [0; 128];
        b[0] = c.method.wire_id();
        b[1] = c.layout.wire_id();
        b[2] = c.checksum.wire_id();
        b[3] = c.semantic_flags();
        put(&mut b, 8, c.block_size);
        put(&mut b, 16, c.min_match);
        put(&mut b, 24, c.seed_size.unwrap_or(0));
        put(&mut b, 32, c.target_chunk.unwrap_or(0));
        put(&mut b, 40, c.header_max_distance());
        put(&mut b, 48, c.effective_min_match()?);
        b[56] = u8::from(c.seed_size.is_some());
        b[57] = u8::from(c.target_chunk.is_some());
        b[58] = u8::from(c.rep_overlay.is_some());
        if let Some(o) = &c.rep_overlay {
            put(&mut b, 64, o.distance);
            put(&mut b, 72, o.min_match);
            put(
                &mut b,
                80,
                match c.max_distance {
                    Some(n) if n != 0 => n.min(o.distance),
                    _ => o.distance,
                },
            );
        }
        Ok(Self(b))
    }
}
struct Transcript {
    hash: blake3::Hasher,
    buffer: [u8; 6144],
    used: usize,
    count: u64,
    next: u64,
    phase: Option<(u8, u8, u64)>,
    ended: u8,
    overlay: bool,
}
impl Transcript {
    fn new(binding: Binding, input: InputIdentity, overlay: bool) -> Self {
        let mut hash = blake3::Hasher::new();
        hash.update(b"SREP-OFF-M3-ENUM-v1\0");
        hash.update(&1u16.to_le_bytes());
        hash.update(&binding.0);
        hash.update(&input.encode());
        Self {
            hash,
            buffer: [0; 6144],
            used: 0,
            count: 0,
            next: 0,
            phase: None,
            ended: 0,
            overlay,
        }
    }
    fn event(&mut self, b: &[u8; 96]) {
        if self.used == self.buffer.len() {
            self.hash.update(&self.buffer);
            self.used = 0;
        }
        self.buffer[self.used..self.used + 96].copy_from_slice(b);
        self.used += 96;
    }
    fn begin(&mut self, phase: u8, method: u8, start: u64) -> Result<()> {
        if self.phase.is_some()
            || phase != self.ended
            || start != self.next
            || method != if phase == 0 { 3 } else { 0 }
            || phase > u8::from(self.overlay)
        {
            return Err(Error::corrupt_index("m3 phase begin mismatch"));
        }
        let mut b = [0; 96];
        b[0] = 1;
        b[1] = phase;
        b[2] = method;
        put(&mut b, 8, start);
        self.event(&b);
        self.phase = Some((phase, method, start));
        Ok(())
    }
    fn row(&mut self, r: CandidateRow) -> Result<()> {
        let (phase, method, _) = self
            .phase
            .ok_or_else(|| Error::corrupt_index("m3 row outside phase"))?;
        if r.candidate.insertion_ordinal != self.next
            || r.provenance.phase != phase
            || r.provenance.method != method
        {
            return Err(Error::corrupt_index("m3 row ordinal/provenance mismatch"));
        }
        let mut b = [0; 96];
        b[0] = 2;
        b[1] = phase;
        b[2] = method;
        for (i, n) in [
            r.candidate.src,
            r.candidate.dst,
            r.candidate.len,
            r.candidate.insertion_ordinal,
        ]
        .into_iter()
        .enumerate()
        {
            put(&mut b, 8 + i * 8, n);
        }
        b[40] = r.provenance.method;
        b[41] = r.provenance.phase;
        self.event(&b);
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("m3 stream count overflow"))?;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("m3 stream ordinal overflow"))?;
        Ok(())
    }
    fn end(&mut self, p: u8, m: u8, start: u64, next: u64) -> Result<()> {
        if self.phase != Some((p, m, start)) || next != self.next {
            return Err(Error::corrupt_index("m3 phase end mismatch"));
        }
        let mut b = [0; 96];
        b[0] = 3;
        b[1] = p;
        b[2] = m;
        put(&mut b, 8, start);
        put(&mut b, 16, next);
        put(
            &mut b,
            24,
            next.checked_sub(start)
                .ok_or_else(|| Error::corrupt_index("m3 phase count"))?,
        );
        self.event(&b);
        self.phase = None;
        self.ended += 1;
        Ok(())
    }
    fn finish(&mut self, summary: &EnumerationSummary) -> Result<[u8; 32]> {
        if self.phase.is_some()
            || self.ended != 1 + u8::from(self.overlay)
            || self.next != summary.next_ordinal
            || self.count != summary.qualifying_count
        {
            return Err(Error::corrupt_index("m3 stream completion mismatch"));
        }
        let mut b = [0; 96];
        b[0] = 4;
        put(&mut b, 8, self.count);
        put(&mut b, 16, self.next);
        put(&mut b, 24, self.ended as u64);
        self.event(&b);
        self.hash.update(&self.buffer[..self.used]);
        Ok(*self.hash.finalize().as_bytes())
    }
}
#[derive(Clone, Copy)]
enum Semantic {
    DestinationOverflow,
    Interval,
    SourceOverflow,
    Minimum,
    Bytes,
}
impl Semantic {
    fn error(self) -> Error {
        Error::invalid_match(match self {
            Self::DestinationOverflow => "candidate destination overflows",
            Self::Interval => "candidate interval is invalid",
            Self::SourceOverflow => "candidate source overflows",
            Self::Minimum => "candidate is shorter than minimum match",
            Self::Bytes => "candidate bytes do not match",
        })
    }
}
fn structure(c: MatchCandidate, input: u64, min: u64) -> std::result::Result<bool, Semantic> {
    let end = c
        .dst
        .checked_add(c.len)
        .ok_or(Semantic::DestinationOverflow)?;
    if c.src >= c.dst || end > input || c.src >= input {
        return Err(Semantic::Interval);
    }
    c.src
        .checked_add(c.len.min(c.dst - c.src))
        .ok_or(Semantic::SourceOverflow)?;
    if c.len <= 25 {
        return Ok(false);
    }
    if c.len < min {
        return Err(Semantic::Minimum);
    }
    Ok(true)
}

enum View {
    Memory {
        bytes: BudgetedVec<u8>,
        lce: Option<ExactLce>,
    },
    File(File, ResourceLease),
}
struct Verifier<'a> {
    view: View,
    session: &'a CompressionSession,
}
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VerifierTestObservation {
    pub(crate) calls: u64,
    pub(crate) file_views: u64,
    pub(crate) budget_denials: u64,
    pub(crate) input_len: u64,
    pub(crate) available_before: u64,
    pub(crate) mandatory_bytes: u64,
    pub(crate) available_after: u64,
}
#[cfg(test)]
thread_local! { static VERIFIER_TEST_OBSERVATION: std::cell::Cell<VerifierTestObservation> = const { std::cell::Cell::new(VerifierTestObservation { calls:0, file_views:0, budget_denials:0, input_len:0, available_before:0, mandatory_bytes:0, available_after:0 }) }; }
#[cfg(test)]
pub(crate) fn test_verifier_observation_reset() {
    VERIFIER_TEST_OBSERVATION.with(|v| v.set(VerifierTestObservation::default()));
}
#[cfg(test)]
pub(crate) fn test_verifier_observation() -> VerifierTestObservation {
    VERIFIER_TEST_OBSERVATION.with(std::cell::Cell::get)
}
impl<'a> Verifier<'a> {
    fn mandatory_generation_headroom_bytes() -> Result<u64> {
        let index = std::mem::size_of::<crate::candidate_index::CodecPagedIndex>()
            .checked_add(8192)
            .ok_or_else(|| Error::memory_limit("source index metadata size overflows"))?;
        let cache = std::mem::size_of::<crate::offload::storage::SourcePageCache<'_>>();
        let reader = 2usize
            .checked_mul(64 * 1024)
            .ok_or_else(|| Error::memory_limit("source reader cache size overflows"))?;
        let total = index
            .checked_add(cache)
            .and_then(|n| n.checked_add(reader))
            .ok_or_else(|| Error::memory_limit("mandatory source headroom overflows"))?;
        u64::try_from(total).map_err(|_| Error::memory_limit("source headroom exceeds u64"))
    }
    fn new(spool: &InputSpool, session: &'a CompressionSession) -> Result<Self> {
        spool.check_owner(session)?;
        #[cfg(test)]
        VERIFIER_TEST_OBSERVATION.with(|v| {
            let mut o = v.get();
            o.calls += 1;
            o.input_len = spool.len;
            v.set(o);
        });
        #[cfg(test)]
        if tests::FORCE_VERIFIER_FILE.with(std::cell::Cell::get) {
            return Self::file_fallback(spool, session);
        }
        let denied = cfg!(test) && tests::denied();
        if spool.len <= 16 * 1024 * 1024 && !denied {
            // The optional snapshot must not consume the last bytes needed by
            // the mandatory source index metadata and the 128 KiB reader
            // cache used by the subsequent base/overlay enumeration.  This is
            // a concrete admission proof, not a guessed LCE margin: the
            // helper sums the exact `size_of` metadata reservations and the
            // reader's two fixed 64 KiB pages.  The reservation is released
            // only after the optional view is complete; the private pipeline is
            // serialized, so the next allocations are precisely those audited
            // mandatory owners and cannot be interleaved with another session.
            let mandatory_bytes = Self::mandatory_generation_headroom_bytes()?;
            #[cfg(test)]
            VERIFIER_TEST_OBSERVATION.with(|v| {
                let mut o = v.get();
                // Capture the admission inequality at the production branch,
                // before the mandatory reservation is attempted.
                o.available_before = session
                    .memory()
                    .limit()
                    .saturating_sub(session.memory().current());
                o.mandatory_bytes = mandatory_bytes;
                o.available_after = o.available_before.saturating_sub(mandatory_bytes);
                v.set(o);
            });
            let mandatory = match session.memory().reserve(mandatory_bytes) {
                Ok(value) => value,
                Err(error) if error.kind() == crate::ErrorKind::MemoryBudgetExceeded => {
                    #[cfg(test)]
                    VERIFIER_TEST_OBSERVATION.with(|v| {
                        let mut o = v.get();
                        o.budget_denials += 1;
                        v.set(o);
                    });
                    return Self::file_fallback(spool, session);
                }
                Err(error) => return Err(error),
            };
            let mut bytes = match BudgetedVec::with_capacity(spool.len as usize, session.memory()) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == crate::ErrorKind::MemoryBudgetExceeded => {
                    #[cfg(test)]
                    VERIFIER_TEST_OBSERVATION.with(|v| {
                        let mut o = v.get();
                        o.budget_denials += 1;
                        v.set(o);
                    });
                    drop(mandatory);
                    return Self::file_fallback(spool, session);
                }
                Err(error) => return Err(error),
            };
            bytes.resize(spool.len as usize, 0)?;
            let _h = session.acquire_handles(1)?;
            session.checkpoint(Operation::Open)?;
            let mut file = spool._temp.reopen().map_err(Error::temp_storage)?;
            let mut permit = session.reserve_io(spool.len as u128)?;
            let mut at = 0;
            while at < bytes.len() {
                session.checkpoint(Operation::Read)?;
                let end = (at + 65536).min(bytes.len());
                let n = match file.read(&mut bytes.as_mut_slice()[at..end]) {
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    r => r.map_err(Error::temp_storage)?,
                };
                if n == 0 {
                    return Err(Error::temp_storage(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    )));
                }
                permit.consume(n as u64)?;
                at += n;
            }
            #[cfg(test)]
            if tests::MUTATE_VIEW.with(std::cell::Cell::get) && !bytes.is_empty() {
                bytes[0] ^= 1;
            }
            spool.input_identity().check_bytes(bytes.as_slice())?;
            let lce = match ExactLce::try_new(bytes.as_slice(), &session.finder_context()) {
                Ok(l) => Some(l),
                Err(e) if e.kind() == crate::ErrorKind::MemoryBudgetExceeded => None,
                Err(e) => return Err(e),
            };
            drop(mandatory);
            return Ok(Self {
                view: View::Memory { bytes, lce },
                session,
            });
        }
        Self::file_fallback(spool, session)
    }
    fn file_fallback(spool: &InputSpool, session: &'a CompressionSession) -> Result<Self> {
        #[cfg(test)]
        tests::LAST_VERIFIER_FILE.with(|v| v.set(true));
        #[cfg(test)]
        tests::VERIFIER_FILE_COUNT.with(|v| v.set(v.get().saturating_add(1)));
        #[cfg(test)]
        VERIFIER_TEST_OBSERVATION.with(|v| {
            let mut o = v.get();
            o.file_views += 1;
            v.set(o);
        });
        spool.verify_identity(session)?;
        let h = session.acquire_handles(1)?;
        session.checkpoint(Operation::Open)?;
        let file = spool._temp.reopen().map_err(Error::temp_storage)?;
        Ok(Self {
            view: View::File(file, h),
            session,
        })
    }
    fn equal(&mut self, c: MatchCandidate) -> Result<bool> {
        match &mut self.view {
            View::Memory { bytes, lce } => {
                let eq = |a: u64, b: u64, n: u64| -> bool {
                    if let Some(l) = lce.as_ref() {
                        l.query(a as usize, b as usize, n as usize) == n as usize
                    } else {
                        bytes.as_slice()[a as usize..(a + n) as usize]
                            == bytes.as_slice()[b as usize..(b + n) as usize]
                    }
                };
                Ok(periodic_equal(c, eq))
            }
            View::File(file, _handle) => {
                let mut expected = [0; VALIDATION_BUFFER];
                let mut actual = [0; VALIDATION_BUFFER];
                let mut period = [0; VALIDATION_BUFFER];
                match validate_row(
                    c,
                    c.dst + c.len,
                    26,
                    file,
                    self.session,
                    &mut expected,
                    &mut actual,
                    &mut period,
                ) {
                    Ok(()) => Ok(true),
                    Err(e) if e.kind() == crate::ErrorKind::InvalidMatch => Ok(false),
                    Err(e) => Err(e),
                }
            }
        }
    }
}
fn periodic_equal(c: MatchCandidate, eq: impl Fn(u64, u64, u64) -> bool) -> bool {
    let d = c.dst - c.src;
    eq(c.src, c.dst, c.len.min(d)) && (c.len <= d || eq(c.dst, c.dst + d, c.len - d))
}

pub(in crate::offload) trait EpochSink: CandidateSink {
    fn begin(&mut self, p: u8, m: u8, s: u64) -> Result<()>;
    fn end(&mut self, p: u8, m: u8, s: u64, n: u64) -> Result<()>;
}
pub(in crate::offload) struct Pass1<'a> {
    spool: &'a InputSpool,
    session: &'a CompressionSession,
    binding: Binding,
    epoch: u64,
    min: u64,
    transcript: Transcript,
    pending: Option<(MatchCandidate, Semantic)>,
    verifier: Verifier<'a>,
    _control: Reservation,
    failed: bool,
}
pub(in crate::offload) struct Certificate {
    session: SessionId,
    input: InputIdentity,
    binding: Binding,
    epoch: u64,
    digest: [u8; 32],
    count: u64,
}
impl<'a> Pass1<'a> {
    pub(in crate::offload) fn new(
        spool: &'a InputSpool,
        c: &CompressionConfig,
        s: &'a CompressionSession,
        epoch: u64,
    ) -> Result<Self> {
        let binding = Binding::new(c)?;
        let control = s.memory().reserve(32768)?;
        let verifier = Verifier::new(spool, s)?;
        Ok(Self {
            spool,
            session: s,
            binding,
            epoch,
            min: c.effective_min_match()?,
            transcript: Transcript::new(binding, spool.input_identity(), c.rep_overlay.is_some()),
            pending: None,
            verifier,
            _control: control,
            failed: false,
        })
    }
    pub(in crate::offload) fn finish(mut self, summary: EnumerationSummary) -> Result<Certificate> {
        if self.failed {
            return Err(Error::temp_storage_context("failed m3 pass1"));
        }
        summary.source_integrity.check_session(self.session)?;
        let count = self.transcript.count;
        let digest = self.transcript.finish(&summary)?;
        self.spool.verify_identity(self.session)?;
        if let Some((_, error)) = self.pending {
            return Err(error.error());
        }
        self.session.profile(
            "m3_pass1_validated",
            format_args!("E={count} epoch={}", self.epoch),
        );
        Ok(Certificate {
            session: self.session.identity(),
            input: self.spool.input_identity(),
            binding: self.binding,
            epoch: self.epoch,
            digest,
            count,
        })
    }
}
impl CandidateSink for Pass1<'_> {
    fn emit(&mut self, row: CandidateRow) -> Result<()> {
        if self.failed {
            return Err(Error::temp_storage_context("failed m3 pass1"));
        }
        let result = self.emit_inner(row);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
impl Pass1<'_> {
    fn emit_inner(&mut self, row: CandidateRow) -> Result<()> {
        self.session.check_cancelled()?;
        self.transcript.row(row)?;
        let c = row.candidate;
        let invalid = match structure(c, self.spool.len, self.min) {
            Err(e) => Some(e),
            Ok(false) => None,
            Ok(true) => {
                if self.verifier.equal(c)? {
                    None
                } else {
                    Some(Semantic::Bytes)
                }
            }
        };
        if let Some(e) = invalid
            && self
                .pending
                .is_none_or(|(p, _)| canonical_key(c) < canonical_key(p))
        {
            self.pending = Some((c, e));
        }
        Ok(())
    }
}
impl EpochSink for Pass1<'_> {
    fn begin(&mut self, p: u8, m: u8, s: u64) -> Result<()> {
        if self.failed {
            return Err(Error::temp_storage_context("failed m3 pass1"));
        }
        let r = self.transcript.begin(p, m, s);
        if r.is_err() {
            self.failed = true;
        }
        r
    }
    fn end(&mut self, p: u8, m: u8, s: u64, n: u64) -> Result<()> {
        if self.failed {
            return Err(Error::temp_storage_context("failed m3 pass1"));
        }
        let r = self.transcript.end(p, m, s, n);
        if r.is_err() {
            self.failed = true;
        }
        r
    }
}
pub(in crate::offload) struct Pass2<'a> {
    spool: &'a InputSpool,
    session: &'a CompressionSession,
    certificate: Certificate,
    transcript: Transcript,
    sort: ExternalSorter<'a, IntervalRow>,
    representatives: BudgetedVec<Option<CandidateRow>>,
    pending: Option<CandidateRow>,
    _control: Reservation,
    failed: bool,
}
impl<'a> Pass2<'a> {
    pub(in crate::offload) fn new(
        cert: Certificate,
        spool: &'a InputSpool,
        c: &CompressionConfig,
        s: &'a CompressionSession,
        epoch: u64,
    ) -> Result<Self> {
        let binding = Binding::new(c)?;
        if cert.session != s.identity()
            || cert.input != spool.input_identity()
            || cert.binding != binding
            || cert.epoch != epoch
        {
            return Err(Error::corrupt_index("m3 certificate binding mismatch"));
        }
        spool.verify_identity(s)?;
        let control = s.memory().reserve(16384)?;
        #[allow(unused_mut)]
        let mut chunk_size = (s.memory().limit() / 512).clamp(1, 65536) as usize;
        #[cfg(test)]
        if let Some(override_size) = crate::offload::sort::test_chunk_override() {
            chunk_size = override_size;
        }
        let chunk = BudgetedVec::with_capacity(chunk_size, s.memory())?;
        let slots = (s.memory().limit() / 8 / std::mem::size_of::<Option<CandidateRow>>() as u64)
            .clamp(4, 262144) as usize;
        let slots = slots.next_power_of_two();
        let mut representatives = BudgetedVec::with_capacity(slots, s.memory())?;
        representatives.resize(slots, None)?;
        Ok(Self {
            transcript: Transcript::new(binding, spool.input_identity(), c.rep_overlay.is_some()),
            certificate: cert,
            spool,
            session: s,
            sort: ExternalSorter::new(s, chunk, spool.len, same_destination_interval)?,
            representatives,
            pending: None,
            _control: control,
            failed: false,
        })
    }
    pub(in crate::offload) fn finish(
        mut self,
        summary: EnumerationSummary,
    ) -> Result<ScheduledCandidates> {
        if self.failed {
            return Err(Error::temp_storage_context("failed m3 pass2"));
        }
        summary.source_integrity.check_session(self.session)?;
        let count = self.transcript.count;
        let digest = self.transcript.finish(&summary)?;
        if count != self.certificate.count || digest != self.certificate.digest {
            return Err(Error::corrupt_index("m3 replay stream mismatch"));
        }
        self.spool.verify_identity(self.session)?;
        if let Some(row) = self.pending.take() {
            self.retain(row)?;
        }
        for row in self.representatives.iter().flatten().copied() {
            self.sort.push(IntervalRow(row))?;
        }
        drop(self.representatives);
        let intervals = self.sort.finish()?;
        self.session.profile(
            "m3_pass2_intervals",
            format_args!("E={count} D={}", intervals.len()),
        );
        let mut ends = ExternalSorter::new(
            self.session,
            fixed_sort_chunk::<EndRow>(self.session)?,
            self.spool.len,
            no_reduction,
        )?;
        let mut cursor = intervals.cursor()?;
        while let Some(row) = cursor.next()? {
            ends.push(EndRow(row.0))?;
        }
        let source = ends.finish()?;
        let count = source.len();
        Ok(ScheduledCandidates {
            source,
            count,
            input_len: self.spool.len,
        })
    }
}
impl CandidateSink for Pass2<'_> {
    fn emit(&mut self, r: CandidateRow) -> Result<()> {
        if self.failed {
            return Err(Error::temp_storage_context("failed m3 pass2"));
        }
        let result = self.emit_inner(r);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
impl Pass2<'_> {
    fn emit_inner(&mut self, r: CandidateRow) -> Result<()> {
        self.session.check_cancelled()?;
        self.transcript.row(r)?;
        if r.candidate.len > 25 {
            if let Some(old) = self.pending {
                if (old.candidate.dst, old.candidate.len) == (r.candidate.dst, r.candidate.len) {
                    #[cfg(test)]
                    tests::CACHE_OBSERVATION.with(|v| {
                        let mut o = v.get();
                        if r.candidate.src < old.candidate.src {
                            // A later smaller source replaces the pending
                            // representative.
                            o.smaller_source += 1;
                        } else if r.candidate.src == old.candidate.src {
                            // An equal-source duplicate retains the earlier
                            // ordinal; the later ordinal never displaces it.
                            o.ordinal_kept += 1;
                        }
                        v.set(o);
                    });
                    if (r.candidate.src, r.candidate.insertion_ordinal)
                        < (old.candidate.src, old.candidate.insertion_ordinal)
                    {
                        self.pending = Some(r);
                    }
                    return Ok(());
                }
                self.retain(old)?;
            }
            self.pending = Some(r);
        }
        Ok(())
    }
}
impl Pass2<'_> {
    fn retain(&mut self, r: CandidateRow) -> Result<()> {
        let mut hash =
            r.candidate.dst.wrapping_mul(0x9e3779b97f4a7c15) ^ r.candidate.len.rotate_left(27);
        hash ^= hash >> 31;
        hash = hash.wrapping_mul(0xbf58476d1ce4e5b9);
        hash ^= hash >> 29;
        let start = (hash as usize & (self.representatives.len() / 4 - 1)) * 4;
        let mut empty = None;
        for slot in start..start + 4 {
            if let Some(old) = self.representatives[slot] {
                if (old.candidate.dst, old.candidate.len) == (r.candidate.dst, r.candidate.len) {
                    if (r.candidate.src, r.candidate.insertion_ordinal)
                        < (old.candidate.src, old.candidate.insertion_ordinal)
                    {
                        self.representatives[slot] = Some(r);
                    }
                    return Ok(());
                }
            } else {
                empty = Some(slot);
            }
        }
        let slot = empty.unwrap_or(start);
        if let Some(evicted) = self.representatives[slot] {
            #[cfg(test)]
            tests::CACHE_OBSERVATION.with(|v| {
                let mut o = v.get();
                o.evictions += 1;
                o.last_evicted_dst = evicted.candidate.dst;
                o.last_evicted_len = evicted.candidate.len;
                o.evicted_dst = evicted.candidate.dst;
                o.evicted_len = evicted.candidate.len;
                // Record the full evicted key so the regression test can
                // reintroduce the ACTUAL evicted row and observe its return.
                o.observed_keys[o.observed_len.min(63)] = Some((
                    evicted.candidate.dst,
                    evicted.candidate.len,
                    evicted.candidate.src,
                    evicted.candidate.insertion_ordinal,
                ));
                o.observed_len = o.observed_len.saturating_add(1);
                v.set(o);
            });
            self.sort.push(IntervalRow(evicted))?;
        }
        self.representatives[slot] = Some(r);
        // A reintroduced row whose key matches a previously evicted key
        // proves the evicted data path returned to the resident set.
        #[cfg(test)]
        tests::CACHE_OBSERVATION.with(|v| {
            let mut o = v.get();
            if o.observed_keys[..o.observed_len.min(64)]
                .iter()
                .flatten()
                .any(|(dst, len, src, _)| {
                    *dst == r.candidate.dst && *len == r.candidate.len && *src == r.candidate.src
                })
            {
                o.reintroduced += 1;
            }
            v.set(o);
        });
        Ok(())
    }
    /// Test hook: insertion probe used by the regression test to force a run
    /// boundary and observe a genuine eviction event.
    #[cfg(test)]
    pub(in crate::offload) fn test_force_run_boundary(&mut self) -> Result<()> {
        self.sort.push_pending_as_run()
    }
}
impl EpochSink for Pass2<'_> {
    fn begin(&mut self, p: u8, m: u8, s: u64) -> Result<()> {
        if self.failed {
            return Err(Error::temp_storage_context("failed m3 pass2"));
        }
        let r = self.transcript.begin(p, m, s);
        if r.is_err() {
            self.failed = true;
        }
        r
    }
    fn end(&mut self, p: u8, m: u8, s: u64, n: u64) -> Result<()> {
        if self.failed {
            return Err(Error::temp_storage_context("failed m3 pass2"));
        }
        let r = self.transcript.end(p, m, s, n);
        if r.is_err() {
            self.failed = true;
        }
        r
    }
}

// Test denial is scoped/thread-local, not a process-wide policy knob.
mod tests {
    pub(super) fn denied() -> bool {
        #[cfg(test)]
        {
            DENY.with(std::cell::Cell::get)
        }
        #[cfg(not(test))]
        {
            false
        }
    }
    #[cfg(test)]
    thread_local! {static DENY:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
    #[cfg(test)]
    thread_local! {pub(super) static MUTATE_VIEW:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
    #[cfg(test)]
    thread_local! {pub(super) static LAST_VERIFIER_FILE:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
    #[cfg(test)]
    thread_local! {pub(super) static FORCE_VERIFIER_FILE:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
    #[cfg(test)]
    thread_local! {pub(super) static VERIFIER_FILE_COUNT:std::cell::Cell<u64>=const{std::cell::Cell::new(0)};}
    #[cfg(test)]
    #[allow(dead_code)]
    #[derive(Clone, Copy, Debug)]
    pub(super) struct CacheObservation {
        pub(super) evictions: u64,
        pub(super) reintroduced: u64,
        pub(super) smaller_source: u64,
        pub(super) ordinal_kept: u64,
        pub(super) evicted_dst: u64,
        pub(super) evicted_len: u64,
        pub(super) last_evicted_dst: u64,
        pub(super) last_evicted_len: u64,
        /// Full keys of evicted rows (dst, len, src, ordinal), oldest first.
        /// The regression test reintroduces one of these ACTUAL evicted keys.
        pub(super) observed_keys: [Option<(u64, u64, u64, u64)>; 64],
        pub(super) observed_len: usize,
    }
    #[cfg(test)]
    impl Default for CacheObservation {
        fn default() -> Self {
            Self {
                evictions: 0,
                reintroduced: 0,
                smaller_source: 0,
                ordinal_kept: 0,
                evicted_dst: 0,
                evicted_len: 0,
                last_evicted_dst: 0,
                last_evicted_len: 0,
                observed_keys: [None; 64],
                observed_len: 0,
            }
        }
    }
    #[cfg(test)]
    thread_local! {pub(super) static CACHE_OBSERVATION:std::cell::Cell<CacheObservation>=const{std::cell::Cell::new(CacheObservation {evictions:0,reintroduced:0,smaller_source:0,ordinal_kept:0,evicted_dst:0,evicted_len:0,last_evicted_dst:0,last_evicted_len:0,observed_keys:[None;64],observed_len:0})};}
    #[cfg(test)]
    mod regression {
        use super::super::*;
        use crate::offload::contracts::CodecSourceIndexFactory;
        use crate::offload::contracts::{
            EnumerationRequirement, Provenance, SourceBarrier, SourcePhase, SourceVerifier,
        };
        use crate::offload::session::{FaultAction, OffloadPolicy};
        use crate::resource::ResourceContext;
        struct Scope(&'static std::thread::LocalKey<std::cell::Cell<bool>>);
        impl Scope {
            fn on(key: &'static std::thread::LocalKey<std::cell::Cell<bool>>) -> Self {
                key.with(|v| v.set(true));
                Self(key)
            }
        }
        impl Drop for Scope {
            fn drop(&mut self) {
                self.0.with(|v| v.set(false));
            }
        }
        fn setup(
            data: &[u8],
        ) -> (
            ResourceContext,
            CompressionSession,
            InputSpool,
            CompressionConfig,
        ) {
            let mut cfg = CompressionConfig::for_method(Method::M3FixedDigest);
            cfg.min_match = 26;
            cfg.seed_size = Some(8);
            cfg.resources.memory = 8 * 1024 * 1024;
            let ctx = ResourceContext::with_resources(&cfg.resources).unwrap();
            let spool = crate::codec::spool_input(data, &cfg.resources, &ctx).unwrap();
            let s =
                CompressionSession::new(&ctx, &cfg.resources, OffloadPolicy::default()).unwrap();
            (ctx, s, spool, cfg)
        }
        struct EmptyVerifier;
        impl SourceVerifier for EmptyVerifier {
            fn finish_verified(&mut self, _: &CompressionSession) -> Result<()> {
                Ok(())
            }
        }
        fn summary(s: &CompressionSession, n: u64) -> EnumerationSummary {
            let token = SourceBarrier::new(EmptyVerifier, s, SourcePhase::Base)
                .finish(s)
                .unwrap();
            EnumerationSummary {
                next_ordinal: n,
                qualifying_count: n,
                source_integrity: EnumerationRequirement::new(s, false)
                    .complete(token, None)
                    .unwrap(),
            }
        }
        fn row(src: u64, dst: u64, len: u64, ordinal: u64) -> CandidateRow {
            CandidateRow {
                candidate: MatchCandidate {
                    src,
                    dst,
                    len,
                    insertion_ordinal: ordinal,
                },
                provenance: Provenance::new(3, 0).unwrap(),
            }
        }
        fn certificate<'a>(
            spool: &'a InputSpool,
            c: &CompressionConfig,
            s: &'a CompressionSession,
        ) -> Certificate {
            let mut p = Pass1::new(spool, c, s, 3).unwrap();
            p.begin(0, 3, 0).unwrap();
            p.emit(row(0, 32, 32, 0)).unwrap();
            p.end(0, 3, 0, 1).unwrap();
            p.finish(summary(s, 1)).unwrap()
        }
        #[test]
        fn exhaustive_forward_lce_matches_independent_periodic_bytes() {
            for n in 2..=9 {
                for mask in 0..1usize << n {
                    let bytes = (0..n).map(|i| ((mask >> i) & 1) as u8).collect::<Vec<_>>();
                    let ctx = ResourceContext::from_limits(1024 * 1024, 1024 * 1024);
                    let l = ExactLce::try_new(&bytes, &ctx).unwrap();
                    for src in 0..n {
                        for dst in src + 1..n {
                            for len in 0..=n - dst {
                                let c = row(src as u64, dst as u64, len as u64, 0).candidate;
                                let expected = (0..len)
                                    .all(|k| bytes[dst + k] == bytes[src + k % (dst - src)]);
                                let actual = periodic_equal(c, |a, b, n| {
                                    l.query(a as usize, b as usize, n as usize) == n as usize
                                });
                                assert_eq!(actual, expected, "{bytes:?} {c:?}");
                            }
                        }
                    }
                }
            }
        }
        #[test]
        fn semantic_witness_is_canonical_and_blocks_sort_allocation() {
            let (_, s, spool, c) = setup(&[0; 96]);
            let sets = [
                [row(1, 50, 10, 0), row(0, 32, 27, 1), row(2, 70, 50, 2)],
                [
                    row(2, 70, 50, 0),
                    row(1, 50, 26, 1),
                    row(0, 32, u64::MAX, 2),
                ],
            ];
            for rows in sets {
                for reverse in [false, true] {
                    let mut rows = rows;
                    if reverse {
                        rows.reverse();
                    }
                    for (n, r) in rows.iter_mut().enumerate() {
                        r.candidate.insertion_ordinal = n as u64;
                    }
                    let mut p = Pass1::new(&spool, &c, &s, 1).unwrap();
                    p.begin(0, 3, 0).unwrap();
                    for r in rows {
                        p.emit(r).unwrap();
                    }
                    p.end(0, 3, 0, 3).unwrap();
                    let mut raw = crate::candidate_validation::collect_candidates(
                        rows.map(|r| r.candidate),
                        s.memory(),
                    )
                    .unwrap();
                    let expected = crate::candidate_validation::validate_candidates(
                        &spool,
                        &mut raw,
                        c.min_match,
                    );
                    let got = p.finish(summary(&s, 3));
                    match expected {
                        Ok(()) => assert!(got.is_ok()),
                        Err(e) => {
                            let got = match got {
                                Err(e) => e,
                                Ok(_) => panic!("semantic error hidden"),
                            };
                            assert_eq!(got.kind(), e.kind());
                            assert_eq!(got.context(), e.context());
                        }
                    }
                }
            }
            let mut p = Pass1::new(&spool, &c, &s, 1).unwrap();
            p.begin(0, 3, 0).unwrap();
            p.emit(row(0, 32, u64::MAX, 0)).unwrap();
            p.end(0, 3, 0, 1).unwrap();
            let before = s.file_usage().current;
            s.inject(Operation::Create, 0, FaultAction::Fail);
            assert!(p.finish(summary(&s, 1)).is_err());
            assert_eq!(s.file_usage().current, before);
        }
        #[test]
        fn loaded_snapshot_identity_and_final_input_barrier_precede_semantics() {
            let (_, s, spool, c) = setup(&[0; 96]);
            {
                let _scope = Scope::on(&super::MUTATE_VIEW);
                let error = match Pass1::new(&spool, &c, &s, 1) {
                    Err(e) => e,
                    Ok(_) => panic!("unauthenticated snapshot"),
                };
                assert_eq!(error.kind(), crate::ErrorKind::ChecksumMismatch);
            }
            let mut p = Pass1::new(&spool, &c, &s, 1).unwrap();
            p.begin(0, 3, 0).unwrap();
            p.emit(row(0, 32, u64::MAX, 0)).unwrap();
            p.end(0, 3, 0, 1).unwrap();
            let mut f = spool._temp.reopen().unwrap();
            std::io::Write::write_all(&mut f, &[9]).unwrap();
            let e = match p.finish(summary(&s, 1)) {
                Err(e) => e,
                Ok(_) => panic!("input mutation accepted"),
            };
            assert_eq!(e.kind(), crate::ErrorKind::ChecksumMismatch);
            assert!(Pass1::new(&spool, &c, &s, 1).is_err());
        }
        #[test]
        fn certificate_rejects_changed_input_session_min_config_epoch() {
            for mode in 0..6 {
                let (_, s, spool, c) = setup(&[0; 96]);
                let cert = certificate(&spool, &c, &s);
                let (_, other, other_spool, _) = setup(&[0; 97]);
                let mut changed = c.clone();
                match mode {
                    0 => changed.min_match = 27,
                    1 => changed.block_size *= 2,
                    2 => changed.layout = crate::config::Layout::Io,
                    3 => changed.max_distance = Some(17),
                    _ => {}
                }
                let result = Pass2::new(
                    cert,
                    if mode == 4 { &other_spool } else { &spool },
                    &changed,
                    if mode == 5 { &other } else { &s },
                    3,
                );
                assert!(result.is_err());
            }
            let (_, s, spool, c) = setup(&[0; 96]);
            let cert = certificate(&spool, &c, &s);
            assert!(Pass2::new(cert, &spool, &c, &s, 4).is_err());
        }
        #[test]
        fn stream_rows_ordinals_provenance_and_phase_mismatch_rejected() {
            for mode in 0..5 {
                let (_, s, spool, c) = setup(&[0; 96]);
                let cert = certificate(&spool, &c, &s);
                let mut p = Pass2::new(cert, &spool, &c, &s, 3).unwrap();
                p.begin(0, 3, 0).unwrap();
                let r = match mode {
                    0 => row(1, 32, 32, 0),
                    1 => row(0, 32, 32, 1),
                    2 => CandidateRow {
                        provenance: Provenance::new(0, 1).unwrap(),
                        ..row(0, 32, 32, 0)
                    },
                    _ => row(0, 32, 32, 0),
                };
                if mode == 1 || mode == 2 {
                    assert!(p.emit(r).is_err());
                    continue;
                }
                p.emit(r).unwrap();
                if mode == 3 {
                    assert!(p.end(1, 0, 0, 1).is_err());
                    continue;
                }
                if mode == 4 {
                    p.emit(row(0, 64, 26, 1)).unwrap();
                    p.end(0, 3, 0, 2).unwrap();
                    assert!(p.finish(summary(&s, 2)).is_err());
                } else {
                    p.end(0, 3, 0, 1).unwrap();
                    assert!(p.finish(summary(&s, 1)).is_err());
                }
            }
        }
        #[test]
        fn fallback_byte_validation_matches_lce_and_physical_errors_win() {
            let (_, s, spool, c) = setup(&[0; 96]);
            let accelerated = certificate(&spool, &c, &s);
            let _scope = Scope::on(&super::DENY);
            let fallback = certificate(&spool, &c, &s);
            assert_eq!(accelerated.digest, fallback.digest);
            let mut p = Pass1::new(&spool, &c, &s, 1).unwrap();
            p.begin(0, 3, 0).unwrap();
            p.emit(row(0, 32, u64::MAX, 0)).unwrap();
            s.inject(Operation::Read, 0, FaultAction::Fail);
            let e = p.emit(row(0, 32, 32, 1)).unwrap_err();
            assert_eq!(e.kind(), crate::ErrorKind::TemporaryStorageFailure);
        }

        #[test]
        fn shared_budget_pressure_admits_snapshot_only_with_mandatory_headroom() {
            fn run(overlay: bool) {
                let mut data = (0..4 * 1024 * 1024usize)
                    .map(|n| ((n.wrapping_mul(131) ^ (n / 17) ^ (n / 4093)) & 0xff) as u8)
                    .collect::<Vec<_>>();
                let planted = data[4096..20_480].to_vec();
                data[2 * 1024 * 1024..2 * 1024 * 1024 + planted.len()].copy_from_slice(&planted);
                let planted = data[24_576..40_960].to_vec();
                data[3 * 1024 * 1024..3 * 1024 * 1024 + planted.len()].copy_from_slice(&planted);
                let planted = data[45_056..61_440].to_vec();
                data[1024 * 1024..1024 * 1024 + planted.len()].copy_from_slice(&planted);
                let planted = data[65_536..81_920].to_vec();
                data[3584 * 1024..3584 * 1024 + planted.len()].copy_from_slice(&planted);

                let mut reference_config = CompressionConfig::for_method(Method::M3FixedDigest);
                reference_config.min_match = 4096;
                reference_config.seed_size = Some(4096);
                reference_config.resources.memory = 64 * 1024 * 1024;
                reference_config.resources.temp_limit = 64 * 1024 * 1024;
                if overlay {
                    reference_config.rep_overlay = Some(crate::config::RepConfig {
                        distance: 128 * 1024,
                        min_match: 4096,
                    });
                }
                let reference_context =
                    ResourceContext::with_resources(&reference_config.resources).unwrap();
                let reference_candidates =
                    crate::find_matches_m3(data.as_slice(), &reference_config, &reference_context)
                        .unwrap();
                let mut reference_archive = Vec::new();
                let reference_stats = crate::v3::writer::compress_reference(
                    data.as_slice(),
                    &mut reference_archive,
                    &reference_config,
                    reference_candidates.iter().copied(),
                )
                .unwrap();
                let reference_ir = crate::inspect_matches(&reference_archive[..]).unwrap();
                let mut reference_plain = Vec::new();
                crate::decompress(&reference_archive[..], &mut reference_plain).unwrap();

                let mut config = reference_config.clone();
                config.resources.memory = 8 * 1024 * 1024;
                let mut context = ResourceContext::with_resources(&config.resources).unwrap();
                context.candidate_index_memtable_bytes = Some(1);
                let session = CompressionSession::new(
                    &context,
                    &config.resources,
                    OffloadPolicy {
                        spill_mode: crate::offload::session::SpillMode::ForceSpill,
                        ..OffloadPolicy::default()
                    },
                )
                .unwrap();
                let fixture_hold = context.memory.reserve(3200 * 1024).unwrap();
                test_verifier_observation_reset();
                let mut actual_archive = Vec::new();
                let actual_stats = crate::codec::compress_in_session(
                    data.as_slice(),
                    &mut actual_archive,
                    &config,
                    &session,
                    None::<std::iter::Empty<crate::match_ir::MatchCandidate>>,
                )
                .unwrap();
                let observation = test_verifier_observation();
                eprintln!(
                    "M3_PRESSURE_ADMISSION overlay={} input={} available_before={} mandatory={} available_after={} calls={} file_views={} budget_denials={} source_spills={}",
                    overlay,
                    observation.input_len,
                    observation.available_before,
                    observation.mandatory_bytes,
                    observation.available_after,
                    observation.calls,
                    observation.file_views,
                    observation.budget_denials,
                    session.candidate_index_spill_count(),
                );
                assert_eq!(actual_archive, reference_archive);
                assert_eq!(actual_stats, reference_stats);
                assert_eq!(
                    crate::inspect_matches(&actual_archive[..]).unwrap(),
                    reference_ir
                );
                let mut actual_plain = Vec::new();
                crate::decompress(&actual_archive[..], &mut actual_plain).unwrap();
                assert_eq!(actual_plain, data);
                assert_eq!(actual_plain, reference_plain);
                assert!(actual_stats.semantic_match_count > 0);
                assert!(observation.calls > 0);
                assert!(observation.file_views > 0);
                assert!(observation.budget_denials > 0);
                assert!(observation.input_len <= observation.available_before);
                assert!(observation.input_len > observation.available_after);
                assert!(observation.mandatory_bytes > 0);
                assert_eq!(
                    observation.available_before - observation.available_after,
                    observation.mandatory_bytes
                );
                assert!(session.candidate_index_spill_count() > 0);
                assert_eq!(session.file_usage().current, 0);
                assert_eq!(session.handle_usage().current, 7);
                assert_eq!(session.io_reserved(), 0);
                let temp_root = session.temp_directory().unwrap().to_path_buf();
                assert_eq!(std::fs::read_dir(temp_root).unwrap().count(), 0);
                drop(session);
                drop(fixture_hold);
                assert_eq!(context.memory.current(), 0);
                assert_eq!(context.temp.current(), 0);
            }

            run(false);
            run(true);
        }

        #[test]
        fn two_real_epochs_overlay_and_fallback_agree_and_close_resources() {
            let data = b"abcdefgh01234567abcdefgh01234567".repeat(4);
            let mut reference = None;
            for fallback in [false, true] {
                let (ctx, s, spool, mut c) = setup(&data);
                c.rep_overlay = Some(crate::config::RepConfig {
                    distance: 128,
                    min_match: 26,
                });
                let _deny = if fallback {
                    Some(Scope::on(&super::DENY))
                } else {
                    None
                };
                let store = crate::offload::prepare_m3_schedule_input(&spool, &c, &s).unwrap();
                let mut rows = Vec::new();
                {
                    let mut cursor = store.cursor().unwrap();
                    while let Some(row) = cursor.next().unwrap() {
                        rows.push(row.0);
                    }
                }
                if let Some(expected) = &reference {
                    assert_eq!(&rows, expected);
                } else {
                    reference = Some(rows);
                }
                s.finish_generation().unwrap();
                assert!(
                    crate::candidate_index::CodecPagedIndexFactory::new(&s)
                        .create(&s)
                        .is_err()
                );
                drop(store);
                drop(spool);
                s.retry_cleanup().unwrap();
                assert_eq!(ctx.temp.current(), 0);
                assert_eq!(s.handle_usage().current, 7);
                drop(s);
                assert_eq!(ctx.memory.current(), 0);
            }
        }

        #[test]
        fn pending_semantic_error_cannot_hide_verifier_io_or_integrity_and_replay_is_terminal() {
            let (_, s, spool, c) = setup(&[0; 96]);
            let mut first = Pass1::new(&spool, &c, &s, 3).unwrap();
            first.begin(0, 3, 0).unwrap();
            first.emit(row(0, 32, u64::MAX, 0)).unwrap();
            first.end(0, 3, 0, 1).unwrap();
            s.inject(Operation::Read, 0, FaultAction::Fail);
            let e = match first.finish(summary(&s, 1)) {
                Err(e) => e,
                Ok(_) => panic!("pending semantic hid barrier I/O"),
            };
            assert_eq!(e.kind(), crate::ErrorKind::TemporaryStorageFailure);
            let (_, s, spool, c) = setup(&[0; 96]);
            let cert = certificate(&spool, &c, &s);
            let mut second = Pass2::new(cert, &spool, &c, &s, 3).unwrap();
            second.begin(0, 3, 0).unwrap();
            assert!(second.emit(row(0, 32, 32, 4)).is_err());
            assert!(second.emit(row(0, 32, 32, 0)).is_err());
            assert!(second.finish(summary(&s, 0)).is_err());
        }

        #[test]
        fn invalid_same_interval_alternative_is_checked_before_reduction() {
            let mut bytes = vec![0; 96];
            bytes[1] = 1;
            let (_, s, spool, c) = setup(&bytes);
            let mut p = Pass1::new(&spool, &c, &s, 3).unwrap();
            p.begin(0, 3, 0).unwrap();
            p.emit(row(2, 40, 32, 0)).unwrap(); // valid representative alternative
            p.emit(row(1, 40, 32, 1)).unwrap(); // bad source must not disappear
            p.end(0, 3, 0, 2).unwrap();
            let e = match p.finish(summary(&s, 2)) {
                Err(e) => e,
                Ok(_) => panic!("invalid alternative hidden"),
            };
            assert_eq!(e.context(), Some("candidate bytes do not match"));
        }

        #[test]
        fn representative_cache_eviction_reintroduction_matches_independent_wis() {
            // The directed rows collide in one four-slot set for the actual
            // representative table. They include four initial fills,
            // eviction/reintroduction, distinct lengths, a later smaller
            // source, and a late ordinal tie. A full independent RAM
            // normalizer supplies the expected WIS chain and archive proof.
            //
            // Eviction/reintroduction honesty: the reintroduced row reuses an
            // ACTUAL evicted key recorded by the production retain() path
            // (not rows[2], which the current table keeps resident). The
            // forced run boundary between the two passes makes the evicted
            // row return through a real merge.
            let (_, s, spool, c) = setup(&[0; 800]);
            let bucket = |r: CandidateRow| {
                let mut hash = r.candidate.dst.wrapping_mul(0x9e3779b97f4a7c15)
                    ^ r.candidate.len.rotate_left(27);
                hash ^= hash >> 31;
                hash = hash.wrapping_mul(0xbf58476d1ce4e5b9);
                hash ^= hash >> 29;
                hash as usize & (32768usize / 4 - 1)
            };
            let mut rows = Vec::new();
            let target_bucket = bucket(row(0, 30, 26, 0));
            // Seven DISTINCT colliding keys first; duplicates are spliced
            // below so each duplicate pair is consecutive in emission order
            // (the only order in which Pass2's pending slot can coalesce
            // them, exactly as a real ordinal stream would).
            let mut keys = Vec::new();
            for dst in 30..800 {
                for len in 26..=(800 - dst) {
                    let candidate = row(10, dst, len, 0);
                    if bucket(candidate) == target_bucket
                        && !keys.iter().any(|&(d, l)| (d, l) == (dst, len))
                    {
                        keys.push((dst, len));
                        if keys.len() == 7 {
                            break;
                        }
                    }
                }
                if keys.len() == 7 {
                    break;
                }
            }
            assert!(
                keys.len() == 7,
                "directed collision search must find 7 keys"
            );
            // Emission order (ordinals reassigned contiguously afterwards):
            // - key0 twice with EQUAL src: later ordinal keeps the earlier
            //   (genuine ordinal_kept update);
            // - key1 then the same key with a SMALLER src: the later row
            //   replaces the pending representative (genuine smaller_source
            //   update);
            // - remaining distinct keys.
            let (d0, l0) = keys[0];
            let (d1, l1) = keys[1];
            let mut emitted = vec![(10, d0, l0), (10, d0, l0), (10, d1, l1), (0, d1, l1)];
            for &(d, l) in &keys[2..] {
                emitted.push((10, d, l));
            }
            for (n, (src, dst, len)) in emitted.iter().enumerate() {
                rows.push(CandidateRow {
                    candidate: MatchCandidate {
                        src: *src,
                        dst: *dst,
                        len: *len,
                        insertion_ordinal: n as u64,
                    },
                    provenance: Provenance::new(3, 0).unwrap(),
                });
            }
            assert!(rows.iter().all(|r| bucket(*r) == target_bucket));
            let rows = rows;
            let mut p = Pass1::new(&spool, &c, &s, 3).unwrap();
            p.begin(0, 3, 0).unwrap();
            for &r in &rows {
                p.emit(r).unwrap();
            }
            let row_count = rows.len() as u64;
            p.end(0, 3, 0, row_count).unwrap();
            let cert = p.finish(summary(&s, row_count)).unwrap();
            super::CACHE_OBSERVATION.with(|v| v.set(super::CacheObservation::default()));
            let sort_scope = crate::offload::sort::TestSortScope::new(2);
            let mut replay = Pass2::new(cert, &spool, &c, &s, 3).unwrap();
            replay.begin(0, 3, 0).unwrap();
            // Emit all but the final row, then force a sealed run boundary:
            // the rows pushed so far become a real leaf run, so the ACTUAL
            // evicted key (recorded by retain()) must return through a merge
            // when reintroduced below.
            for &r in &rows[..rows.len() - 1] {
                replay.emit(r).unwrap();
            }
            let evicted_before_boundary = super::CACHE_OBSERVATION.with(|v| v.get().observed_len);
            assert!(
                evicted_before_boundary > 0,
                "directed fixture must evict before the forced boundary"
            );
            replay.test_force_run_boundary().unwrap();
            let (evicted_dst, evicted_len, evicted_src, _) = super::CACHE_OBSERVATION.with(|v| {
                v.get().observed_keys[evicted_before_boundary - 1].expect("recorded evicted key")
            });
            // Reintroduce the ACTUAL evicted row with a fresh later ordinal.
            // Same (dst,len,src): the duplicate path observes the key event.
            let reintroduced = CandidateRow {
                candidate: MatchCandidate {
                    src: evicted_src,
                    dst: evicted_dst,
                    len: evicted_len,
                    insertion_ordinal: row_count,
                },
                provenance: Provenance::new(3, 0).unwrap(),
            };
            // The transcript demands contiguous ordinals: replay the final
            // directed row first (ordinal row_count - 1), then the
            // reintroduction (ordinal row_count).
            replay.emit(rows[rows.len() - 1]).unwrap();
            replay.emit(reintroduced).unwrap();
            let full_count = row_count + 1;
            replay.end(0, 3, 0, full_count).unwrap();
            // The certificate was taken over `rows`; re-seal Pass1 over the
            // extended stream so the replay digest still matches.
            drop(replay);
            let mut p = Pass1::new(&spool, &c, &s, 4).unwrap();
            p.begin(0, 3, 0).unwrap();
            for &r in &rows {
                p.emit(r).unwrap();
            }
            p.emit(reintroduced).unwrap();
            p.end(0, 3, 0, full_count).unwrap();
            let cert = p.finish(summary(&s, full_count)).unwrap();
            let mut replay = Pass2::new(cert, &spool, &c, &s, 4).unwrap();
            replay.begin(0, 3, 0).unwrap();
            for &r in &rows[..rows.len() - 1] {
                replay.emit(r).unwrap();
            }
            replay.test_force_run_boundary().unwrap();
            replay.emit(rows[rows.len() - 1]).unwrap();
            replay.emit(reintroduced).unwrap();
            replay.end(0, 3, 0, full_count).unwrap();
            let prepared = replay.finish(summary(&s, full_count)).unwrap();
            let sort_observation = sort_scope.observation();
            let cache_observation = super::CACHE_OBSERVATION.with(std::cell::Cell::get);
            // The ACTUAL evicted key was reintroduced and observed.
            assert!(cache_observation.evictions > 0);
            assert!(cache_observation.reintroduced > 0);
            assert!(cache_observation.smaller_source > 0);
            assert!(cache_observation.ordinal_kept > 0);
            assert!(
                cache_observation.observed_keys[..cache_observation.observed_len.min(64)]
                    .iter()
                    .flatten()
                    .any(|&(dst, len, _, _)| dst == evicted_dst && len == evicted_len),
                "reintroduced key must be an actually evicted key"
            );
            // Full prepared Match replay: every field of every row must match
            // the expected representative set, not just the COUNT.
            let mut full_rows = rows.clone();
            full_rows.push(reintroduced);
            let mut expected_full = full_rows.iter().map(|r| r.candidate).fold(
                Vec::<MatchCandidate>::new(),
                |mut out, candidate| {
                    if let Some(existing) = out.iter_mut().find(|existing| {
                        (existing.dst, existing.len) == (candidate.dst, candidate.len)
                    }) {
                        if (candidate.src, candidate.insertion_ordinal)
                            < (existing.src, existing.insertion_ordinal)
                        {
                            *existing = candidate;
                        }
                    } else {
                        out.push(candidate);
                    }
                    out
                },
            );
            let mut actual = Vec::new();
            let mut cursor = prepared.cursor().unwrap();
            while let Some(row) = cursor.next().unwrap() {
                actual.push(row.0.candidate);
            }
            drop(cursor);
            actual.sort_unstable_by_key(|r| (r.dst, r.len, r.src, r.insertion_ordinal));
            expected_full.sort_unstable_by_key(|r| (r.dst, r.len, r.src, r.insertion_ordinal));
            assert_eq!(actual, expected_full);
            // The reintroduced evicted row is genuinely present in the output.
            assert!(
                actual
                    .iter()
                    .any(|r| (r.dst, r.len, r.src) == (evicted_dst, evicted_len, evicted_src)),
                "reintroduced evicted row must survive in prepared output"
            );
            assert!(
                prepared.len() > 1,
                "directed fixture must force multiple end-sort rows"
            );
            assert!(sort_observation.interval_leaf_runs > 1);
            assert!(sort_observation.interval_merges > 0);
            // Prepared source counts are REAL leaves/merges, not prepared.len:
            // leaves accepted only with merges proving compaction ran.
            assert!(
                sort_observation.interval_leaf_runs >= 2 && sort_observation.interval_merges >= 1
            );
            eprintln!(
                "M3_CACHE_OBSERVATION evictions={} reintroduced={} smaller_source={} ordinal_kept={} evicted_key=({}, {}) leaves={} merges={} prepared={}",
                cache_observation.evictions,
                cache_observation.reintroduced,
                cache_observation.smaller_source,
                cache_observation.ordinal_kept,
                cache_observation.evicted_dst,
                cache_observation.evicted_len,
                sort_observation.interval_leaf_runs,
                sort_observation.interval_merges,
                prepared.len()
            );

            use crate::offload::contracts::SelectedStore;
            let independent = crate::match_ir::normalize_matches_with_budget(
                full_rows.iter().map(|r| r.candidate),
                spool.len,
                c.effective_min_match().unwrap(),
                &crate::resource::MemoryBudget::new(8 * 1024 * 1024),
            )
            .unwrap();
            let expected_ir = independent.as_slice().to_vec();
            let builder = crate::offload::matches::DiskSelectedBuilder::new(
                spool.len,
                c.effective_min_match().unwrap(),
                &s,
            )
            .unwrap();
            let selected = crate::offload::schedule::DiskScheduler::new(builder)
                .schedule_prepared(prepared, &s)
                .unwrap();
            // Full IR comparison: every Match field, not just the count.
            let mut actual_ir = Vec::new();
            let mut cursor = selected.cursor().unwrap();
            while let Some(row) = cursor.next().unwrap() {
                actual_ir.push(row);
            }
            drop(cursor);
            assert_eq!(actual_ir, expected_ir);
            assert_eq!(selected.summary().count as usize, expected_ir.len());
            let mut bounded_archive = Vec::new();
            let (mut staged, bounded_stats) =
                crate::v3::writer::stage_selected(&spool, &c, &selected, &s).unwrap();
            let permit = crate::codec::prepare_publication(&mut staged, &s).unwrap();
            crate::codec::copy_staged_exact(&mut staged, &mut bounded_archive, permit, &s).unwrap();
            let mut reference_archive = Vec::new();
            let reference_stats = crate::v3::writer::compress_reference(
                &[0; 800][..],
                &mut reference_archive,
                &c,
                full_rows.iter().map(|r| r.candidate),
            )
            .unwrap();
            assert_eq!(bounded_archive, reference_archive);
            assert_eq!(bounded_stats, reference_stats);
            // Sequence + stats + roundtrip + cleanup, as before.
            let mut reference_plain = Vec::new();
            crate::decompress(&reference_archive[..], &mut reference_plain).unwrap();
            assert_eq!(reference_plain, vec![0; 800]);
            let mut actual_plain = Vec::new();
            crate::decompress(&bounded_archive[..], &mut actual_plain).unwrap();
            assert_eq!(actual_plain, reference_plain);
            drop(staged);
            drop(selected);
            s.retry_cleanup().unwrap();
            assert_eq!(s.file_usage().current, 0);
        }

        #[test]
        fn trusted_input_digest_is_not_adopted_on_short_snapshot_or_between_passes() {
            let (_, s, spool, c) = setup(&[0; 96]);
            spool.file.set_len(7).unwrap();
            let e = match Pass1::new(&spool, &c, &s, 3) {
                Err(e) => e,
                Ok(_) => panic!("short snapshot"),
            };
            assert_eq!(e.kind(), crate::ErrorKind::TemporaryStorageFailure);
            let (_, s, spool, c) = setup(&[0; 96]);
            let cert = certificate(&spool, &c, &s);
            let mut f = spool._temp.reopen().unwrap();
            std::io::Write::write_all(&mut f, &[5]).unwrap();
            let e = match Pass2::new(cert, &spool, &c, &s, 3) {
                Err(e) => e,
                Ok(_) => panic!("mutated pass2 input"),
            };
            assert_eq!(e.kind(), crate::ErrorKind::ChecksumMismatch);
        }

        #[test]
        fn transcript_has_fixed_le_phase_row_and_full_counter_encoding() {
            let (_, s, spool, c) = setup(&[0; 96]);
            let binding = Binding::new(&c).unwrap();
            let input = spool.input_identity();
            let mut t = Transcript::new(binding, input, false);
            t.begin(0, 3, 0).unwrap();
            t.row(row(0, 32, 32, 0)).unwrap();
            t.end(0, 3, 0, 1).unwrap();
            let digest = t.finish(&summary(&s, 1)).unwrap();
            let mut expected = blake3::Hasher::new();
            expected.update(b"SREP-OFF-M3-ENUM-v1\0");
            expected.update(&1u16.to_le_bytes());
            expected.update(&binding.0);
            expected.update(&input.encode());
            let mut begin = [0; 96];
            begin[0] = 1;
            begin[2] = 3;
            expected.update(&begin);
            let mut candidate = [0; 96];
            candidate[0] = 2;
            candidate[2] = 3;
            candidate[16..24].copy_from_slice(&32u64.to_le_bytes());
            candidate[24..32].copy_from_slice(&32u64.to_le_bytes());
            candidate[40] = 3;
            expected.update(&candidate);
            let mut end = [0; 96];
            end[0] = 3;
            end[2] = 3;
            end[16..24].copy_from_slice(&1u64.to_le_bytes());
            end[24..32].copy_from_slice(&1u64.to_le_bytes());
            expected.update(&end);
            let mut done = [0; 96];
            done[0] = 4;
            done[8..16].copy_from_slice(&1u64.to_le_bytes());
            done[16..24].copy_from_slice(&1u64.to_le_bytes());
            done[24..32].copy_from_slice(&1u64.to_le_bytes());
            expected.update(&done);
            assert_eq!(digest, *expected.finalize().as_bytes());
        }
    }
}
#[cfg(test)]
pub(crate) fn test_verifier_file_reset() {
    tests::VERIFIER_FILE_COUNT.with(|v| v.set(0));
}
#[cfg(test)]
pub(crate) fn test_verifier_file_count() -> u64 {
    tests::VERIFIER_FILE_COUNT.with(std::cell::Cell::get)
}
#[cfg(test)]
pub(crate) struct VerifierFileScope {
    previous: bool,
}
#[cfg(test)]
impl VerifierFileScope {
    pub(crate) fn new(force: bool) -> Self {
        let previous = tests::FORCE_VERIFIER_FILE.with(std::cell::Cell::get);
        tests::FORCE_VERIFIER_FILE.with(|v| v.set(force));
        Self { previous }
    }
}
#[cfg(test)]
impl Drop for VerifierFileScope {
    fn drop(&mut self) {
        tests::FORCE_VERIFIER_FILE.with(|v| v.set(self.previous));
    }
}
