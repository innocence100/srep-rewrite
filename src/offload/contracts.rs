//! Representation-independent seams. Concrete storage/encoding belongs to S1/L1.
use super::session::{CompressionSession, SessionId};
use crate::error::{Error, Result};
use crate::match_ir::{Match, MatchCandidate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Provenance {
    pub(crate) method: u8,
    pub(crate) phase: u8,
}
impl Provenance {
    pub(crate) const EXTERNAL: Self = Self {
        method: 255,
        phase: 2,
    };
    pub(crate) fn new(method: u8, phase: u8) -> Result<Self> {
        if !((method <= 5 && phase <= 1) || (method == 255 && phase == 2)) {
            return Err(Error::invalid_config("invalid candidate provenance"));
        }
        Ok(Self { method, phase })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CandidateRow {
    pub(crate) candidate: MatchCandidate,
    pub(crate) provenance: Provenance,
}
pub(crate) trait CandidateSink {
    fn emit(&mut self, row: CandidateRow) -> Result<()>;
}

/// All positions are rows, including EOF. A disk failure must not become None.
pub(crate) trait RecordCursor<T: Copy> {
    fn position(&self) -> u64;
    fn seek_row(&mut self, position: u64) -> Result<()>;
    fn next(&mut self) -> Result<Option<T>>;
}
pub(crate) trait ReplayStore<T: Copy> {
    fn len(&self) -> u64;
    fn cursor(&self) -> Result<Box<dyn RecordCursor<T> + '_>>;
}
pub(crate) trait RandomStore<T: Copy>: ReplayStore<T> {
    fn read_at(&self, index: u64) -> Result<T>;
}
/// Finish consumes the builder, including on failure. No RAM materialization seam.
pub(crate) trait TableBuilder<T: Copy> {
    type Store: RandomStore<T>;
    fn append(&mut self, row: T) -> Result<u64>;
    #[cfg(test)]
    fn read_committed(&mut self, index: u64) -> Result<T>;
    fn finish(self) -> Result<Self::Store>;
}
pub(crate) trait CandidateRunSink: CandidateSink {
    type Store: ReplayStore<CandidateRow>;
    fn finish(self) -> Result<Self::Store>;
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SelectedSummary {
    pub(crate) count: u64,
    pub(crate) covered_bytes: u64,
    pub(crate) literal_bytes: u64,
}
pub(crate) trait SelectedStore: ReplayStore<Match> {
    fn summary(&self) -> SelectedSummary;
}
/// L1 owns the destination/source concrete types and their invariants.
pub(crate) trait SelectedBuilder {
    type Store: SelectedStore;
    fn push(&mut self, item: Match) -> Result<()>;
    fn finish(self, expected: SelectedSummary) -> Result<Self::Store>;
}
pub(crate) trait SourceOrderBuilder<S: SelectedStore + ?Sized> {
    type Store: ReplayStore<Match>;
    fn build(self, selected: &S, session: &CompressionSession) -> Result<Self::Store>;
}

// V1 will define CandidateBatch/ValidatedCandidates/ScheduledCandidates privately
// in offload::normalize. In particular there is NO constructible "validated"
// wrapper here. This associated type lets consumers compile against an opaque
// outcome without granting finders a validation constructor.
#[cfg(test)]
pub(crate) trait CandidateValidator<S: ReplayStore<CandidateRow>> {
    type Validated;
    fn validate(self, source: S, session: &CompressionSession) -> Result<Self::Validated>;
}
pub(crate) trait Scheduler<V> {
    type Selected: SelectedStore;
    fn schedule(self, validated: V, session: &CompressionSession) -> Result<Self::Selected>;
}

/// I1's private codec index implements this trait. No token constructor is exposed
/// to finder modules. A factory supplies this object to the barrier owner below.
pub(crate) trait SourceVerifier {
    /// Fresh uncached validation of every remaining live run, retiring it on
    /// success. I1 must also verify each earlier retirement before discarding it.
    fn finish_verified(&mut self, session: &CompressionSession) -> Result<()>;
}
/// I1 owns the concrete index; B0 wires its factory only once I1 is reviewed.
/// The factory result goes to the offload-parent barrier constructor, never to
/// a finder-controlled success-token constructor.
pub(crate) trait CodecSourceIndexFactory {
    type Index: crate::candidate_index::CandidateIndex + SourceVerifier;
    fn create(&self, session: &CompressionSession) -> Result<Self::Index>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourcePhase {
    Base,
    Overlay,
}
#[derive(Debug)]
pub(crate) struct SourceIntegrityComplete {
    session: SessionId,
    phase: SourcePhase,
}

/// Not a public capability/security boundary against malicious crate code:
/// offload components are trusted. Rust privacy prevents finders constructing a
/// success token or passing an arbitrary success closure. Only the offload parent
/// may construct a barrier, at the I1 factory integration seam.
pub(crate) struct SourceBarrier<V> {
    verifier: V,
    session: SessionId,
    phase: SourcePhase,
}
impl<V: SourceVerifier> SourceBarrier<V> {
    pub(crate) fn index_mut(&mut self) -> &mut V {
        &mut self.verifier
    }
    pub(super) fn new(verifier: V, session: &CompressionSession, phase: SourcePhase) -> Self {
        Self {
            verifier,
            session: session.identity(),
            phase,
        }
    }
    pub(crate) fn finish(
        mut self,
        session: &CompressionSession,
    ) -> Result<SourceIntegrityComplete> {
        if self.session != session.identity() {
            return Err(Error::corrupt_index("cross-session source barrier"));
        }
        session.check_cancelled()?;
        self.verifier.finish_verified(session)?;
        session.check_cancelled()?;
        Ok(SourceIntegrityComplete {
            session: self.session,
            phase: self.phase,
        })
    }
}
#[derive(Debug)]
pub(crate) struct EnumerationComplete {
    session: SessionId,
}
pub(crate) struct EnumerationRequirement {
    session: SessionId,
    overlay_required: bool,
}
impl EnumerationRequirement {
    /// Frozen from the codec's configuration, not inferred from returned tokens.
    pub(crate) fn new(session: &CompressionSession, overlay_required: bool) -> Self {
        Self {
            session: session.identity(),
            overlay_required,
        }
    }
    pub(crate) fn complete(
        self,
        base: SourceIntegrityComplete,
        overlay: Option<SourceIntegrityComplete>,
    ) -> Result<EnumerationComplete> {
        if base.session != self.session
            || base.phase != SourcePhase::Base
            || self.overlay_required != overlay.is_some()
            || overlay
                .as_ref()
                .is_some_and(|t| t.session != self.session || t.phase != SourcePhase::Overlay)
        {
            return Err(Error::corrupt_index(
                "missing, wrong-phase or cross-session source completion",
            ));
        }
        Ok(EnumerationComplete {
            session: self.session,
        })
    }
}
impl EnumerationComplete {
    pub(crate) fn check_session(&self, session: &CompressionSession) -> Result<()> {
        if self.session != session.identity() {
            return Err(Error::corrupt_index("cross-session enumeration"));
        }
        Ok(())
    }
}
pub(crate) struct EnumerationSummary {
    pub(crate) next_ordinal: u64,
    pub(crate) qualifying_count: u64,
    pub(crate) source_integrity: EnumerationComplete,
}
// External candidate input has no source-index completion token. It must take
// the separate candidate-validation entry route, never mint a fake token here.

/// S1 owns codecs and page checksums. This identity is required on *every* read;
/// no global mutable page-valid flag is part of the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
pub(crate) struct StoreIdentity {
    pub(crate) session: SessionId,
    pub(crate) generation: u64,
}
pub(crate) trait VerifiedSourcePages {
    #[cfg(test)]
    fn identity(&self) -> StoreIdentity;
    fn entry_count(&self) -> u64;
    #[cfg(test)]
    fn read_entry(&mut self, index: u64) -> Result<crate::candidate_index::IndexEntry>;
    fn verify_uncached(&mut self) -> Result<()>;
}
