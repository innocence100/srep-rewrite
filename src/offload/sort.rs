//! Fixed-slot binary carry. Pressure compaction never changes semantic ordering.
use super::contracts::{ReplayStore, TableBuilder};
use super::session::CompressionSession;
use super::storage::{FixedRecord, RecordTable, TypedTableBuilder};
use crate::error::{Error, Result};
use crate::resource::BudgetedVec;
use crate::resource::Reservation;

const LEVELS: usize = 64;

/// V1/L1 transform seam: callers supply an already charged fixed-capacity chunk.
/// Reducer must equate adjacent semantically interchangeable rows; None preserves
/// every row. First sorted representative wins. No hash map or hidden collection.
pub(crate) struct ExternalSorter<'a, T: FixedRecord> {
    session: &'a CompressionSession,
    chunk: BudgetedVec<T>,
    carry: CarrySet<T>,
    input_len: u64,
    reduce: fn(&T, &T) -> bool,
    failed: bool,
}
impl<'a, T: FixedRecord> ExternalSorter<'a, T> {
    pub(crate) fn new(
        session: &'a CompressionSession,
        chunk: BudgetedVec<T>,
        input_len: u64,
        reduce: fn(&T, &T) -> bool,
    ) -> Result<Self> {
        if chunk.capacity() == 0 || !chunk.is_empty() {
            return Err(Error::invalid_config(
                "sort chunk must be empty with fixed capacity",
            ));
        }
        Ok(Self {
            session,
            chunk,
            carry: CarrySet::new(session)?,
            input_len,
            reduce,
            failed: false,
        })
    }
    pub(crate) fn push(&mut self, row: T) -> Result<()> {
        if self.failed {
            return Err(Error::temp_storage_context("failed sorter"));
        }
        let result = (|| {
            if self.chunk.len() == self.chunk.capacity() {
                self.flush()?;
            }
            self.chunk.push(row)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn flush(&mut self) -> Result<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        self.chunk
            .as_mut_slice()
            .sort_unstable_by(FixedRecord::compare);
        let reduce = self.reduce;
        self.chunk.dedup_by(|a, b| reduce(a, b));
        self.carry.make_room(self.session, reduce)?;
        let mut builder = TypedTableBuilder::<T>::new(self.session, self.input_len)?;
        for row in self.chunk.iter().copied() {
            builder.append(row)?;
        }
        let n = self.chunk.len() as u64;
        self.carry.insert(
            Run {
                store: builder.finish()?,
                leaf_rows: n,
                chunks: 1,
            },
            self.session,
            reduce,
        )?;
        self.chunk.clear_with_capacity();
        Ok(())
    }
    pub(crate) fn finish(mut self) -> Result<RecordTable<T>> {
        if self.failed {
            return Err(Error::temp_storage_context("failed sorter finish"));
        }
        self.flush()?;
        match self.carry.finish(self.session, self.reduce)? {
            Some(s) => Ok(s),
            None => TypedTableBuilder::<T>::new(self.session, self.input_len)?.finish(),
        }
    }
}
pub(crate) struct Run<T: FixedRecord> {
    pub(crate) store: RecordTable<T>,
    pub(crate) leaf_rows: u64,
    pub(crate) chunks: u64,
}
pub(crate) struct CarrySet<T: FixedRecord> {
    slots: [Option<Run<T>>; LEVELS],
    _metadata: Reservation,
}
impl<T: FixedRecord> CarrySet<T> {
    pub(crate) fn new(s: &CompressionSession) -> Result<Self> {
        Ok(Self {
            slots: std::array::from_fn(|_| None),
            _metadata: s.memory().reserve(std::mem::size_of::<Self>() as u64)?,
        })
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }
    pub(crate) fn live_rows(&self) -> u64 {
        self.slots.iter().flatten().map(|r| r.store.len()).sum()
    }
    pub(crate) fn occupied(&self) -> usize {
        self.slots.iter().flatten().count()
    }
    /// Before creating a leaf keep one file slot for replacement and one ordinary
    /// handle for the new run. Pressure compaction uses the protected handle pool.
    pub(crate) fn make_room(
        &mut self,
        s: &CompressionSession,
        reduce: fn(&T, &T) -> bool,
    ) -> Result<()> {
        loop {
            let handles = s.handle_usage().current;
            let files = s.file_usage().current;
            if handles < s.policy().max_handles
                && files
                    .checked_add(2)
                    .is_some_and(|n| n <= s.policy().max_temp_files)
            {
                return Ok(());
            }
            let mut best: [Option<usize>; 2] = [None, None];
            for i in 0..LEVELS {
                if let Some(r) = self.slots[i].as_ref() {
                    if best[0].is_none_or(|j| r.chunks < self.slots[j].as_ref().unwrap().chunks) {
                        best[1] = best[0];
                        best[0] = Some(i);
                    } else if best[1]
                        .is_none_or(|j| r.chunks < self.slots[j].as_ref().unwrap().chunks)
                    {
                        best[1] = Some(i);
                    }
                }
            }
            let (Some(a), Some(b)) = (best[0], best[1]) else {
                return Err(Error::temp_storage_context(
                    "insufficient run merge headroom",
                ));
            };
            let merged = merge(
                self.slots[a].take().unwrap(),
                self.slots[b].take().unwrap(),
                s,
                reduce,
            )?;
            self.insert(merged, s, reduce)?;
        }
    }
    pub(crate) fn insert(
        &mut self,
        mut incoming: Run<T>,
        s: &CompressionSession,
        reduce: fn(&T, &T) -> bool,
    ) -> Result<()> {
        loop {
            if incoming.chunks == 0 {
                return Err(Error::corrupt_index("zero carry weight"));
            }
            let level = incoming.chunks.ilog2() as usize;
            if let Some(old) = self.slots[level].take() {
                incoming = merge(old, incoming, s, reduce)?;
            } else {
                self.slots[level] = Some(incoming);
                return Ok(());
            }
        }
    }
    pub(crate) fn finish(
        mut self,
        s: &CompressionSession,
        reduce: fn(&T, &T) -> bool,
    ) -> Result<Option<RecordTable<T>>> {
        let mut result = None;
        for slot in &mut self.slots {
            if let Some(run) = slot.take() {
                result = Some(match result {
                    None => run,
                    Some(old) => merge(old, run, s, reduce)?,
                });
            }
        }
        Ok(result.map(|r| r.store))
    }
}
fn merge<T: FixedRecord>(
    left: Run<T>,
    right: Run<T>,
    s: &CompressionSession,
    reduce: fn(&T, &T) -> bool,
) -> Result<Run<T>> {
    left.store.check_session(s)?;
    right.store.check_session(s)?;
    if left.store.input_len() != right.store.input_len() {
        return Err(Error::corrupt_index("merge input identities disagree"));
    }
    let chunks = left
        .chunks
        .checked_add(right.chunks)
        .ok_or_else(|| Error::temp_limit("carry chunk overflow"))?;
    let leaf_rows = left
        .leaf_rows
        .checked_add(right.leaf_rows)
        .ok_or_else(|| Error::temp_limit("carry leaf rows overflow"))?;
    let guard = s.emergency_merge()?;
    let mut output = TypedTableBuilder::<T>::emergency(s, left.store.input_len())?;
    {
        let mut a = left.store.cursor()?;
        let mut b = right.store.cursor()?;
        let mut x = a.next()?;
        let mut y = b.next()?;
        let mut previous: Option<T> = None;
        loop {
            let row = match (x, y) {
                (None, None) => break,
                (Some(v), None) => {
                    x = a.next()?;
                    v
                }
                (None, Some(v)) => {
                    y = b.next()?;
                    v
                }
                (Some(v), Some(w)) => {
                    if v.compare(&w).is_gt() {
                        y = b.next()?;
                        w
                    } else {
                        x = a.next()?;
                        v
                    }
                }
            };
            if previous.is_none_or(|p| !reduce(&p, &row)) {
                output.append(row)?;
                previous = Some(row);
            }
        }
    }
    let mut store = output.finish()?;
    // Inputs undergo uncached verification before closing their actual handles.
    let first = left.store.retire()?;
    let second = right.store.retire()?;
    let lease = match guard.adopt_output(first, second) {
        Ok(lease) => lease,
        Err((error, guard, a, b)) => {
            drop(store);
            drop(guard);
            drop(a);
            drop(b);
            return Err(error);
        }
    };
    store.adopt(lease);
    Ok(Run {
        store,
        leaf_rows,
        chunks,
    })
}
pub(crate) fn same_triple(
    a: &super::contracts::CandidateRow,
    b: &super::contracts::CandidateRow,
) -> bool {
    (a.candidate.src, a.candidate.dst, a.candidate.len)
        == (b.candidate.src, b.candidate.dst, b.candidate.len)
}

#[cfg(test)]
mod tests {
    use super::super::contracts::{CandidateRow, Provenance};
    use super::super::session::OffloadPolicy;
    use super::super::storage::{IntervalRow, SourceMatch};
    use super::*;
    use crate::config::ResourceConfig;
    use crate::match_ir::{Match, MatchCandidate};
    use crate::resource::ResourceContext;
    #[test]
    fn generic_transforms_preserve_distinct_lengths_and_source_order() {
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let chunk = BudgetedVec::with_capacity(2, s.memory()).unwrap();
        let mut sorter = ExternalSorter::new(&s, chunk, 500, |a: &IntervalRow, b: &IntervalRow| {
            (a.0.candidate.dst, a.0.candidate.len) == (b.0.candidate.dst, b.0.candidate.len)
        })
        .unwrap();
        for (src, len) in [(8, 30), (2, 40), (1, 30), (0, 40)] {
            sorter
                .push(IntervalRow(CandidateRow {
                    candidate: MatchCandidate {
                        src,
                        dst: 100,
                        len,
                        insertion_ordinal: src,
                    },
                    provenance: Provenance::EXTERNAL,
                }))
                .unwrap();
        }
        let t = sorter.finish().unwrap();
        assert_eq!(t.len(), 2);
        let mut cur = t.cursor().unwrap();
        assert_eq!(cur.next().unwrap().unwrap().0.candidate.src, 1);
        assert_eq!(cur.next().unwrap().unwrap().0.candidate.src, 0);
        let chunk = BudgetedVec::with_capacity(1, s.memory()).unwrap();
        let mut sorter =
            ExternalSorter::new(&s, chunk, 500, |_: &SourceMatch, _: &SourceMatch| false).unwrap();
        for src in [9, 1, 7, 0] {
            sorter
                .push(SourceMatch(Match {
                    src,
                    dst: 100 + src,
                    len: 30,
                    origin_match_id: src,
                }))
                .unwrap();
        }
        let t = sorter.finish().unwrap();
        let mut cur = t.cursor().unwrap();
        for expected in [0, 1, 7, 9] {
            assert_eq!(cur.next().unwrap().unwrap().0.src, expected);
        }
    }

    #[test]
    fn carry_weight_overflow_is_checked_without_allocating_levels() {
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let a = Run {
            store: TypedTableBuilder::<SourceMatch>::new(&s, 0)
                .unwrap()
                .finish()
                .unwrap(),
            leaf_rows: 0,
            chunks: u64::MAX,
        };
        let b = Run {
            store: TypedTableBuilder::<SourceMatch>::new(&s, 0)
                .unwrap()
                .finish()
                .unwrap(),
            leaf_rows: 0,
            chunks: 1,
        };
        assert!(merge(a, b, &s, |_, _| false).is_err());
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.handle_usage().current, 7);
    }
}
