use std::fs;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use srep::{
    Checksum, CompressionConfig, ErrorKind, Layout, MatchCandidate, Method, ResourceConfig,
    ResourceContext, compress_with_candidates_with_context, decompress_with_context,
};

const MEMORY_LIMIT: u64 = 8 * 1024 * 1024;
const INPUT_LEN: usize = 512 * 1024;

fn fixture() -> (Vec<u8>, Vec<MatchCandidate>) {
    let input = b"0123456789abcdef".repeat(INPUT_LEN / 16);
    let candidates = (1024..INPUT_LEN - 64)
        .step_by(64)
        .enumerate()
        .map(|(ordinal, dst)| MatchCandidate {
            src: 0,
            dst: dst as u64,
            len: 64,
            insertion_ordinal: ordinal as u64,
        })
        .collect();
    (input, candidates)
}

fn config(temp_dir: &std::path::Path, temp_limit: u64) -> CompressionConfig {
    let mut config = CompressionConfig::for_method(Method::M1RollingCdc);
    config.layout = Layout::Index;
    config.checksum = Checksum::Blake3;
    config.block_size = 1024;
    config.min_match = 32;
    config.target_chunk = Some(32);
    config.resources = ResourceConfig {
        memory: MEMORY_LIMIT,
        temp_dir: temp_dir.to_path_buf(),
        temp_limit,
        ..ResourceConfig::default()
    };
    config
}

fn assert_clean_temp_root(path: &std::path::Path) {
    assert!(fs::read_dir(path).unwrap().next().is_none());
}

fn candidates_with_progress(
    candidates: &[MatchCandidate],
    progress: Arc<AtomicUsize>,
) -> impl Iterator<Item = MatchCandidate> + '_ {
    candidates.iter().copied().inspect(move |_| {
        progress.fetch_add(1, Ordering::Relaxed);
    })
}

fn candidates_with_temp_observation<'a>(
    candidates: &'a [MatchCandidate],
    progress: Arc<AtomicUsize>,
    context: &'a ResourceContext,
    current: Arc<AtomicU64>,
    high_water: Arc<AtomicU64>,
) -> impl Iterator<Item = MatchCandidate> + 'a {
    candidates.iter().copied().inspect(move |_| {
        let earlier_emissions = progress.fetch_add(1, Ordering::Relaxed);
        if earlier_emissions > 0 {
            // `inspect` runs while the sink requests its next row.  Thus each
            // sample is taken after an earlier row was emitted, but before the
            // current row is yielded and before this iterator can be exhausted.
            current.fetch_max(context.temp.current(), Ordering::Relaxed);
            high_water.fetch_max(context.temp.high_water(), Ordering::Relaxed);
        }
    })
}

#[test]
fn larger_temp_budget_offloads_public_candidates_and_round_trips() {
    let temp_root = tempfile::tempdir().unwrap();
    let (input, candidates) = fixture();
    let config = config(temp_root.path(), 16 * 1024 * 1024);
    let context = ResourceContext::with_resources(&config.resources).unwrap();
    let progress = Arc::new(AtomicUsize::new(0));
    let candidate_phase_current = Arc::new(AtomicU64::new(0));
    let candidate_phase_high_water = Arc::new(AtomicU64::new(0));

    let mut archive = Vec::new();
    let stats = compress_with_candidates_with_context(
        Cursor::new(&input),
        &mut archive,
        &config,
        candidates_with_temp_observation(
            &candidates,
            progress.clone(),
            &context,
            candidate_phase_current.clone(),
            candidate_phase_high_water.clone(),
        ),
        &context,
    )
    .unwrap();

    assert!(stats.semantic_match_count > 0);
    assert!(!archive.is_empty());
    // These samples were taken from inside the public candidate iterator after
    // earlier rows were emitted, so they prove candidate-phase offload rather
    // than WIS, selected-match, or staged-writer temporary usage.
    assert!(candidate_phase_current.load(Ordering::Relaxed) > input.len() as u64 + 64 * 1024);
    assert!(candidate_phase_high_water.load(Ordering::Relaxed) > input.len() as u64 + 64 * 1024);
    assert!(context.memory.high_water() <= MEMORY_LIMIT);
    assert_eq!(progress.load(Ordering::Relaxed), candidates.len());
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);

    let mut restored = Vec::new();
    let decoded = decompress_with_context(
        Cursor::new(&archive),
        &mut restored,
        &config.resources,
        &context,
    )
    .unwrap();
    assert_eq!(decoded.original_size, input.len() as u64);
    assert_eq!(restored, input);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert_clean_temp_root(temp_root.path());
}

#[test]
fn insufficient_temp_budget_fails_during_candidate_offload_atomically() {
    let temp_root = tempfile::tempdir().unwrap();
    let (input, candidates) = fixture();
    // This admits the complete input spool, but not the first bounded external
    // candidate run.  It consequently tests the candidate-offload phase rather
    // than the initial input-spooling phase.
    let config = config(temp_root.path(), input.len() as u64 + 64 * 1024);
    let context = ResourceContext::with_resources(&config.resources).unwrap();
    let progress = Arc::new(AtomicUsize::new(0));

    let mut archive = Vec::new();
    let error = compress_with_candidates_with_context(
        Cursor::new(&input),
        &mut archive,
        &config,
        candidates_with_progress(&candidates, progress.clone()),
        &context,
    )
    .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::TempBudgetExceeded);
    assert!(archive.is_empty());
    assert!(
        context.temp.high_water() > input.len() as u64,
        "candidate offload admitted no temporary bytes beyond the input spool"
    );
    let consumed = progress.load(Ordering::Relaxed);
    assert!(consumed > 0, "candidate sink was never entered");
    assert!(
        consumed < candidates.len(),
        "candidate stream was fully consumed before failure"
    );
    assert!(context.temp.high_water() >= input.len() as u64);
    assert!(context.memory.high_water() <= MEMORY_LIMIT);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert_clean_temp_root(temp_root.path());
}
