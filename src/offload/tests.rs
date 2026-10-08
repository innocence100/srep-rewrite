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

#[test]
fn combined_large_spill_overlay_snapshot_fallback_matches_reference() {
    use crate::codec::compress_in_session;
    use crate::config::{CompressionConfig, Layout, Method, RepConfig};
    use crate::offload::session::{OffloadPolicy, SpillMode};
    use crate::v3::writer::compress_reference;

    // Eight independently patterned blocks keep the M5 candidate volume
    // tractable while making both source-index and candidate-run spills real.
    let mut data = (0usize..256 * 1024)
        .map(|n| ((n.wrapping_mul(131) ^ (n / 17) ^ (n / 4093)) & 0xff) as u8)
        .collect::<Vec<_>>();
    let planted = data[4096..8192].to_vec();
    data[131_072..135_168].copy_from_slice(&planted);
    let planted = data[8192..12_288].to_vec();
    data[196_608..200_704].copy_from_slice(&planted);
    let planted = data[12_288..16_384].to_vec();
    data[65_536..69_632].copy_from_slice(&planted);
    let planted = data[16_384..20_480].to_vec();
    data[229_376..233_472].copy_from_slice(&planted);
    let mut config = CompressionConfig::for_method(Method::M3FixedDigest);
    config.layout = Layout::Index;
    config.block_size = 4096;
    config.min_match = 4096;
    config.seed_size = Some(4096);
    config.rep_overlay = Some(RepConfig {
        distance: 128 * 1024,
        min_match: 4096,
    });
    config.resources.memory = 8 * 1024 * 1024;
    config.resources.temp_limit = 64 * 1024 * 1024;

    let mut reference_config = config.clone();
    reference_config.resources.memory = 256 * 1024 * 1024;
    let reference_context = ResourceContext::with_resources(&reference_config.resources).unwrap();
    let reference_candidates =
        crate::find_matches_m3(data.as_slice(), &reference_config, &reference_context).unwrap();
    let mut reference = Vec::new();
    let reference_stats = compress_reference(
        data.as_slice(),
        &mut reference,
        &reference_config,
        reference_candidates.iter().copied(),
    )
    .unwrap();

    let mut context = ResourceContext::with_resources(&config.resources).unwrap();
    context.candidate_index_memtable_bytes = Some(1);
    let session = CompressionSession::new(
        &context,
        &config.resources,
        OffloadPolicy {
            spill_mode: SpillMode::ForceSpill,
            ..OffloadPolicy::default()
        },
    )
    .unwrap();
    let _verifier_file = crate::offload::normalize::m3::VerifierFileScope::new(true);
    let _deny_snapshot = crate::match_finder::snapshot::test_snapshot_limit(0);
    let _deny_accel = crate::match_finder::fixed::disable_acceleration();
    let sort_scope = crate::offload::sort::TestSortScope::new(2);
    // Reset every observation immediately before the sole production call;
    // reference generation and fixture setup cannot contaminate these counts.
    crate::offload::test_phase_counts_reset();
    crate::match_finder::snapshot::reset_test_denials();
    crate::match_finder::fixed::reset_acceleration_denials();
    crate::offload::normalize::m3::test_verifier_file_reset();
    let mut actual = Vec::new();
    let stats = compress_in_session(
        data.as_slice(),
        &mut actual,
        &config,
        &session,
        None::<std::iter::Empty<crate::match_ir::MatchCandidate>>,
    )
    .unwrap();
    assert_eq!(actual, reference);
    assert_eq!(stats, reference_stats);
    assert!(stats.semantic_match_count > 0);
    let mut reference_plain = Vec::new();
    crate::decompress(&reference[..], &mut reference_plain).unwrap();
    let mut actual_plain = Vec::new();
    crate::decompress(&actual[..], &mut actual_plain).unwrap();
    assert_eq!(actual_plain, data);
    assert_eq!(actual_plain, reference_plain);
    assert_eq!(
        crate::inspect_matches(&actual[..]).unwrap(),
        crate::inspect_matches(&reference[..]).unwrap()
    );
    let (base_count, overlay_count) = crate::offload::test_phase_counts();
    let sort_observation = sort_scope.observation();
    let verifier_files = crate::offload::normalize::m3::test_verifier_file_count();
    let accel_denials = crate::match_finder::fixed::acceleration_denials();
    let snapshot_denials = crate::match_finder::snapshot::test_denials();
    eprintln!(
        "M3_COMBINED_OBSERVATION bytes={} base={} overlay={} source_spills={} verifier_file={} accel_denials={} snapshot_denials={} leaves={} merges={} ram_peak={} temp_peak={}",
        data.len(),
        base_count,
        overlay_count,
        context.candidate_index_spill_count(),
        verifier_files,
        accel_denials,
        snapshot_denials,
        sort_observation.interval_leaf_runs,
        sort_observation.interval_merges,
        context.memory.high_water(),
        context.temp.high_water()
    );
    assert!(base_count > 0);
    assert!(overlay_count > 0);
    // Phase counters are ACTUAL qualifying emissions per phase (checked
    // nextOrdinal - start), not phase-invocation counts: base emissions cover
    // ordinals [0, base_count) and overlay emissions continue from there.
    // Both phases must genuinely emit on this SAME >=256 KiB fixture; an
    // empty overlay phase would contribute zero, not one.
    assert!(base_count >= 1);
    assert!(overlay_count >= 1);
    // The inspected archive IR is the cross-check that both phases' emissions
    // survived end to end: the bounded archive matches the reference archive
    // (asserted above) and carries semantic matches.
    assert!(stats.semantic_match_count > 0);
    assert!(verifier_files > 0);
    assert!(accel_denials > 0);
    assert!(snapshot_denials > 0);
    assert!(sort_observation.interval_leaf_runs > 1);
    assert!(sort_observation.interval_merges > 0);
    assert!(context.candidate_index_spill_count() > 0);
    assert!(context.temp.high_water() > data.len() as u64);
    assert!(context.memory.high_water() <= config.resources.memory);
    assert_eq!(context.temp.current(), 0);
    assert_eq!(session.file_usage().current, 0);
    assert_eq!(session.handle_usage().current, 7);
    assert_eq!(session.io_reserved(), 0);
    let temp_root = session.temp_directory().unwrap().to_path_buf();
    assert_eq!(std::fs::read_dir(temp_root).unwrap().count(), 0);
    drop(session);
    assert_eq!(context.memory.current(), 0);
}
