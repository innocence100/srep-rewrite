//! In-crate B0 seam tests; T1 extends this module with full pipeline tests later.
use super::contracts::*;
use super::session::{CompressionSession, OffloadPolicy};
use crate::config::ResourceConfig;
use crate::error::{Error, Result};
use crate::match_ir::Match;
use crate::resource::{BudgetedVec, ResourceContext};

fn session() -> CompressionSession {
    let r = ResourceConfig {
        memory: 2 * 1024 * 1024,
        ..ResourceConfig::default()
    };
    CompressionSession::new(
        &ResourceContext::with_resources(&r).unwrap(),
        &r,
        OffloadPolicy::default(),
    )
    .unwrap()
}
struct Verifier(bool);
impl SourceVerifier for Verifier {
    fn finish_verified(&mut self, _: &CompressionSession) -> Result<()> {
        if self.0 {
            Ok(())
        } else {
            Err(Error::corrupt_index("mock source corruption"))
        }
    }
}
fn token(s: &CompressionSession, phase: SourcePhase) -> SourceIntegrityComplete {
    SourceBarrier::new(Verifier(true), s, phase)
        .finish(s)
        .unwrap()
}
#[test]
fn integrity_requires_correct_phases_and_session() {
    let a = session();
    let b = session();
    assert!(
        SourceBarrier::new(Verifier(false), &a, SourcePhase::Base)
            .finish(&a)
            .is_err()
    );
    assert!(
        SourceBarrier::new(Verifier(true), &a, SourcePhase::Base)
            .finish(&b)
            .is_err()
    );
    assert!(
        EnumerationRequirement::new(&a, true)
            .complete(token(&a, SourcePhase::Base), None)
            .is_err()
    );
    assert!(
        EnumerationRequirement::new(&a, true)
            .complete(
                token(&a, SourcePhase::Base),
                Some(token(&a, SourcePhase::Base))
            )
            .is_err()
    );
    assert!(
        EnumerationRequirement::new(&a, true)
            .complete(
                token(&a, SourcePhase::Base),
                Some(token(&b, SourcePhase::Overlay))
            )
            .is_err()
    );
    assert!(
        EnumerationRequirement::new(&a, false)
            .complete(token(&a, SourcePhase::Overlay), None)
            .is_err()
    );
    let done = EnumerationRequirement::new(&a, true)
        .complete(
            token(&a, SourcePhase::Base),
            Some(token(&a, SourcePhase::Overlay)),
        )
        .unwrap();
    done.check_session(&a).unwrap();
    assert!(done.check_session(&b).is_err());
}

// Deliberately a cursor-only mock; no as_slice/whole-collection trait exists.
struct Store;
struct Cursor {
    position: u64,
}
impl RecordCursor<Match> for Cursor {
    fn position(&self) -> u64 {
        self.position
    }
    fn seek_row(&mut self, p: u64) -> Result<()> {
        if p > 2 {
            return Err(Error::corrupt_index("bad seek"));
        }
        self.position = p;
        Ok(())
    }
    fn next(&mut self) -> Result<Option<Match>> {
        if self.position == 1 {
            return Err(Error::temp_storage_context("mock disk read failure"));
        }
        if self.position == 2 {
            return Ok(None);
        }
        self.position += 1;
        Ok(Some(Match {
            src: 0,
            dst: 40,
            len: 30,
            origin_match_id: 0,
        }))
    }
}
impl ReplayStore<Match> for Store {
    fn len(&self) -> u64 {
        2
    }
    fn cursor(&self) -> Result<Box<dyn RecordCursor<Match> + '_>> {
        Ok(Box::new(Cursor { position: 0 }))
    }
}
impl SelectedStore for Store {
    fn summary(&self) -> SelectedSummary {
        SelectedSummary {
            count: 2,
            covered_bytes: 60,
            literal_bytes: 40,
        }
    }
}
struct FailingBuilder;
impl SelectedBuilder for FailingBuilder {
    type Store = Store;
    fn push(&mut self, _: Match) -> Result<()> {
        Err(Error::temp_limit("mock write failure"))
    }
    fn finish(self, _: SelectedSummary) -> Result<Store> {
        Err(Error::corrupt_index("unsealed output"))
    }
}
struct Orderer;
impl<S: SelectedStore + ?Sized> SourceOrderBuilder<S> for Orderer {
    type Store = Store;
    fn build(self, s: &S, _: &CompressionSession) -> Result<Store> {
        let mut cursor = s.cursor()?;
        while cursor.next()?.is_some() {}
        Ok(Store)
    }
}
#[test]
fn fallible_store_builder_and_source_order_seams_need_no_slice() {
    let s = session();
    let store = Store;
    let mut c = store.cursor().unwrap();
    assert_eq!(c.next().unwrap().unwrap().dst, 40);
    assert!(c.next().is_err());
    c.seek_row(2).unwrap();
    assert!(c.next().unwrap().is_none());
    assert!(c.seek_row(3).is_err());
    assert!(Orderer.build(&store, &s).is_err());
    assert!(FailingBuilder.finish(store.summary()).is_err());
}
#[test]
fn clear_with_capacity_keeps_budget_reservation() {
    let budget = crate::resource::MemoryBudget::new(128);
    let mut values = BudgetedVec::with_capacity(4, &budget).unwrap();
    values.resize(4, 1u64).unwrap();
    let capacity = values.capacity();
    values.clear_with_capacity();
    assert_eq!(values.capacity(), capacity);
    assert_eq!(values.len(), 0);
    assert_eq!(budget.current(), values.reserved_bytes());
}

