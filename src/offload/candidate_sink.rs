//! Candidate sink retains one fixed chunk, never semantic interval heuristics.
use super::contracts::{
    CandidateRow, CandidateRunSink, CandidateSink, RecordCursor, ReplayStore, TableBuilder,
};
use super::session::{CompressionSession, SessionId, SpillMode, WorkspaceKind};
use super::sort::{CarrySet, Run, same_triple};
use super::storage::{CandidateTableBuilder, CandidateTableStore, FixedRecord};
use crate::error::{Error, Result};
use crate::resource::{BudgetedVec, Reservation};
use std::cell::Cell;

#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct CandidateMetrics {
    pub(crate) emitted: u64,
    pub(crate) leaf_retained: u64,
    pub(crate) chunks: u64,
    pub(crate) peak_runs: usize,
    pub(crate) peak_live_rows: u64,
}
pub(crate) enum CandidateStore {
    Ram(RamCandidateStore),
    Disk(CandidateTableStore),
}
/// Keep the charged rows and their construction identity inseparable. Sibling
/// modules may move this owner, but cannot construct or relabel its payload.
pub(crate) struct RamCandidateStore {
    rows: BudgetedVec<CandidateRow>,
    cursors: Cell<u8>,
    session: SessionId,
    input_len: u64,
}
impl CandidateStore {
    /// Trusted construction identity, checked without any file access.
    pub(crate) fn check_session(&self, expected: &CompressionSession) -> Result<()> {
        match self {
            Self::Ram(ram) if ram.session != expected.identity() => {
                Err(Error::corrupt_index("cross-session candidate store"))
            }
            Self::Ram(_) => Ok(()),
            Self::Disk(table) => table.check_session(expected),
        }
    }

    pub(crate) fn input_len(&self) -> u64 {
        match self {
            Self::Ram(ram) => ram.input_len,
            Self::Disk(table) => table.input_len(),
        }
    }

    /// RAM has no encoded envelope; V1 checks canonical order/uniqueness for
    /// both representations after this fresh disk digest verification.
    pub(crate) fn verify_uncached(&self) -> Result<()> {
        match self {
            Self::Ram(_) => Ok(()),
            Self::Disk(table) => table.verify_uncached(),
        }
    }

