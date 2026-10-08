//! Disk-backed weighted interval scheduling for validated offload candidates.
//!
//! The scheduler deliberately has no concrete selected-store implementation.  L1
//! supplies a [`SelectedBuilder`], while this module owns the bounded DP and
//! backtrace work tables.  In particular, a selected chain is never collected in
//! a Rust `Vec`: the reverse-index table is the only backtrace workspace.

use std::cmp::Ordering;

use super::contracts::{ReplayStore, Scheduler, SelectedBuilder, SelectedSummary, TableBuilder};
use super::normalize::{ScheduleInput, ValidatedCandidates};
use super::session::CompressionSession;
use super::storage::{DpRow, RecordPageCache, ReverseIndexRow, TypedTableBuilder};
use crate::error::{Error, Result};
use crate::match_ir::{Match, MatchCandidate};

const SENTINEL: u64 = u64::MAX;
const CACHE_PAGES: usize = 2;

/// A scheduler carrying the output builder supplied by the writer owner.
///
/// Keeping the builder as a type parameter means W1 does not need to invent a
/// selected-store representation before L1 has installed its implementation.
pub(crate) struct DiskScheduler<B> {
    builder: B,
}

impl<B> DiskScheduler<B> {
    pub(crate) fn new(builder: B) -> Self {
        Self { builder }
    }
}
impl<B: SelectedBuilder> DiskScheduler<B> {
    pub(crate) fn schedule_prepared(
        self,
        input: ScheduleInput,
        session: &CompressionSession,
    ) -> Result<B::Store> {
        schedule_input(input, session, self.builder)
    }
}

impl<B> Scheduler<ValidatedCandidates> for DiskScheduler<B>
where
    B: SelectedBuilder,
{
    type Selected = B::Store;

    fn schedule(
        self,
        validated: ValidatedCandidates,
        session: &CompressionSession,
    ) -> Result<Self::Selected> {
        let input = validated.into_schedule_input(session)?;
        schedule_input(input, session, self.builder)
    }
}

/// Run WIS over an already validated, end-ordered input table.
///
/// This function is also the small integration seam useful to L1 tests: the
/// caller owns construction of `B`, and the scheduler only calls `push` in
/// destination order followed by one checked `finish`.
fn schedule_input<B>(
    input: ScheduleInput,
    session: &CompressionSession,
    builder: B,
) -> Result<B::Store>
where
    B: SelectedBuilder,
{
    session.check_cancelled()?;
    let input_len = input.input_len();
    let count = input.len();
    validate_input_order(&input, input_len, session)?;

    // A zero-row schedule must still finish the output with the exact literal
    // accounting.  Do not create empty DP/reverse files in this case.
    if count == 0 {
        return builder.finish(zero_summary(input_len));
    }

    // These are deliberately separate reservations.  Sharing a single cache
    // would make candidate and live-DP eviction state interact and would also
    // make the low-memory bound dependent on access order.
    let pages = if session.memory().limit() >= 64 * 1024 * 1024 {
        64
    } else {
        CACHE_PAGES
    };
    let mut candidate_cache = RecordPageCache::new(session.memory(), pages)?;
    let mut dp_cache = RecordPageCache::new(session.memory(), pages)?;
    #[cfg(test)]
    tests::checkpoint(tests::Stage::DpCreate, session);
    let mut dp = TypedTableBuilder::<DpRow>::new(session, input_len)?;

    for index in 0..count {
        session.check_cancelled()?;
        let current = input.read_cached(index, &mut candidate_cache)?.0.candidate;
        validate_schedule_candidate(current, input_len)?;

        let predecessor =
            upper_bound_predecessor(&input, index, current.dst, &mut candidate_cache)?;
        if predecessor > dp.committed_len() {
            return Err(Error::corrupt_index(
                "schedule predecessor exceeds committed DP prefix",
            ));
        }

        let previous = if predecessor == 0 {
            DpState::zero()
        } else {
            let row = read_live_dp(&mut dp, predecessor - 1, &mut dp_cache, input_len)?;
            DpState::from_row(row)
        };
        let excluded = if index == 0 {
            DpState::zero()
        } else {
            #[cfg(test)]
            tests::checkpoint(tests::Stage::DpRead, session);
            let row = read_live_dp(&mut dp, index - 1, &mut dp_cache, input_len)?;
            DpState::from_row(row)
        };

        let include_depth = previous
            .count
            .checked_add(1)
            .ok_or_else(|| Error::corrupt_index("schedule chain depth overflow"))?;
        let include = DpState {
            gain: previous
                .gain
                .checked_add(candidate_gain(current)?)
                .ok_or_else(|| Error::invalid_match("schedule gain overflows"))?,
            covered: previous
                .covered
                .checked_add(current.len)
                .ok_or_else(|| Error::invalid_match("schedule coverage overflows"))?,
            count: include_depth,
            best_node: index,
            include_parent: previous.best_node,
            include_depth,
        };

        let choose_include = compare_states(
            &include,
            &excluded,
            (index, current),
            &input,
            &mut candidate_cache,
            &mut dp,
            &mut dp_cache,
        )? == Ordering::Less;
        let winner = if choose_include { include } else { excluded };
        // `include_parent`/`include_depth` describe candidate `index`, not
        // whichever state won the recurrence.  Backtrace reaches a selected
        // candidate through these fields even when later rows exclude it.
        #[cfg(test)]
        tests::checkpoint(tests::Stage::DpAppend, session);
        dp.append(DpRow {
            gain: winner.gain,
            covered: winner.covered,
            count: winner.count,
            best_node: winner.best_node,
            include_parent: include.include_parent,
            include_depth: include.include_depth,
        })?;
    }

    // Finish seals and uncached-verifies the DP table before any selected output
    // is handed to the caller.  A failure therefore cannot yield a partial
    // selected store.
    #[cfg(test)]
    tests::checkpoint(tests::Stage::DpFinish, session);
    let dp = dp.finish()?;
    let final_row = read_sealed_dp(&dp, count - 1, &mut dp_cache, input_len)?;
    validate_final_state(final_row, count)?;

    #[cfg(test)]
    tests::checkpoint(tests::Stage::ReverseCreate, session);
    let mut reverse = TypedTableBuilder::<ReverseIndexRow>::new(session, input_len)?;
    let mut remaining = final_row.count;
    let mut node = final_row.best_node;
    let mut prior_node = SENTINEL;

    while remaining != 0 {
        session.check_cancelled()?;
        if node == SENTINEL || node >= count || node >= prior_node {
            return Err(Error::corrupt_index(
                "schedule backpointer is not strictly decreasing",
            ));
        }
        let row = read_sealed_dp(&dp, node, &mut dp_cache, input_len)?;
        validate_dp_row(row, node, input_len)?;
        if row.best_node != node || row.include_depth != remaining {
            return Err(Error::corrupt_index("schedule backpointer depth mismatch"));
        }
        let parent = if row.include_parent == SENTINEL {
            DpState::zero()
        } else {
            DpState::from_row(read_sealed_dp(
                &dp,
                row.include_parent,
                &mut dp_cache,
                input_len,
            )?)
        };
        let candidate = input.read_cached(node, &mut candidate_cache)?.0.candidate;
        validate_extension(row, parent, candidate)?;
        #[cfg(test)]
        tests::checkpoint(tests::Stage::ReverseAppend, session);
        reverse.append(ReverseIndexRow {
            candidate_index: node,
        })?;
        prior_node = node;
        node = row.include_parent;
        remaining -= 1;
        if remaining != 0 && node == SENTINEL {
            return Err(Error::corrupt_index(
                "schedule chain ended before its recorded depth",
            ));
        }
    }
    if node != SENTINEL {
        return Err(Error::corrupt_index(
            "schedule chain has an extra backpointer",
        ));
    }

    #[cfg(test)]
    tests::checkpoint(tests::Stage::ReverseFinish, session);
    let reverse = reverse.finish()?;
    // The DP table is no longer needed. Reuse its separately reserved cache
    // for the reverse-index replay; the candidate cache remains independent.
    drop(dp);
    let mut output = builder;
    let mut reverse_position = reverse.len();
    let mut previous_end = 0u64;
    let mut have_previous = false;
    let mut covered = 0u64;
    let mut gain = 0u64;
    let mut origin = 0u64;

    while reverse_position != 0 {
        session.check_cancelled()?;
        reverse_position -= 1;
        #[cfg(test)]
        tests::checkpoint(tests::Stage::ReverseRead, session);
        let reverse_row = dp_cache.read(&reverse, reverse_position)?;
        let candidate = input
            .read_cached(reverse_row.candidate_index, &mut candidate_cache)?
            .0
            .candidate;
        validate_schedule_candidate(candidate, input_len)?;
        let end = candidate
            .dst
            .checked_add(candidate.len)
            .ok_or_else(|| Error::corrupt_index("selected interval end overflows"))?;
        if have_previous && candidate.dst < previous_end {
            return Err(Error::corrupt_index(
                "selected chain is not destination ordered",
            ));
        }
        covered = covered
            .checked_add(candidate.len)
            .ok_or_else(|| Error::corrupt_index("selected coverage overflows"))?;
        gain = gain
            .checked_add(candidate_gain(candidate)?)
            .ok_or_else(|| Error::corrupt_index("selected gain overflows"))?;
        output.push(Match {
            src: candidate.src,
            dst: candidate.dst,
            len: candidate.len,
            origin_match_id: origin,
        })?;
        origin = origin
            .checked_add(1)
            .ok_or_else(|| Error::memory_limit("selected match ID overflows"))?;
        previous_end = end;
        have_previous = true;
    }

    validate_reconstructed(final_row, origin, covered, gain)?;
    let literal = input_len
        .checked_sub(covered)
        .ok_or_else(|| Error::corrupt_index("selected coverage exceeds input"))?;
    output.finish(SelectedSummary {
        count: origin,
        covered_bytes: covered,
        literal_bytes: literal,
    })
}

