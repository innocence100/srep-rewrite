//! L1-owned selected metadata. Every growing collection is a fixed-record table.
use super::contracts::{
    RecordCursor, ReplayStore, SelectedBuilder, SelectedStore, SelectedSummary, SourceOrderBuilder,
    TableBuilder,
};
use super::session::CompressionSession;
use super::sort::ExternalSorter;
use super::storage::{RecordTable, SourceMatch, TypedTableBuilder};
use crate::error::{Error, Result};
use crate::match_ir::Match;
use crate::resource::BudgetedVec;

pub(crate) struct DiskSelectedBuilder<'a> {
    session: &'a CompressionSession,
    rows: TypedTableBuilder<Match>,
    input_len: u64,
    minimum: u64,
    count: u64,
    covered: u64,
    previous_end: u64,
    failed: bool,
}
impl<'a> DiskSelectedBuilder<'a> {
    pub(crate) fn new(
        input_len: u64,
        minimum: u64,
        session: &'a CompressionSession,
    ) -> Result<Self> {
        if input_len > crate::format_v3::MAX_WIRE || minimum < crate::config::MIN_MATCH_LEN {
            return Err(Error::invalid_match(
                "selected builder input/minimum invalid",
            ));
        }
        Ok(Self {
            session,
            rows: TypedTableBuilder::new(session, input_len)?,
            input_len,
            minimum: minimum.max(26),
            count: 0,
            covered: 0,
            previous_end: 0,
            failed: false,
        })
    }
}
impl SelectedBuilder for DiskSelectedBuilder<'_> {
    type Store = DiskSelectedStore;
    fn push(&mut self, item: Match) -> Result<()> {
        if self.failed {
            return Err(Error::temp_storage_context("failed selected builder"));
        }
        let result = (|| {
            self.session.check_cancelled()?;
            let end = item
                .dst
                .checked_add(item.len)
                .ok_or_else(|| Error::invalid_match("selected match endpoint overflows"))?;
            if item.src >= item.dst
                || item.len < self.minimum
                || end > self.input_len
                || item.dst < self.previous_end
                || item.origin_match_id != self.count
            {
                return Err(Error::invalid_match(
                    "selected match source/length/order/ID invalid",
                ));
            }
            let count = self
                .count
                .checked_add(1)
                .filter(|n| *n <= crate::format_v3::MAX_WIRE)
                .ok_or_else(|| Error::invalid_match("selected count overflows"))?;
            let covered = self
                .covered
                .checked_add(item.len)
                .filter(|n| *n <= self.input_len)
                .ok_or_else(|| Error::invalid_match("selected coverage exceeds input"))?;
            self.rows.append(item)?;
            self.count = count;
            self.covered = covered;
            self.previous_end = end;
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn finish(self, expected: SelectedSummary) -> Result<DiskSelectedStore> {
        if self.failed {
            return Err(Error::temp_storage_context(
                "failed selected builder finish",
            ));
        }
        self.session.check_cancelled()?;
        let actual = SelectedSummary {
            count: self.count,
            covered_bytes: self.covered,
            literal_bytes: self
                .input_len
                .checked_sub(self.covered)
                .ok_or_else(|| Error::invalid_match("selected literal count underflows"))?,
        };
        if expected != actual {
            return Err(Error::corrupt_index("selected summary mismatch"));
        }
        Ok(DiskSelectedStore {
            rows: self.rows.finish()?,
            summary: actual,
            input_len: self.input_len,
        })
    }
}

pub(crate) struct DiskSelectedStore {
    rows: RecordTable<Match>,
    summary: SelectedSummary,
    input_len: u64,
}
impl DiskSelectedStore {
    pub(crate) fn input_len(&self) -> u64 {
        self.input_len
    }
    pub(crate) fn verify(&self, session: &CompressionSession) -> Result<()> {
        self.rows.check_session(session)?;
        session.check_cancelled()?;
        self.rows.verify_uncached()?;
        if self.rows.len() != self.summary.count
            || self.rows.input_len() != self.input_len
            || self
                .summary
                .covered_bytes
                .checked_add(self.summary.literal_bytes)
                != Some(self.input_len)
        {
            return Err(Error::corrupt_index("selected store metadata inconsistent"));
        }
        // Structural invariants are checked by builder and records are immutable.
        Ok(())
    }
}
impl ReplayStore<Match> for DiskSelectedStore {
    fn len(&self) -> u64 {
        self.summary.count
    }
    fn cursor(&self) -> Result<Box<dyn RecordCursor<Match> + '_>> {
        self.rows.cursor()
    }
}
impl SelectedStore for DiskSelectedStore {
    fn summary(&self) -> SelectedSummary {
        self.summary
    }
}

pub(crate) struct DiskSourceOrderBuilder {
    chunk_records: usize,
}
impl Default for DiskSourceOrderBuilder {
    fn default() -> Self {
        Self {
            chunk_records: 4096,
        }
    }
}
impl DiskSourceOrderBuilder {
    pub(crate) fn with_chunk_records(chunk_records: usize) -> Result<Self> {
        if chunk_records == 0 || chunk_records > 4096 {
            return Err(Error::invalid_config("source-order chunk outside1..4096"));
        }
        Ok(Self { chunk_records })
    }
}
pub(crate) struct DiskSourceOrderedStore {
    rows: RecordTable<SourceMatch>,
    memory: crate::resource::MemoryBudget,
}
impl SourceOrderBuilder<DiskSelectedStore> for DiskSourceOrderBuilder {
    type Store = DiskSourceOrderedStore;
    fn build(
        self,
        selected: &DiskSelectedStore,
        session: &CompressionSession,
    ) -> Result<Self::Store> {
        selected.verify(session)?;
        let count = self
            .chunk_records
            .min((session.memory().limit() / 1024).max(1) as usize);
        let chunk = BudgetedVec::with_capacity(count, session.memory())?;
        let mut sort = ExternalSorter::new(
            session,
            chunk,
            selected.input_len,
            |_: &SourceMatch, _: &SourceMatch| false,
        )?;
        let mut cursor = selected.cursor()?;
        while let Some(item) = cursor.next()? {
            session.check_cancelled()?;
            sort.push(SourceMatch(item))?;
        }
        let rows = sort.finish()?;
        if rows.len() != selected.len() {
            return Err(Error::corrupt_index("source-order match count changed"));
        }
        Ok(DiskSourceOrderedStore {
            rows,
            memory: session.memory().clone(),
        })
    }
}
struct SourceCursor<'a> {
    inner: Box<dyn RecordCursor<SourceMatch> + 'a>,
    _charge: crate::resource::Reservation,
}
impl RecordCursor<Match> for SourceCursor<'_> {
    fn position(&self) -> u64 {
        self.inner.position()
    }
    fn seek_row(&mut self, position: u64) -> Result<()> {
        self.inner.seek_row(position)
    }
    fn next(&mut self) -> Result<Option<Match>> {
        self.inner.next().map(|r| r.map(|v| v.0))
    }
}
impl DiskSourceOrderedStore {
    pub(crate) fn verify(&self, session: &CompressionSession) -> Result<()> {
        self.rows.check_session(session)?;
        self.rows.verify_uncached()
    }
    // Explicit session admission for the adapter box; underlying cursor remains
    // separately charged and capped by the S1 table.
    pub(crate) fn cursor_in(
        &self,
        session: &CompressionSession,
    ) -> Result<Box<dyn RecordCursor<Match> + '_>> {
        self.rows.check_session(session)?;
        let charge = session.memory().reserve(128)?;
        Ok(Box::new(SourceCursor {
            inner: self.rows.cursor()?,
            _charge: charge,
        }))
    }
}
// SourceOrderBuilder requires ReplayStore. Store the adapter's budget in the
// concrete store, rather than an unaccounted per-cursor allocation.
impl ReplayStore<Match> for DiskSourceOrderedStore {
    fn len(&self) -> u64 {
        self.rows.len()
    }
    fn cursor(&self) -> Result<Box<dyn RecordCursor<Match> + '_>> {
        let charge = self.memory.reserve(128)?;
        Ok(Box::new(SourceCursor {
            inner: self.rows.cursor()?,
            _charge: charge,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::session::{FaultAction, OffloadPolicy, Operation};
    use super::*;
    use crate::config::ResourceConfig;
    use crate::resource::ResourceContext;
    #[test]
    fn builder_invariants_terminal_errors_empty_summary_and_cleanup() {
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let empty = DiskSelectedBuilder::new(100, 32, &s)
            .unwrap()
            .finish(SelectedSummary {
                count: 0,
                covered_bytes: 0,
                literal_bytes: 100,
            })
            .unwrap();
        assert_eq!(empty.len(), 0);
        drop(empty);
        for item in [
            Match {
                src: 40,
                dst: 40,
                len: 32,
                origin_match_id: 0,
            },
            Match {
                src: 0,
                dst: 40,
                len: 31,
                origin_match_id: 0,
            },
            Match {
                src: 0,
                dst: 90,
                len: 32,
                origin_match_id: 0,
            },
            Match {
                src: 0,
                dst: 40,
                len: 32,
                origin_match_id: 1,
            },
            Match {
                src: 0,
                dst: 40,
                len: u64::MAX,
                origin_match_id: 0,
            },
        ] {
            let mut b = DiskSelectedBuilder::new(100, 32, &s).unwrap();
            assert!(b.push(item).is_err());
            assert!(
                b.push(Match {
                    src: 0,
                    dst: 40,
                    len: 32,
                    origin_match_id: 0
                })
                .is_err()
            );
            assert!(b.finish(SelectedSummary::default()).is_err());
        }
        let mut b = DiskSelectedBuilder::new(100, 32, &s).unwrap();
        b.push(Match {
            src: 0,
            dst: 40,
            len: 32,
            origin_match_id: 0,
        })
        .unwrap();
        assert!(
            b.push(Match {
                src: 0,
                dst: 60,
                len: 32,
                origin_match_id: 1
            })
            .is_err()
        );
        assert!(b.finish(SelectedSummary::default()).is_err());
        let b = DiskSelectedBuilder::new(100, 32, &s).unwrap();
        assert!(b.finish(SelectedSummary::default()).is_err());
        let mut b = DiskSelectedBuilder::new(100, 32, &s).unwrap();
        s.inject(Operation::Write, 0, FaultAction::Fail);
        assert!(
            b.push(Match {
                src: 0,
                dst: 40,
                len: 32,
                origin_match_id: 0
            })
            .is_err()
        );
        assert!(b.finish(SelectedSummary::default()).is_err());
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.file_usage().current, 0);
    }

    #[test]
    fn source_order_many_runs_preserves_ids_and_rejects_corrupt_selected_file() {
        use std::io::{Read, Seek, SeekFrom, Write};
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let mut b = DiskSelectedBuilder::new(10000, 32, &s).unwrap();
        for id in 0..100 {
            b.push(Match {
                src: (99 - id) % 17,
                dst: 100 + id * 64,
                len: 32,
                origin_match_id: id,
            })
            .unwrap();
        }
        let selected = b
            .finish(SelectedSummary {
                count: 100,
                covered_bytes: 3200,
                literal_bytes: 6800,
            })
            .unwrap();
        let ordered = DiskSourceOrderBuilder::with_chunk_records(3)
            .unwrap()
            .build(&selected, &s)
            .unwrap();
        let mut cursor = ordered.cursor().unwrap();
        let mut previous = None;
        let mut ids = Vec::new();
        while let Some(m) = cursor.next().unwrap() {
            let key = (m.src, m.dst, m.origin_match_id);
            assert!(previous.is_none_or(|p| p <= key));
            previous = Some(key);
            ids.push(m.origin_match_id);
        }
        ids.sort_unstable();
        assert_eq!(ids, (0..100).collect::<Vec<_>>());
        drop(cursor);
        drop(ordered);
        let (_, other) = {
            let context = ResourceContext::with_resources(&r).unwrap();
            let session = CompressionSession::new(&context, &r, OffloadPolicy::default()).unwrap();
            (context, session)
        };
        assert!(selected.verify(&other).is_err());
        let dir = s.temp_directory().unwrap();
        let path = std::fs::read_dir(&*dir)
            .unwrap()
            .find_map(|e| {
                let path = e.unwrap().path();
                let mut f = std::fs::File::open(&path).unwrap();
                let mut h = [0; 128];
                f.read_exact(&mut h).unwrap();
                (h[14] == 3 && h[15] == 4).then_some(path)
            })
            .unwrap();
        drop(dir);
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        f.seek(SeekFrom::Start(128)).unwrap();
        let mut byte = [0];
        f.read_exact(&mut byte).unwrap();
        byte[0] ^= 1;
        f.seek(SeekFrom::Start(128)).unwrap();
        f.write_all(&byte).unwrap();
        drop(f);
        assert!(selected.verify(&s).is_err());
        assert!(
            DiskSourceOrderBuilder::default()
                .build(&selected, &s)
                .is_err()
        );
        drop(selected);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.file_usage().current, 0);
    }
}