    /// Fault injection for V1's canonical prepass tests, not an identity setter
    /// or a mutable rows accessor. Absent from production builds.
    #[cfg(test)]
    pub(super) fn replace_ram_row_for_test(&mut self, index: usize, row: CandidateRow) {
        let Self::Ram(ram) = self else {
            panic!("RAM required for row fault injection")
        };
        ram.rows.as_mut_slice()[index] = row;
    }
}
pub(crate) struct ExternalCandidateSink<'a> {
    session: &'a CompressionSession,
    chunk: BudgetedVec<CandidateRow>,
    limit: usize,
    carry: CarrySet<CandidateRow>,
    metrics: CandidateMetrics,
    failed: bool,
    input_len: u64,
}
impl<'a> ExternalCandidateSink<'a> {
    pub(crate) fn new(s: &'a CompressionSession) -> Result<Self> {
        Self::with_chunk_limit(s, usize::MAX)
    }
    pub(crate) fn with_chunk_limit(s: &'a CompressionSession, limit: usize) -> Result<Self> {
        Self::with_input_len(s, 0, limit)
    }
    pub(crate) fn with_input_len(
        s: &'a CompressionSession,
        input_len: u64,
        limit: usize,
    ) -> Result<Self> {
        let chunk = s.take_typed_workspace(WorkspaceKind::Candidate)?;
        let limit = limit.min(chunk.capacity());
        if limit == 0 {
            return Err(Error::memory_limit("empty candidate workspace"));
        }
        Ok(Self {
            session: s,
            chunk,
            limit,
            carry: CarrySet::new(s)?,
            metrics: CandidateMetrics::default(),
            failed: false,
            input_len,
        })
    }
    pub(crate) fn metrics(&self) -> CandidateMetrics {
        self.metrics
    }
    fn canonicalize(&mut self) {
        self.chunk
            .as_mut_slice()
            .sort_unstable_by(FixedRecord::compare);
        self.chunk.dedup_by(|a, b| same_triple(a, b));
    }
    fn flush(&mut self) -> Result<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        if self.session.policy().spill_mode == SpillMode::ForceRam {
            return Err(Error::memory_limit("forced RAM candidate chunk exhausted"));
        }
        self.canonicalize();
        self.carry.make_room(self.session, same_triple)?;
        let n = self.chunk.len() as u64;
        let mut b = CandidateTableBuilder::new(self.session, self.input_len)?;
        for row in self.chunk.iter().copied() {
            b.append(row)?;
        }
        let store = b.finish()?;
        self.metrics.leaf_retained = self
            .metrics
            .leaf_retained
            .checked_add(n)
            .ok_or_else(|| Error::temp_limit("retained leaf counter overflow"))?;
        self.metrics.chunks = self
            .metrics
            .chunks
            .checked_add(1)
            .ok_or_else(|| Error::temp_limit("chunk count overflow"))?;
        self.metrics.peak_runs = self.metrics.peak_runs.max(self.carry.occupied() + 1);
        self.metrics.peak_live_rows = self.metrics.peak_live_rows.max(self.carry.live_rows() + n);
        self.carry.insert(
            Run {
                store,
                leaf_rows: n,
                chunks: 1,
            },
            self.session,
            same_triple,
        )?;
        self.chunk.clear_with_capacity();
        Ok(())
    }
}
impl CandidateSink for ExternalCandidateSink<'_> {
    fn emit(&mut self, row: CandidateRow) -> Result<()> {
        if self.failed {
            return Err(Error::temp_storage_context("failed candidate sink"));
        }
        let result = (|| {
            self.session.check_cancelled()?;
            if self.chunk.len() == self.limit {
                self.flush()?;
            }
            self.chunk.push(row)?;
            self.metrics.emitted = self
                .metrics
                .emitted
                .checked_add(1)
                .ok_or_else(|| Error::temp_limit("emission count overflow"))?;
            if self.session.policy().spill_mode == SpillMode::ForceSpill {
                self.flush()?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
impl CandidateRunSink for ExternalCandidateSink<'_> {
    type Store = CandidateStore;
    fn finish(mut self) -> Result<CandidateStore> {
        if self.failed {
            return Err(Error::temp_storage_context("failed candidate sink finish"));
        }
        if self.carry.is_empty() && self.session.policy().spill_mode != SpillMode::ForceSpill {
            self.canonicalize();
            return Ok(CandidateStore::Ram(RamCandidateStore {
                rows: self.chunk,
                cursors: Cell::new(0),
                session: self.session.identity(),
                input_len: self.input_len,
            }));
        }
        self.flush()?;
        match self.carry.finish(self.session, same_triple)? {
            Some(s) => Ok(CandidateStore::Disk(s)),
            None => Ok(CandidateStore::Disk(
                CandidateTableBuilder::new(self.session, self.input_len)?.finish()?,
            )),
        }
    }
}
struct RamCursor<'a> {
    rows: &'a BudgetedVec<CandidateRow>,
    count: &'a Cell<u8>,
    position: u64,
    _charge: Reservation,
}
impl Drop for RamCursor<'_> {
    fn drop(&mut self) {
        self.count.set(self.count.get() - 1);
    }
}
impl RecordCursor<CandidateRow> for RamCursor<'_> {
    fn position(&self) -> u64 {
        self.position
    }
    fn seek_row(&mut self, p: u64) -> Result<()> {
        if p > self.rows.len() as u64 {
            return Err(Error::corrupt_index("RAM cursor bounds"));
        }
        self.position = p;
        Ok(())
    }
    fn next(&mut self) -> Result<Option<CandidateRow>> {
        let r = self.rows.get(self.position as usize).copied();
        if r.is_some() {
            self.position += 1;
        }
        Ok(r)
    }
}
impl ReplayStore<CandidateRow> for CandidateStore {
    fn len(&self) -> u64 {
        match self {
            Self::Ram(ram) => ram.rows.len() as u64,
            Self::Disk(s) => s.len(),
        }
    }
    fn cursor(&self) -> Result<Box<dyn RecordCursor<CandidateRow> + '_>> {
        match self {
            Self::Disk(s) => s.cursor(),
            Self::Ram(RamCandidateStore { rows, cursors, .. }) => {
                if cursors.get() >= 4 {
                    return Err(Error::memory_limit("RAM cursor limit"));
                }
                let charge = rows.budget().reserve(128)?;
                cursors.set(cursors.get() + 1);
                Ok(Box::new(RamCursor {
                    rows,
                    count: cursors,
                    position: 0,
                    _charge: charge,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::session::{FaultAction, OffloadPolicy, Operation};
    use super::*;
    use crate::config::ResourceConfig;
    use crate::resource::ResourceContext;
    fn row(n: u64, ordinal: u64) -> CandidateRow {
        CandidateRow {
            candidate: crate::match_ir::MatchCandidate {
                src: n,
                dst: 100 + n,
                len: 30,
                insertion_ordinal: ordinal,
            },
            provenance: super::super::contracts::Provenance::EXTERNAL,
        }
    }
    fn collect(store: &impl ReplayStore<CandidateRow>) -> Vec<CandidateRow> {
        let mut c = store.cursor().unwrap();
        let mut v = Vec::new();
        while let Some(r) = c.next().unwrap() {
            v.push(r);
        }
        v
    }
    #[test]
    fn tiny_chunks_many_carries_exact_min_ordinal_and_empty_paths() {
        for mode in [
            SpillMode::Adaptive,
            SpillMode::ForceSpill,
            SpillMode::ForceRam,
        ] {
            let r = ResourceConfig {
                memory: 8 * 1024 * 1024,
                ..ResourceConfig::default()
            };
            let c = ResourceContext::with_resources(&r).unwrap();
            let s = CompressionSession::new(
                &c,
                &r,
                OffloadPolicy {
                    spill_mode: mode,
                    ..OffloadPolicy::default()
                },
            )
            .unwrap();
            let mut sink = ExternalCandidateSink::with_chunk_limit(
                &s,
                if mode == SpillMode::ForceRam { 300 } else { 3 },
            )
            .unwrap();
            let mut reference = Vec::new();
            for ordinal in (0..257).rev() {
                let value = row(ordinal % 17, ordinal);
                reference.push(value);
                sink.emit(value).unwrap();
            }
            reference.sort_unstable_by(FixedRecord::compare);
            reference.dedup_by(|a, b| same_triple(a, b));
            let store = sink.finish().unwrap();
            assert_eq!(collect(&store), reference);
            drop(store);
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), 0);
            assert_eq!(s.file_usage().current, 0);
            assert_eq!(s.handle_usage().current, 7);
        }
        for mode in [SpillMode::Adaptive, SpillMode::ForceSpill] {
            let r = ResourceConfig::default();
            let c = ResourceContext::with_resources(&r).unwrap();
            let s = CompressionSession::new(
                &c,
                &r,
                OffloadPolicy {
                    spill_mode: mode,
                    ..OffloadPolicy::default()
                },
            )
            .unwrap();
            let t = ExternalCandidateSink::new(&s).unwrap().finish().unwrap();
            assert_eq!(t.len(), 0);
            assert!(collect(&t).is_empty());
        }
    }
    #[test]
    fn pressure_merges_adopt_outputs_twice_with_other_handles_live() {
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(
            &c,
            &r,
            OffloadPolicy {
                max_handles: 10,
                max_temp_files: 4,
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        let mut sink = ExternalCandidateSink::with_chunk_limit(&s, 1).unwrap();
        for i in 0..63 {
            sink.emit(row(i % 9, 63 - i)).unwrap();
        }
        let store = sink.finish().unwrap();
        assert_eq!(store.len(), 9);
        assert!(s.handle_usage().peak <= 10);
        assert!(s.file_usage().peak <= 4);
        assert!(s.emergency_merge().is_ok());
        drop(store);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.handle_usage().current, 7);
    }
    #[test]
    fn duplicate_carry_peak_depends_on_leaf_retained_not_final_uniques() {
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let mut sink = ExternalCandidateSink::with_chunk_limit(&s, 2).unwrap();
        for i in 0..255 {
            sink.emit(row(i % 3, 255 - i)).unwrap();
        }
        let metrics = sink.metrics();
        assert!(metrics.leaf_retained > 100);
        assert!(metrics.peak_runs >= 5);
        let store = sink.finish().unwrap();
        assert_eq!(store.len(), 3);
        assert!(c.temp.high_water() > 3 * 64 * 3);
        assert!(c.temp.high_water() <= 66 * 128 + 128 * (metrics.leaf_retained + 2));
        drop(store);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
    }
    #[test]
    fn failure_during_replacement_retains_inputs_until_aborted_cleanup() {
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(
            &c,
            &r,
            OffloadPolicy {
                spill_mode: SpillMode::ForceSpill,
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        let mut sink = ExternalCandidateSink::new(&s).unwrap();
        sink.emit(row(1, 1)).unwrap();
        // Second leaf writes header,row,sealed header; fail replacement row after
        // its header, while both input files are retained and charged.
        s.inject(Operation::Write, 4, FaultAction::Fail);
        assert!(sink.emit(row(2, 2)).is_err());
        assert!(sink.emit(row(3, 3)).is_err());
        assert!(sink.finish().is_err());
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.file_usage().current, 0);
        assert_eq!(s.handle_usage().current, 7);
        assert!(c.temp.high_water() >= 3 * 128 + 2 * 64);
    }
}