fn zero_summary(input_len: u64) -> SelectedSummary {
    SelectedSummary {
        count: 0,
        covered_bytes: 0,
        literal_bytes: input_len,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DpState {
    gain: u64,
    covered: u64,
    count: u64,
    best_node: u64,
    include_parent: u64,
    include_depth: u64,
}

impl DpState {
    fn zero() -> Self {
        Self {
            gain: 0,
            covered: 0,
            count: 0,
            best_node: SENTINEL,
            include_parent: SENTINEL,
            include_depth: 0,
        }
    }

    fn from_row(row: DpRow) -> Self {
        Self {
            gain: row.gain,
            covered: row.covered,
            count: row.count,
            best_node: row.best_node,
            include_parent: row.include_parent,
            include_depth: row.include_depth,
        }
    }
}

fn candidate_gain(candidate: MatchCandidate) -> Result<u64> {
    candidate
        .len
        .checked_sub(25)
        .ok_or_else(|| Error::invalid_match("schedule candidate has nonpositive gain"))
}

fn validate_schedule_candidate(candidate: MatchCandidate, input_len: u64) -> Result<u64> {
    if candidate.len <= 25 || candidate.src >= candidate.dst {
        return Err(Error::corrupt_index("invalid row in scheduled candidates"));
    }
    if candidate.src >= input_len {
        return Err(Error::corrupt_index("scheduled source is outside input"));
    }
    let end = candidate
        .dst
        .checked_add(candidate.len)
        .ok_or_else(|| Error::corrupt_index("scheduled interval end overflows"))?;
    if end > input_len {
        return Err(Error::corrupt_index("scheduled interval exceeds input"));
    }
    let distance = candidate.dst - candidate.src;
    let source_span = candidate.len.min(distance);
    let source_end = candidate
        .src
        .checked_add(source_span)
        .ok_or_else(|| Error::corrupt_index("scheduled source span overflows"))?;
    if source_end > input_len {
        return Err(Error::corrupt_index("scheduled source span exceeds input"));
    }
    candidate_gain(candidate)?;
    Ok(end)
}

fn validate_input_order(
    input: &ScheduleInput,
    input_len: u64,
    session: &CompressionSession,
) -> Result<()> {
    let mut cursor = input.cursor()?;
    let mut previous = None;
    while let Some(row) = cursor.next()? {
        session.check_cancelled()?;
        let candidate = row.0.candidate;
        let end = validate_schedule_candidate(candidate, input_len)?;
        let key = (
            end,
            candidate.dst,
            candidate.src,
            std::cmp::Reverse(candidate.len),
            candidate.insertion_ordinal,
        );
        if previous.is_some_and(|old| old > key) {
            return Err(Error::corrupt_index("scheduled candidates are unordered"));
        }
        previous = Some(key);
    }
    Ok(())
}

fn upper_bound_predecessor(
    input: &ScheduleInput,
    limit: u64,
    destination: u64,
    cache: &mut RecordPageCache,
) -> Result<u64> {
    let mut low = 0u64;
    let mut high = limit;
    while low < high {
        let distance = high - low;
        let middle = low + distance / 2;
        let row = input.read_cached(middle, cache)?.0.candidate;
        let end = row
            .dst
            .checked_add(row.len)
            .ok_or_else(|| Error::corrupt_index("scheduled predecessor end overflows"))?;
        if end <= destination {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

fn read_live_dp(
    dp: &mut TypedTableBuilder<DpRow>,
    index: u64,
    cache: &mut RecordPageCache,
    input_len: u64,
) -> Result<DpRow> {
    // Let S1 enforce committed-prefix bounds: an attempted out-of-prefix live
    // read must poison the builder, including subsequent cache hits/finish.
    let row = dp.read_committed_cached(index, cache)?;
    validate_dp_row(row, index, input_len)?;
    if row.best_node != index {
        let selected = dp.read_committed_cached(row.best_node, cache)?;
        validate_dp_row(selected, row.best_node, input_len)?;
        validate_selected_node(row, selected)?;
    }
    Ok(row)
}

fn read_sealed_dp(
    dp: &super::storage::RecordTable<DpRow>,
    index: u64,
    cache: &mut RecordPageCache,
    input_len: u64,
) -> Result<DpRow> {
    if index >= dp.len() {
        return Err(Error::corrupt_index("sealed DP row is out of bounds"));
    }
    let row = cache.read(dp, index)?;
    validate_dp_row(row, index, input_len)?;
    if row.best_node != index {
        let selected = cache.read(dp, row.best_node)?;
        validate_dp_row(selected, row.best_node, input_len)?;
        validate_selected_node(row, selected)?;
    }
    Ok(row)
}

fn validate_dp_row(row: DpRow, index: u64, input_len: u64) -> Result<()> {
    validate_score(row.gain, row.covered, row.count)?;
    let max_depth = index
        .checked_add(1)
        .ok_or_else(|| Error::corrupt_index("DP row index overflows depth"))?;
    // Every stored row is a nonempty positive-gain prefix; the empty state
    // exists only in memory before the first candidate.
    if row.count == 0
        || row.covered > input_len
        || row.count > max_depth
        || row.include_depth == 0
        || row.include_depth > max_depth
        || (row.best_node != SENTINEL && row.best_node > index)
        || (row.include_parent != SENTINEL && row.include_parent >= index)
        || (row.count == 0 && row.best_node != SENTINEL)
        || (row.count != 0 && row.best_node == SENTINEL)
        || (row.include_depth == 1 && row.include_parent != SENTINEL)
        || (row.include_depth > 1 && row.include_parent == SENTINEL)
        || (row.best_node == index && row.include_depth != row.count)
    {
        return Err(Error::corrupt_index("malformed DP state link"));
    }
    Ok(())
}

fn validate_score(gain: u64, covered: u64, count: u64) -> Result<()> {
    let expected = u128::from(count)
        .checked_mul(25)
        .and_then(|cost| cost.checked_add(u128::from(gain)))
        .ok_or_else(|| Error::corrupt_index("DP score overflows"))?;
    if expected != u128::from(covered)
        || gain < count
        || (count == 0 && (gain != 0 || covered != 0))
    {
        return Err(Error::corrupt_index("inconsistent DP gain/coverage/count"));
    }
    Ok(())
}

fn validate_selected_node(row: DpRow, selected: DpRow) -> Result<()> {
    if selected.best_node != row.best_node
        || selected.include_depth != row.count
        || selected.count != row.count
        || selected.covered != row.covered
        || selected.gain != row.gain
    {
        return Err(Error::corrupt_index(
            "DP selected node differs from prefix score",
        ));
    }
    Ok(())
}

fn validate_extension(row: DpRow, parent: DpState, candidate: MatchCandidate) -> Result<()> {
    if parent.best_node != row.include_parent
        || parent.count.checked_add(1) != Some(row.count)
        || parent.covered.checked_add(candidate.len) != Some(row.covered)
        || candidate
            .len
            .checked_sub(25)
            .and_then(|gain| parent.gain.checked_add(gain))
            != Some(row.gain)
    {
        return Err(Error::corrupt_index(
            "DP backpointer candidate/score mismatch",
        ));
    }
    Ok(())
}

fn validate_reconstructed(row: DpRow, count: u64, covered: u64, gain: u64) -> Result<()> {
    if (count, covered, gain) != (row.count, row.covered, row.gain) {
        return Err(Error::corrupt_index(
            "selected chain differs from DP final score",
        ));
    }
    Ok(())
}

fn validate_final_state(row: DpRow, count: u64) -> Result<()> {
    validate_score(row.gain, row.covered, row.count)?;
    if row.count > count || (row.count == 0 && (row.best_node != SENTINEL || count != 0)) {
        return Err(Error::corrupt_index("malformed final DP state"));
    }
    if row.count != 0 && (row.best_node == SENTINEL || row.best_node >= count) {
        return Err(Error::corrupt_index("final DP tail is out of bounds"));
    }
    Ok(())
}

fn compare_states(
    include: &DpState,
    exclude: &DpState,
    current_node: (u64, MatchCandidate),
    input: &ScheduleInput,
    candidate_cache: &mut RecordPageCache,
    dp: &mut TypedTableBuilder<DpRow>,
    dp_cache: &mut RecordPageCache,
) -> Result<Ordering> {
    let (current_index, current) = current_node;
    validate_score(include.gain, include.covered, include.count)?;
    validate_score(exclude.gain, exclude.covered, exclude.count)?;
    match include.gain.cmp(&exclude.gain) {
        Ordering::Equal => {}
        order => {
            return Ok(if order == Ordering::Greater {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
    }
    match include.covered.cmp(&exclude.covered) {
        Ordering::Equal => {}
        order => {
            return Ok(if order == Ordering::Greater {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
    }
    match include.count.cmp(&exclude.count) {
        Ordering::Equal => {}
        order => return Ok(order),
    }

    if include.count != exclude.count {
        return Err(Error::corrupt_index(
            "DP tie comparator received unequal depths",
        ));
    }
    let order = compare_chains(
        Chain {
            tail: include.best_node,
            parent: include.include_parent,
            depth: include.include_depth,
            current: Some((current_index, current)),
        },
        Chain {
            tail: exclude.best_node,
            parent: SENTINEL,
            depth: exclude.count,
            current: None,
        },
        input,
        candidate_cache,
        dp,
        dp_cache,
        current_index,
    )?;
    // Equal chains choose exclude, as does the reference scheduler.
    Ok(order)
}

#[derive(Clone, Copy)]
struct Chain {
    tail: u64,
    parent: u64,
    depth: u64,
    current: Option<(u64, MatchCandidate)>,
}

/// Compare equal-score chains by walking both tails backwards in lockstep.
/// `difference` is overwritten on each reverse difference, so the last one is
/// the earliest forward difference.  No chain-sized allocation is needed.
fn compare_chains(
    mut a: Chain,
    mut b: Chain,
    input: &ScheduleInput,
    candidate_cache: &mut RecordPageCache,
    dp: &mut TypedTableBuilder<DpRow>,
    dp_cache: &mut RecordPageCache,
    current_index: u64,
) -> Result<Ordering> {
    let input_len = input.input_len();
    #[cfg(test)]
    tests::record_tie(a.depth);
    #[cfg(test)]
    let mut walked = 0usize;
    let mut difference = Ordering::Equal;
    while a.depth != 0 || b.depth != 0 {
        if a.depth == 0 || b.depth == 0 {
            return Err(Error::corrupt_index("DP chain depth ended inconsistently"));
        }
        if a.tail == b.tail {
            #[cfg(test)]
            tests::record_shared_prefix();
            // A node identifies its complete prefix; once tails meet, the
            // remaining reverse walk is shared and cannot change the result.
            break;
        }
        let a_candidate =
            chain_candidate(&a, input, candidate_cache, current_index, true, input_len)?;
        let b_candidate =
            chain_candidate(&b, input, candidate_cache, current_index, false, input_len)?;
        let order = chain_key(a_candidate).cmp(&chain_key(b_candidate));
        #[cfg(test)]
        {
            walked += 1;
            tests::record_comparison(a.depth, order);
        }
        if order != Ordering::Equal {
            difference = order;
        }
        a = chain_parent(&a, dp, dp_cache, input_len, current_index)?;
        b = chain_parent(&b, dp, dp_cache, input_len, current_index)?;
    }
    if a.depth != b.depth {
        return Err(Error::corrupt_index(
            "DP chain lengths differ after comparison",
        ));
    }
    #[cfg(test)]
    tests::record_walk(walked);
    Ok(difference)
}

fn chain_candidate(
    chain: &Chain,
    input: &ScheduleInput,
    cache: &mut RecordPageCache,
    current_index: u64,
    allow_current: bool,
    input_len: u64,
) -> Result<MatchCandidate> {
    if chain.tail == SENTINEL
        || (chain.tail >= current_index && !(allow_current && chain.tail == current_index))
    {
        return Err(Error::corrupt_index("DP chain node is out of bounds"));
    }
    let candidate = if let Some((index, candidate)) = chain.current {
        if index == chain.tail {
            candidate
        } else {
            input.read_cached(chain.tail, cache)?.0.candidate
        }
    } else {
        input.read_cached(chain.tail, cache)?.0.candidate
    };
    validate_schedule_candidate(candidate, input_len)?;
    Ok(candidate)
}

fn chain_parent(
    chain: &Chain,
    dp: &mut TypedTableBuilder<DpRow>,
    cache: &mut RecordPageCache,
    input_len: u64,
    current_index: u64,
) -> Result<Chain> {
    if chain.depth == 0 {
        return Err(Error::corrupt_index("cannot follow empty DP chain"));
    }
    let parent = if chain.tail == current_index {
        chain.parent
    } else {
        let row = read_live_dp(dp, chain.tail, cache, input_len)?;
        if row.best_node != chain.tail
            || row.count != chain.depth
            || row.include_depth != chain.depth
        {
            return Err(Error::corrupt_index("DP chain depth link changed"));
        }
        row.include_parent
    };
    let valid_parent = if chain.depth == 1 {
        parent == SENTINEL
    } else {
        parent != SENTINEL && parent < chain.tail
    };
    if !valid_parent {
        return Err(Error::corrupt_index("DP chain parent is not earlier"));
    }
    Ok(Chain {
        tail: parent,
        parent: SENTINEL,
        depth: chain.depth - 1,
        current: None,
    })
}

fn chain_key(candidate: MatchCandidate) -> (u64, u64, std::cmp::Reverse<u64>, u64) {
    (
        candidate.dst,
        candidate.src,
        std::cmp::Reverse(candidate.len),
        candidate.insertion_ordinal,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResourceConfig;
    use crate::error::ErrorKind;
    use crate::offload::candidate_sink::ExternalCandidateSink;
    use crate::offload::contracts::CandidateValidator;
    use crate::offload::contracts::{
        CandidateRow, CandidateRunSink, CandidateSink, RecordCursor, ReplayStore, SelectedBuilder,
        SelectedStore,
    };
    use crate::offload::normalize::Validator;
    use crate::offload::session::OffloadPolicy;
    use crate::offload::session::{FaultAction, IoLimit, Operation};
    use crate::resource::ResourceContext;
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Stage {
        DpCreate,
        DpAppend,
        DpRead,
        DpFinish,
        ReverseCreate,
        ReverseAppend,
        ReverseFinish,
        ReverseRead,
    }

    #[derive(Default)]
    struct Probe {
        fault: Option<(Stage, u64, Operation, FaultAction)>,
        stages: Vec<(Stage, u128)>,
        armed: bool,
        ties: Vec<u64>,
        differences: Vec<u64>,
        shared: usize,
        walks: Vec<usize>,
    }
    thread_local! {
        static PROBE: RefCell<Probe> = RefCell::new(Probe::default());
    }

    // W1-scoped adapter to B0's per-session injector. No global fault state,
    // sleeps, syscall-count guesses through V1, or changes to shared APIs.
    struct ScopedFaultInjector;
    impl ScopedFaultInjector {
        fn new(fault: Option<(Stage, u64, Operation, FaultAction)>) -> Self {
            PROBE.with(|p| {
                *p.borrow_mut() = Probe {
                    fault,
                    ..Probe::default()
                }
            });
            Self
        }
    }
    impl Drop for ScopedFaultInjector {
        fn drop(&mut self) {
            PROBE.with(|p| *p.borrow_mut() = Probe::default());
        }
    }
    pub(super) fn checkpoint(stage: Stage, session: &CompressionSession) {
        PROBE.with(|p| {
            let mut p = p.borrow_mut();
            p.stages.push((stage, session.io_used()));
            match p.fault.as_mut() {
                Some((wanted, remaining, operation, action)) if stage == *wanted => {
                    if *remaining == 0 {
                        session.inject(*operation, 0, *action);
                        p.fault = None;
                        p.armed = true;
                    } else {
                        *remaining -= 1;
                    }
                }
                _ => {}
            }
        });
    }
    pub(super) fn record_tie(depth: u64) {
        PROBE.with(|p| p.borrow_mut().ties.push(depth));
    }
    pub(super) fn record_shared_prefix() {
        PROBE.with(|p| p.borrow_mut().shared += 1);
    }
    pub(super) fn record_walk(walked: usize) {
        PROBE.with(|p| p.borrow_mut().walks.push(walked));
    }
    pub(super) fn record_comparison(depth: u64, order: Ordering) {
        if order != Ordering::Equal {
            PROBE.with(|p| p.borrow_mut().differences.push(depth));
        }
    }

    #[derive(Debug)]
    struct TestStore {
        rows: Vec<Match>,
        summary: SelectedSummary,
    }

    struct TestCursor<'a> {
        rows: &'a [Match],
        position: usize,
    }

    impl RecordCursor<Match> for TestCursor<'_> {
        fn position(&self) -> u64 {
            self.position as u64
        }

        fn seek_row(&mut self, position: u64) -> Result<()> {
            self.position = usize::try_from(position)
                .map_err(|_| Error::corrupt_index("test cursor index overflow"))?;
            if self.position > self.rows.len() {
                return Err(Error::corrupt_index("test cursor seek beyond end"));
            }
            Ok(())
        }

        fn next(&mut self) -> Result<Option<Match>> {
            let row = self.rows.get(self.position).copied();
            if row.is_some() {
                self.position += 1;
            }
            Ok(row)
        }
    }

    impl ReplayStore<Match> for TestStore {
        fn len(&self) -> u64 {
            self.rows.len() as u64
        }

        fn cursor(&self) -> Result<Box<dyn RecordCursor<Match> + '_>> {
            Ok(Box::new(TestCursor {
                rows: &self.rows,
                position: 0,
            }))
        }
    }

    impl SelectedStore for TestStore {
        fn summary(&self) -> SelectedSummary {
            self.summary
        }
    }

    struct TestBuilder {
        rows: Vec<Match>,
        input_len: u64,
    }

    impl SelectedBuilder for TestBuilder {
        type Store = TestStore;

        fn push(&mut self, item: Match) -> Result<()> {
            if item.origin_match_id != self.rows.len() as u64 {
                return Err(Error::corrupt_index("test selected ID is not sequential"));
            }
            if let Some(previous) = self.rows.last() {
                let previous_end = previous
                    .dst
                    .checked_add(previous.len)
                    .ok_or_else(|| Error::corrupt_index("test selected end overflows"))?;
                if item.dst < previous_end {
                    return Err(Error::corrupt_index("test selected rows overlap"));
                }
            }
            self.rows.push(item);
            Ok(())
        }

        fn finish(self, expected: SelectedSummary) -> Result<TestStore> {
            let covered = self.rows.iter().try_fold(0u64, |sum, row| {
                sum.checked_add(row.len)
                    .ok_or_else(|| Error::corrupt_index("test coverage overflow"))
            })?;
            let actual = SelectedSummary {
                count: self.rows.len() as u64,
                covered_bytes: covered,
                literal_bytes: self
                    .input_len
                    .checked_sub(covered)
                    .ok_or_else(|| Error::corrupt_index("test literal overflow"))?,
            };
            if actual.count != expected.count || actual.covered_bytes != expected.covered_bytes {
                return Err(Error::corrupt_index("test summary mismatch"));
            }
            Ok(TestStore {
                rows: self.rows,
                summary: expected,
            })
        }
    }

    fn fixture() -> (
        ResourceContext,
        CompressionSession,
        crate::codec::InputSpool,
    ) {
        fixture_with(8192, OffloadPolicy::default())
    }

    fn fixture_with(
        input_len: usize,
        policy: OffloadPolicy,
    ) -> (
        ResourceContext,
        CompressionSession,
        crate::codec::InputSpool,
    ) {
        let resources = ResourceConfig {
            memory: 32 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let context = ResourceContext::with_resources(&resources).unwrap();
        let session = CompressionSession::new(&context, &resources, policy).unwrap();
        let spool =
            crate::codec::spool_input(Cursor::new(vec![0u8; input_len]), &resources, &context)
                .unwrap();
        (context, session, spool)
    }

    fn candidate(src: u64, dst: u64, len: u64, ordinal: u64) -> MatchCandidate {
        MatchCandidate {
            src,
            dst,
            len,
            insertion_ordinal: ordinal,
        }
    }

    fn schedule(rows: &[MatchCandidate]) -> (TestStore, ResourceContext, CompressionSession) {
        let (context, session, spool) = fixture();
        let validated = prepare(rows, &session, &spool);
        let selected = DiskScheduler::new(TestBuilder {
            rows: Vec::new(),
            input_len: spool.len,
        })
        .schedule(validated, &session)
        .unwrap();
        (selected, context, session)
    }

    fn prepare(
        rows: &[MatchCandidate],
        session: &CompressionSession,
        spool: &crate::codec::InputSpool,
    ) -> ValidatedCandidates {
        let mut sink = ExternalCandidateSink::with_input_len(session, spool.len, 3).unwrap();
        for &row in rows {
            sink.emit(CandidateRow {
                candidate: row,
                provenance: super::super::contracts::Provenance::EXTERNAL,
            })
            .unwrap();
        }
        let source = sink.finish().unwrap();
        Validator::new(spool, 26).validate(source, session).unwrap()
    }

    use super::chain_key;

    #[test]
    fn forward_chain_key_matches_wire_tie_order() {
        let a = MatchCandidate {
            src: 4,
            dst: 20,
            len: 31,
            insertion_ordinal: 8,
        };
        let b = MatchCandidate {
            src: 4,
            dst: 20,
            len: 30,
            insertion_ordinal: 1,
        };
        assert!(chain_key(a) < chain_key(b));
    }

    #[test]
    fn recurrence_prefers_gain_and_emits_destination_order() {
        let rows = [
            candidate(0, 100, 60, 0),
            candidate(0, 160, 60, 1),
            candidate(0, 100, 80, 2),
            candidate(0, 180, 60, 3),
        ];
        let (selected, context, session) = schedule(&rows);
        assert_eq!(selected.rows.len(), 2);
        assert_eq!(
            selected.rows,
            vec![
                Match {
                    src: 0,
                    dst: 100,
                    len: 80,
                    origin_match_id: 0,
                },
                Match {
                    src: 0,
                    dst: 180,
                    len: 60,
                    origin_match_id: 1,
                },
            ]
        );
        assert_eq!(selected.summary.covered_bytes, 140);
        assert_eq!(selected.summary.literal_bytes, 8052);
        drop(session);
        assert_eq!(context.temp.current(), 0);
    }

    #[test]
    fn canonical_interval_representative_uses_smallest_source() {
        let rows = [
            candidate(10, 100, 40, 0),
            candidate(20, 100, 40, 1),
            candidate(0, 140, 40, 2),
            candidate(0, 180, 40, 3),
        ];
        let (selected, context, session) = schedule(&rows);
        assert_eq!(selected.rows[0].src, 10);
        assert_eq!(selected.rows[0].origin_match_id, 0);
        drop(session);
        assert_eq!(context.temp.current(), 0);
    }

    #[test]
    fn exhaustive_small_subsets_match_bruteforce_objective() {
        // All input subsets of a universe containing nested, overlapping,
        // end-aligned and adjacent intervals; for EACH input enumerate all
        // selected subsets independently (not another DP implementation).
        let universe = [
            candidate(0, 100, 60, 0),
            candidate(1, 100, 30, 1),
            candidate(2, 130, 30, 2),
            candidate(3, 110, 50, 3),
            candidate(4, 160, 26, 4),
            candidate(5, 140, 46, 5),
        ];
        for mask in 0..1usize << universe.len() {
            let rows: Vec<_> = universe
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, c)| *c)
                .collect();
            assert_oracles(&rows);
        }
        let mut seed = 0x9e37_79b9u64;
        for _ in 0..96 {
            let mut rows = Vec::new();
            for ordinal in 0..8 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let dst = 100 + (seed >> 32) % 100;
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                rows.push(candidate(seed % 10, dst, 26 + (seed >> 32) % 70, ordinal));
            }
            assert_oracles(&rows);
        }
    }

    // Deliberately independent of chain_key, DpState and the recurrence. Empty
    // schedule is a genuine subset. Canonicalization is not needed: enumeration
    // over unreduced alternatives yields the same complete-list optimum.
    fn brute_force(rows: &[MatchCandidate]) -> Vec<Match> {
        assert!(rows.len() <= 16);
        let key =
            |c: &MatchCandidate| (c.dst, c.src, std::cmp::Reverse(c.len), c.insertion_ordinal);
        let mut best = Vec::<MatchCandidate>::new();
        let mut best_score = (0u128, 0u128, std::cmp::Reverse(0usize));
        for mask in 0..1usize << rows.len() {
            let mut chain: Vec<_> = rows
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, c)| *c)
                .collect();
            chain.sort_unstable_by_key(key);
            if chain.iter().any(|c| c.len <= 25)
                || chain.windows(2).any(|w| w[0].dst + w[0].len > w[1].dst)
            {
                continue;
            }
            let covered: u128 = chain.iter().map(|c| u128::from(c.len)).sum();
            let gain: u128 = chain.iter().map(|c| u128::from(c.len) - 25).sum();
            let score = (gain, covered, std::cmp::Reverse(chain.len()));
            if score > best_score
                || (score == best_score
                    && chain.iter().map(key).cmp(best.iter().map(key)) == Ordering::Less)
            {
                best_score = score;
                best = chain;
            }
        }
        best.into_iter()
            .enumerate()
            .map(|(i, c)| Match {
                src: c.src,
                dst: c.dst,
                len: c.len,
                origin_match_id: i as u64,
            })
            .collect()
    }

    fn assert_oracles(rows: &[MatchCandidate]) -> TestStore {
        let expected = brute_force(rows);
        let ram = crate::match_ir::normalize_matches(rows.iter().copied(), 8192, 26).unwrap();
        let (selected, context, session) = schedule(rows);
        assert_eq!(selected.rows, expected, "candidates: {rows:?}");
        assert_eq!(selected.rows, ram.as_slice());
        let covered: u64 = expected.iter().map(|m| m.len).sum();
        assert_eq!(selected.summary.count, expected.len() as u64);
        assert_eq!(selected.summary.covered_bytes, covered);
        assert_eq!(selected.summary.literal_bytes, 8192 - covered);
        assert_eq!(ram.covered_bytes, covered);
        assert_eq!(ram.literal_bytes, 8192 - covered);
        assert_eq!(session.file_usage().current, 0);
        assert_eq!(session.handle_usage().current, 7);
        assert_eq!(session.io_reserved(), 0);
        drop(session);
        assert_eq!(context.temp.current(), 0);
        selected
    }

    #[test]
    fn equal_gain_prefers_coverage_not_fewer_records() {
        // One 55-byte match and two adjacent 40-byte matches both gain30.
        // Coverage80 wins despite count2. Once gain AND coverage tie, count
        // MUST tie too: covered = gain +25*count. No impossible third-level
        // scheduling witness is asserted; inconsistent scores are rejected below.
        let selected = assert_oracles(&[
            candidate(0, 100, 55, 0),
            candidate(0, 100, 40, 1),
            candidate(0, 140, 40, 2),
        ]);
        assert_eq!(selected.summary.count, 2);
        assert_eq!(selected.summary.covered_bytes, 80);
    }

    #[test]
    fn full_chain_ties_survive_v1_and_walk_early_middle_late_differences() {
        for prefix in [0, 3, 6] {
            let _probe = ScopedFaultInjector::new(None);
            let mut rows: Vec<_> = (0..prefix)
                .map(|i| candidate(0, 100 + i * 40, 40, i))
                .collect();
            let start = 100 + prefix * 40;
            // Two complete nonshared two-node branches, same coverage80/gain30.
            // Reverse walk first sees dst+50 vs dst+40, but the EARLIER node
            // (same dst/src, ReverseLen50 vs40) reverses that ordering.
            rows.extend([
                candidate(0, start, 40, 10),
                candidate(0, start + 40, 40, 11),
                candidate(0, start, 50, 12),
                candidate(0, start + 50, 30, 13),
            ]);
            for i in 0..(6 - prefix) {
                rows.push(candidate(0, start + 80 + i * 40, 40, 20 + i));
            }
            let selected = assert_oracles(&rows);
            assert_eq!(selected.rows[prefix as usize].len, 50);
            PROBE.with(|p| {
                let p = p.borrow();
                assert!(
                    p.ties.contains(&(prefix + 2)),
                    "full tie comparator was bypassed"
                );
                assert!(p.differences.contains(&(prefix + 1)));
                assert!(p.differences.contains(&(prefix + 2)));
                if prefix != 0 {
                    assert!(p.shared > 0);
                }
            });
        }
        // Destination first, then source BEFORE ReverseLen for different-length
        // same-start branches. The alternatives are NOT identical intervals.
        for (short_src, long_src, expected_len) in [(1, 2, 40), (2, 1, 50)] {
            let _probe = ScopedFaultInjector::new(None);
            let selected = assert_oracles(&[
                candidate(short_src, 100, 40, 0),
                candidate(0, 140, 40, 1),
                candidate(long_src, 100, 50, 2),
                candidate(0, 150, 30, 3),
            ]);
            assert_eq!(selected.rows[0].len, expected_len);
            PROBE.with(|p| assert!(!p.borrow().ties.is_empty()));
        }
        let _probe = ScopedFaultInjector::new(None);
        assert_oracles(&[candidate(0, 100, 40, 0), candidate(0, 101, 40, 1)]);
        PROBE.with(|p| assert!(!p.borrow().ties.is_empty()));
    }

    #[test]
    fn v1_reduces_source_and_ordinal_alternatives_before_wis() {
        let (_, session, spool) = fixture();
        let rows = [
            candidate(2, 100, 40, 0),
            candidate(1, 100, 40, 9),
            candidate(1, 100, 40, 3),
        ];
        let input = prepare(&rows, &session, &spool)
            .into_schedule_input(&session)
            .unwrap();
        let mut cursor = input.cursor().unwrap();
        assert_eq!(
            cursor.next().unwrap().unwrap().0.candidate,
            candidate(1, 100, 40, 3)
        );
        assert!(cursor.next().unwrap().is_none());
        // Ordinal remains in the full tuple, but identical dst/src/len cannot
        // survive V1 as two scheduler nodes. Do not claim that as a WIS tie.
        assert!(chain_key(candidate(1, 100, 40, 3)) < chain_key(candidate(1, 100, 40, 9)));
    }

    #[test]
    fn long_nonshared_chains_compare_every_node_before_shared_prefix() {
        for prefix in [0, 3] {
            let _probe = ScopedFaultInjector::new(None);
            let mut rows: Vec<_> = (0..prefix)
                .map(|i| candidate(0, 100 + i * 40, 40, i))
                .collect();
            let start = 100 + prefix * 40;
            // Both branches have six nodes, coverage240/gain90. B is ahead
            // by ten bytes until its last node, so both prefixes survive the
            // recurrence and the final tie must walk SIX nonshared nodes.
            for i in 0..6 {
                rows.push(candidate(0, start + i * 40, 40, 10 + i));
                let (dst, len) = if i == 0 {
                    (start, 50)
                } else if i == 5 {
                    (start + 210, 30)
                } else {
                    (start + 10 + i * 40, 40)
                };
                rows.push(candidate(0, dst, len, 20 + i));
            }
            let selected = assert_oracles(&rows);
            assert_eq!(selected.rows[prefix as usize].len, 50);
            PROBE.with(|p| {
                let p = p.borrow();
                assert!(p.ties.contains(&(prefix + 6)));
                assert!(p.walks.contains(&6), "must not shortcut on the tail");
                if prefix > 0 {
                    assert!(p.shared > 0);
                }
            });
        }
    }

    fn good_row() -> DpRow {
        DpRow {
            gain: 1,
            covered: 26,
            count: 1,
            best_node: 0,
            include_parent: SENTINEL,
            include_depth: 1,
        }
    }

    fn corrupt(result: Result<()>) {
        assert_eq!(result.unwrap_err().kind(), ErrorKind::CorruptIndex);
    }

    #[test]
    fn malformed_typed_dp_states_fail_closed_without_checksum_forgery() {
        let good = good_row();
        validate_dp_row(good, 0, 26).unwrap();
        validate_score(0, 0, 0).unwrap();
        for row in [
            DpRow {
                count: 0,
                best_node: SENTINEL,
                ..good
            },
            DpRow {
                count: 0,
                gain: 0,
                covered: 0,
                best_node: SENTINEL,
                ..good
            },
            DpRow {
                gain: 0,
                covered: 25,
                ..good
            },
            DpRow { gain: 2, ..good },
            DpRow {
                covered: 27,
                ..good
            },
            DpRow {
                count: u64::MAX,
                ..good
            },
            DpRow {
                gain: u64::MAX,
                covered: u64::MAX,
                ..good
            },
            DpRow {
                best_node: SENTINEL,
                ..good
            },
            DpRow {
                best_node: 1,
                ..good
            },
            DpRow {
                include_parent: 0,
                ..good
            }, // self-cycle/root mismatch
            DpRow {
                include_parent: 1,
                ..good
            }, // forward link
            DpRow {
                include_depth: 0,
                ..good
            },
            DpRow {
                include_depth: 2,
                ..good
            },
        ] {
            corrupt(validate_dp_row(row, 0, u64::MAX));
        }
        corrupt(validate_dp_row(good, u64::MAX, u64::MAX));
        corrupt(validate_dp_row(good, 0, 25));
        corrupt(validate_dp_row(
            DpRow {
                include_parent: 0,
                include_depth: 2,
                best_node: 1,
                ..good
            },
            1,
            100,
        ));
        corrupt(validate_dp_row(
            DpRow {
                include_depth: 2,
                ..good
            },
            1,
            100,
        ));
        corrupt(validate_score(1, 26, u64::MAX));
        corrupt(validate_score(1, 26, 2)); // impossible "count-only" objective tie
        validate_score(u64::MAX - 25, u64::MAX, 1).unwrap();
        corrupt(validate_score(u64::MAX - 24, u64::MAX, 1));
        validate_selected_node(good, good).unwrap();
        for selected in [
            DpRow {
                include_depth: 2,
                ..good
            },
            DpRow {
                best_node: 1,
                ..good
            },
            DpRow { count: 2, ..good },
            DpRow {
                covered: 27,
                ..good
            },
            DpRow { gain: 2, ..good },
        ] {
            corrupt(validate_selected_node(good, selected));
        }
        validate_extension(good, DpState::zero(), candidate(0, 100, 26, 0)).unwrap();
        corrupt(validate_extension(
            good,
            DpState::zero(),
            candidate(0, 100, 27, 0),
        ));
        corrupt(validate_extension(
            good,
            DpState {
                count: u64::MAX,
                ..DpState::zero()
            },
            candidate(0, 100, 26, 0),
        ));
        corrupt(validate_extension(
            good,
            DpState {
                gain: u64::MAX,
                ..DpState::zero()
            },
            candidate(0, 100, 26, 0),
        ));
        corrupt(validate_extension(
            good,
            DpState {
                best_node: 0,
                ..DpState::zero()
            },
            candidate(0, 100, 26, 0),
        ));
        validate_reconstructed(good, 1, 26, 1).unwrap();
        corrupt(validate_reconstructed(good, 1, 27, 1));
        corrupt(validate_reconstructed(good, 1, 26, 2));
        corrupt(validate_reconstructed(good, 2, 26, 1));
    }

    #[test]
    fn typed_table_selected_node_and_chain_link_mismatches_are_rejected() {
        let (_, session, _) = fixture();
        let mut cache = RecordPageCache::new(session.memory(), CACHE_PAGES).unwrap();
        let mut dp = TypedTableBuilder::<DpRow>::new(&session, 8192).unwrap();
        dp.append(good_row()).unwrap();
        // Valid local links and arithmetic, but a lying prefix score. The
        // ordinary S1 append computes genuine checksums for these typed rows.
        dp.append(DpRow {
            gain: 2,
            covered: 27,
            ..good_row()
        })
        .unwrap();
        assert_eq!(
            read_live_dp(&mut dp, 1, &mut cache, 8192)
                .unwrap_err()
                .kind(),
            ErrorKind::CorruptIndex
        );
        let chain = Chain {
            tail: 0,
            parent: SENTINEL,
            depth: 2,
            current: None,
        };
        assert_eq!(
            chain_parent(&chain, &mut dp, &mut cache, 8192, 2)
                .err()
                .unwrap()
                .kind(),
            ErrorKind::CorruptIndex
        );
        let dp = dp.finish().unwrap();
        assert_eq!(
            read_sealed_dp(&dp, 1, &mut cache, 8192).unwrap_err().kind(),
            ErrorKind::CorruptIndex
        );
    }

    #[test]
    fn live_dp_out_of_prefix_and_io_failure_remain_poisoned_on_cache_hits() {
        use super::super::storage::FixedRecord;
        assert_eq!(DpRow::WIDTH, 96);
        for io_fault in [false, true] {
            let (_, session, _) = fixture();
            let mut cache = RecordPageCache::new(session.memory(), CACHE_PAGES).unwrap();
            let mut dp = TypedTableBuilder::<DpRow>::new(&session, 8192).unwrap();
            dp.append(good_row()).unwrap();
            read_live_dp(&mut dp, 0, &mut cache, 8192).unwrap();
            if io_fault {
                // Appending changes the committed-prefix cache identity.
                dp.append(good_row()).unwrap();
                session.inject(Operation::Read, 0, FaultAction::Fail);
            }
            let error = read_live_dp(&mut dp, 1, &mut cache, 8192).unwrap_err();
            assert_eq!(
                error.kind(),
                if io_fault {
                    ErrorKind::TemporaryStorageFailure
                } else {
                    ErrorKind::CorruptIndex
                }
            );
            assert!(read_live_dp(&mut dp, 0, &mut cache, 8192).is_err());
            assert!(dp.append(good_row()).is_err());
            assert!(dp.finish().is_err());
            assert_eq!(session.io_reserved(), 0);
            assert_eq!(session.file_usage().current, 0);
        }
    }

    #[derive(Default, Debug)]
    struct Calls {
        pushes: usize,
        finishes: usize,
    }
    struct FailingBuilder {
        calls: Rc<RefCell<Calls>>,
        fail_push: Option<usize>,
        fail_finish: bool,
    }
    impl SelectedBuilder for FailingBuilder {
        type Store = TestStore;
        fn push(&mut self, _: Match) -> Result<()> {
            let mut calls = self.calls.borrow_mut();
            calls.pushes += 1;
            if self.fail_push == Some(calls.pushes) {
                return Err(Error::new(ErrorKind::OutputIo, "W1 selected push marker"));
            }
            Ok(())
        }
        fn finish(self, summary: SelectedSummary) -> Result<TestStore> {
            self.calls.borrow_mut().finishes += 1;
            if self.fail_finish {
                return Err(Error::new(ErrorKind::OutputIo, "W1 selected finish marker"));
            }
            Ok(TestStore {
                rows: Vec::new(),
                summary,
            })
        }
    }

    fn fault_rows() -> [MatchCandidate; 4] {
        [
            candidate(0, 100, 40, 0),
            candidate(0, 101, 40, 1),
            candidate(0, 150, 40, 2),
            candidate(0, 200, 40, 3),
        ]
    }

    // Baseline includes the real spool and an unrelated sealed file owned by
    // this test, but NOT consumed V1 input. This catches both leaking W1/V1
    // files and accidentally releasing somebody else's ownership.
    #[derive(Debug, PartialEq, Eq)]
    struct Usage {
        memory: u64,
        temp: u64,
        files: u64,
        handles: u64,
        paths: usize,
    }
    fn usage(session: &CompressionSession) -> Usage {
        // The persistent directory must exist BEFORE any usage field is
        // sampled: temp_directory() admits and charges the directory owner,
        // so sampling memory.current() first would produce a baseline that
        // excludes the directory charge and break exact post-run equality.
        let paths = std::fs::read_dir(&*session.temp_directory().unwrap())
            .unwrap()
            .count();
        Usage {
            memory: session.memory().current(),
            temp: session.temp().current(),
            files: session.file_usage().current,
            handles: session.handle_usage().current,
            paths,
        }
    }
    fn before_preparation(session: &CompressionSession) -> Usage {
        let mut baseline = usage(session);
        // CandidateSink takes (rather than borrows) this pre-admitted workspace.
        // Its charge belongs to the consumed V1 input thereafter, not session.
        baseline.memory -= session
            .workspace(super::super::session::WorkspaceKind::Candidate)
            .unwrap()
            .len() as u64;
        baseline
    }
    fn retained(
        session: &CompressionSession,
    ) -> super::super::storage::RecordTable<ReverseIndexRow> {
        let mut b = TypedTableBuilder::new(session, 8192).unwrap();
        b.append(ReverseIndexRow {
            candidate_index: 123,
        })
        .unwrap();
        b.finish().unwrap()
    }

    #[test]
    fn w1_boundary_io_faults_and_midstage_cancellation_release_consumed_input() {
        for (stage, occurrence, operation) in [
            (Stage::DpCreate, 0, Operation::Create),
            (Stage::DpCreate, 0, Operation::Write),
            (Stage::DpAppend, 0, Operation::Write),
            (Stage::DpAppend, 2, Operation::Write),
            (Stage::DpRead, 0, Operation::Read),
            (Stage::DpFinish, 0, Operation::Flush),
            (Stage::DpFinish, 0, Operation::Read),
            (Stage::ReverseCreate, 0, Operation::Create),
            (Stage::ReverseCreate, 0, Operation::Write),
            (Stage::ReverseAppend, 0, Operation::Write),
            (Stage::ReverseAppend, 1, Operation::Write),
            (Stage::ReverseFinish, 0, Operation::Flush),
            (Stage::ReverseFinish, 0, Operation::Read),
            (Stage::ReverseRead, 0, Operation::Read),
        ] {
            for action in [FaultAction::Fail, FaultAction::Cancel] {
                let (context, session, spool) = fixture();
                let unrelated = retained(&session);
                let baseline = before_preparation(&session);
                let input = prepare(&fault_rows(), &session, &spool)
                    .into_schedule_input(&session)
                    .unwrap();
                assert!(session.file_usage().current > baseline.files);
                let before_io = session.io_used();
                let _scope = ScopedFaultInjector::new(Some((stage, occurrence, operation, action)));
                let calls = Rc::new(RefCell::new(Calls::default()));
                let error = schedule_input(
                    input,
                    &session,
                    FailingBuilder {
                        calls: calls.clone(),
                        fail_push: None,
                        fail_finish: false,
                    },
                )
                .unwrap_err();
                assert_eq!(
                    error.kind(),
                    ErrorKind::TemporaryStorageFailure,
                    "{stage:?}/{action:?}: {error}"
                );
                PROBE.with(|p| {
                    let p = p.borrow();
                    assert!(p.armed, "unreached {stage:?}");
                    let last = p.stages.last().unwrap();
                    assert_eq!(last.0, stage, "fault escaped named boundary");
                    assert!(last.1 >= before_io);
                });
                assert_eq!(calls.borrow().finishes, 0);
                assert_eq!(session.io_reserved(), 0);
                assert!(session.io_used() >= before_io); // successful I/O is never refunded
                assert_eq!(usage(&session), baseline, "{stage:?}/{action:?}");
                drop(unrelated);
                drop(spool);
                drop(session);
                assert_eq!(context.temp.current(), 0);
                assert_eq!(context.memory.current(), 0);
            }
        }
    }

    #[test]
    fn selected_builder_errors_propagate_exactly_without_finishing_after_push_error() {
        for fail_push in [Some(1), Some(2), None] {
            let (_, session, spool) = fixture();
            let unrelated = retained(&session);
            let baseline = before_preparation(&session);
            let input = prepare(&fault_rows(), &session, &spool);
            let calls = Rc::new(RefCell::new(Calls::default()));
            let error = DiskScheduler::new(FailingBuilder {
                calls: calls.clone(),
                fail_push,
                fail_finish: fail_push.is_none(),
            })
            .schedule(input, &session)
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::OutputIo);
            assert_eq!(
                error.context(),
                Some(if fail_push.is_some() {
                    "W1 selected push marker"
                } else {
                    "W1 selected finish marker"
                })
            );
            assert_eq!(calls.borrow().pushes, fail_push.unwrap_or(3));
            assert_eq!(calls.borrow().finishes, usize::from(fail_push.is_none()));
            assert_eq!(usage(&session), baseline);
            assert_eq!(session.io_reserved(), 0);
            drop(unrelated);
        }
    }

    #[test]
    fn cancellation_before_w1_and_empty_input_semantics() {
        for input_len in [0, 8192] {
            let (context, session, spool) = fixture_with(input_len, OffloadPolicy::default());
            let baseline = before_preparation(&session);
            let input = prepare(&[], &session, &spool);
            let selected = DiskScheduler::new(TestBuilder {
                rows: Vec::new(),
                input_len: spool.len,
            })
            .schedule(input, &session)
            .unwrap();
            assert!(selected.rows.is_empty());
            assert_eq!(selected.summary.count, 0);
            assert_eq!(selected.summary.covered_bytes, 0);
            assert_eq!(selected.summary.literal_bytes, input_len as u64);
            assert_eq!(usage(&session), baseline);
            // The persistent directory exists (created by the baseline
            // sampling) and is removed with zero memory after session drop.
            let dir = session.temp_directory().unwrap().to_path_buf();
            assert!(dir.exists());
            drop(spool);
            drop(session);
            assert!(!dir.exists());
            assert_eq!(context.memory.current(), 0);
            assert_eq!(context.temp.current(), 0);
        }
        for through_trait in [false, true] {
            let (context, session, spool) = fixture();
            let baseline = before_preparation(&session);
            let input = prepare(&fault_rows(), &session, &spool);
            let calls = Rc::new(RefCell::new(Calls::default()));
            let builder = FailingBuilder {
                calls: calls.clone(),
                fail_push: None,
                fail_finish: false,
            };
            let result = if through_trait {
                session.cancel();
                DiskScheduler::new(builder).schedule(input, &session)
            } else {
                let input = input.into_schedule_input(&session).unwrap();
                session.cancel();
                schedule_input(input, &session, builder)
            };
            assert_eq!(
                result.unwrap_err().kind(),
                ErrorKind::TemporaryStorageFailure
            );
            assert_eq!(calls.borrow().pushes, 0);
            assert_eq!(calls.borrow().finishes, 0);
            assert_eq!(session.io_reserved(), 0);
            assert_eq!(usage(&session), baseline);
            let dir = session.temp_directory().unwrap().to_path_buf();
            assert!(dir.exists());
            drop(spool);
            drop(session);
            assert!(!dir.exists());
            assert_eq!(context.memory.current(), 0);
            assert_eq!(context.temp.current(), 0);
        }
    }

    #[test]
    fn w1_resource_admission_failures_keep_unrelated_reservations() {
        for kind in ["ram", "temp", "handles", "files", "io-zero", "io-finite"] {
            let policy = OffloadPolicy {
                io_limit: IoLimit::Finite(10_000_000),
                ..OffloadPolicy::default()
            };
            let (_, session, spool) = fixture_with(8192, policy);
            let unrelated = retained(&session);
            let baseline = before_preparation(&session);
            let input = prepare(&fault_rows(), &session, &spool)
                .into_schedule_input(&session)
                .unwrap();
            let mut memory = None;
            let mut temp = None;
            let mut handles = None;
            let mut files = Vec::new();
            let mut io = None;
            let expected = match kind {
                "ram" => {
                    memory = Some(
                        session
                            .memory()
                            .reserve(session.memory().limit() - session.memory().current())
                            .unwrap(),
                    );
                    ErrorKind::MemoryBudgetExceeded
                }
                "temp" => {
                    temp = Some(
                        session
                            .temp()
                            .reserve(session.temp().limit() - session.temp().current())
                            .unwrap(),
                    );
                    ErrorKind::TempBudgetExceeded
                }
                "handles" => {
                    handles = Some(
                        session
                            .acquire_handles(policy.max_handles - session.handle_usage().current)
                            .unwrap(),
                    );
                    ErrorKind::TemporaryStorageFailure
                }
                "files" => {
                    while session.file_usage().current < policy.max_temp_files {
                        files.push(session.acquire_file_slot().unwrap());
                    }
                    ErrorKind::TemporaryStorageFailure
                }
                _ => {
                    let available = if kind == "io-zero" { 0 } else { 512 };
                    io = Some(
                        session
                            .reserve_io(10_000_000 - session.io_used() - available)
                            .unwrap(),
                    );
                    ErrorKind::TemporaryStorageFailure
                }
            };
            let reserved_io = session.io_reserved();
            let calls = Rc::new(RefCell::new(Calls::default()));
            let before_io = session.io_used();
            let error = schedule_input(
                input,
                &session,
                FailingBuilder {
                    calls: calls.clone(),
                    fail_push: None,
                    fail_finish: false,
                },
            )
            .unwrap_err();
            assert_eq!(error.kind(), expected, "{kind}: {error}");
            assert_eq!(calls.borrow().finishes, 0);
            assert_eq!(session.io_reserved(), reserved_io);
            assert!(session.io_used() >= before_io);
            drop((memory, temp, handles, files, io));
            assert_eq!(session.io_reserved(), 0);
            assert_eq!(usage(&session), baseline, "{kind}");
            drop(unrelated);
        }
        // An actually zero-total-I/O policy cannot prepare V1; it fails before
        // W1, unlike the exhausted remaining allowance tested above.
        let (_, session, spool) = fixture_with(
            0,
            OffloadPolicy {
                io_limit: IoLimit::Finite(0),
                ..OffloadPolicy::default()
            },
        );
        let input = prepare(&[], &session, &spool);
        assert_eq!(
            DiskScheduler::new(TestBuilder {
                rows: Vec::new(),
                input_len: 0
            })
            .schedule(input, &session)
            .unwrap_err()
            .kind(),
            ErrorKind::TemporaryStorageFailure
        );
        assert_eq!(session.io_used(), 0);
        assert_eq!(session.io_reserved(), 0);
        assert_eq!(session.file_usage().current, 0);
    }

    #[test]
    fn injected_delete_failure_retains_charge_until_explicit_cleanup() {
        let (_, session, spool) = fixture();
        let unrelated = retained(&session);
        let baseline = before_preparation(&session);
        let directory = session.temp_directory().unwrap().to_path_buf();
        let unrelated_paths: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|p| p.unwrap().path())
            .collect();
        let input = prepare(&fault_rows(), &session, &spool)
            .into_schedule_input(&session)
            .unwrap();
        let _scope = ScopedFaultInjector::new(Some((
            Stage::DpAppend,
            0,
            Operation::Write,
            FaultAction::Fail,
        )));
        session.inject(Operation::Remove, 0, FaultAction::Fail);
        let error = schedule_input(
            input,
            &session,
            TestBuilder {
                rows: Vec::new(),
                input_len: spool.len,
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TemporaryStorageFailure);
        let retained_usage = usage(&session);
        assert_eq!(retained_usage.handles, baseline.handles);
        assert_eq!(retained_usage.files, baseline.files + 1);
        assert_eq!(retained_usage.paths, baseline.paths + 1);
        assert!(retained_usage.temp > baseline.temp);
        assert_eq!(session.io_reserved(), 0);
        let deferred: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|p| p.unwrap().path())
            .filter(|p| !unrelated_paths.contains(p))
            .collect();
        assert_eq!(deferred.len(), 1);
        // Keep the actual failed-delete file while obstructing only its owned
        // retry path. A real remove_file error must preserve all charges over
        // repeated retries; restore it before final cleanup (no forget/leak).
        let path = &deferred[0];
        let saved = path.with_extension("w1-retained");
        std::fs::rename(path, &saved).unwrap();
        std::fs::create_dir(path).unwrap();
        for _ in 0..2 {
            assert_eq!(
                session.retry_cleanup().unwrap_err().kind(),
                ErrorKind::TemporaryStorageFailure
            );
            assert_eq!(session.temp().current(), retained_usage.temp);
            assert_eq!(session.file_usage().current, retained_usage.files);
            assert_eq!(session.handle_usage().current, retained_usage.handles);
            assert_eq!(session.memory().current(), retained_usage.memory);
        }
        std::fs::remove_dir(path).unwrap();
        std::fs::rename(saved, path).unwrap();
        session.retry_cleanup().unwrap();
        assert_eq!(usage(&session), baseline);
        drop(unrelated);
    }

    #[test]
    fn dp_rows_cross_page_boundaries_with_two_page_caches() {
        let (context, session, spool) = {
            let resources = ResourceConfig {
                memory: 32 * 1024 * 1024,
                ..ResourceConfig::default()
            };
            let context = ResourceContext::with_resources(&resources).unwrap();
            let session =
                CompressionSession::new(&context, &resources, OffloadPolicy::default()).unwrap();
            let spool =
                crate::codec::spool_input(Cursor::new(vec![0u8; 50_000]), &resources, &context)
                    .unwrap();
            (context, session, spool)
        };
        let mut sink = ExternalCandidateSink::with_input_len(&session, spool.len, 7).unwrap();
        for i in (0..1_400u64).rev() {
            sink.emit(CandidateRow {
                candidate: candidate(0, 100 + i * 30, 26, i),
                provenance: super::super::contracts::Provenance::EXTERNAL,
            })
            .unwrap();
        }
        let input = Validator::new(&spool, 26)
            .validate(sink.finish().unwrap(), &session)
            .unwrap()
            .into_schedule_input(&session)
            .unwrap();
        let selected = schedule_input(
            input,
            &session,
            TestBuilder {
                rows: Vec::new(),
                input_len: spool.len,
            },
        )
        .unwrap();
        assert_eq!(selected.rows.len(), 1_400);
        assert_eq!(selected.summary.covered_bytes, 1_400 * 26);
        drop(spool);
        drop(session);
        assert_eq!(context.temp.current(), 0);
    }
}