#[test]
fn sequential_base_overlay_reuses_same_charged_source_workspaces() {
    use super::candidate_sink::ExternalCandidateSink;
    use crate::candidate_index::{CandidateIndex, IndexEntry, new_codec_candidate_index};
    let r = ResourceConfig {
        memory: 8 * 1024 * 1024,
        ..ResourceConfig::default()
    };
    let mut c = ResourceContext::with_resources(&r).unwrap();
    c.candidate_index_memtable_bytes = Some(2 * std::mem::size_of::<IndexEntry>() as u64);
    let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
    let mut sink = ExternalCandidateSink::with_chunk_limit(&s, 2).unwrap();
    let requirement = EnumerationRequirement::new(&s, true);
    let mut base = SourceBarrier::new(
        new_codec_candidate_index(&s).unwrap(),
        &s,
        SourcePhase::Base,
    );
    for n in 0u64..12 {
        base.index_mut()
            .insert(IndexEntry::new(0, &7u64.to_le_bytes(), n, n, &[]).unwrap())
            .unwrap();
    }
    base.index_mut()
        .for_each_candidate(0, &7u64.to_le_bytes(), 12, 0, &mut |e| {
            sink.emit(CandidateRow {
                candidate: crate::match_ir::MatchCandidate {
                    src: e.position,
                    dst: 100 + e.position,
                    len: 30,
                    insertion_ordinal: e.insertion_ordinal,
                },
                provenance: Provenance::EXTERNAL,
            })
        })
        .unwrap();
    let base_token = base.finish(&s).unwrap();
    let spills = c.candidate_index_spill_count();
    assert!(spills > 0);
    let retained = c.memory.current();
    let peak = c.memory.high_water();
    // New index starts source positions/ordinals at0; old membership/cache state
    // must not leak. Candidate sink stays live with its separate chunk/runs.
    let mut overlay = SourceBarrier::new(
        new_codec_candidate_index(&s).unwrap(),
        &s,
        SourcePhase::Overlay,
    );
    let mut old_hits = 0;
    overlay
        .index_mut()
        .for_each_candidate(0, &7u64.to_le_bytes(), 100, 0, &mut |_| {
            old_hits += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(old_hits, 0);
    for n in 0u64..12 {
        overlay
            .index_mut()
            .insert(IndexEntry::new(0, &9u64.to_le_bytes(), n, n, &[]).unwrap())
            .unwrap();
    }
    let mut hits = 0;
    overlay
        .index_mut()
        .for_each_candidate(0, &9u64.to_le_bytes(), 12, 0, &mut |_| {
            hits += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(hits, 12);
    let overlay_token = overlay.finish(&s).unwrap();
    requirement
        .complete(base_token, Some(overlay_token))
        .unwrap()
        .check_session(&s)
        .unwrap();
    assert_eq!(c.memory.current(), retained);
    assert!(c.memory.high_water() <= peak + 65536);
    assert!(c.memory.high_water() <= r.memory);
    assert!(c.candidate_index_spill_count() > spills);
    drop(sink.finish().unwrap());
    s.retry_cleanup().unwrap();
    assert_eq!(c.temp.current(), 0);
    assert_eq!(s.handle_usage().current, 7);
    drop(s);
    assert_eq!(c.memory.current(), 0);
}

#[test]
fn failed_base_verification_cannot_be_bypassed_by_reusing_overlay_buffers() {
    use crate::candidate_index::{CandidateIndex, IndexEntry, new_codec_candidate_index};
    let r = ResourceConfig {
        memory: 8 * 1024 * 1024,
        ..ResourceConfig::default()
    };
    let mut c = ResourceContext::with_resources(&r).unwrap();
    c.candidate_index_memtable_bytes = Some(0);
    let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
    let mut base = SourceBarrier::new(
        new_codec_candidate_index(&s).unwrap(),
        &s,
        SourcePhase::Base,
    );
    base.index_mut()
        .insert(IndexEntry::new(0, &7u64.to_le_bytes(), 0, 0, &[]).unwrap())
        .unwrap();
    // Corrupt a real persisted source page, not merely its cached view.
    {
        use std::io::{Seek, SeekFrom, Write};
        let directory = s.temp_directory().unwrap();
        let path = std::fs::read_dir(&*directory)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(128)).unwrap();
        file.write_all(&[255]).unwrap();
        file.flush().unwrap();
    }
    assert!(base.finish(&s).is_err());
    assert!(new_codec_candidate_index(&s).is_err());
    s.retry_cleanup().unwrap();
    assert_eq!(c.temp.current(), 0);
    assert_eq!(s.file_usage().current, 0);
    drop(s);
    assert_eq!(c.memory.current(), 0);
}
