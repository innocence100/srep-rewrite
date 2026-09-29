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
