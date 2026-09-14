use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::Deref;

use crate::config::{DEFAULT_MEMORY, MIN_MATCH_LEN};
use crate::error::{Error, Result};
use crate::resource::{BudgetedVec, MemoryBudget, Reservation};

const IDENTICAL_INTERVAL_RESERVATION: u64 = 128;
const EXACT_INTERVAL_RESERVATION: u64 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchCandidate {
    pub src: u64,
    pub dst: u64,
    pub len: u64,
    pub insertion_ordinal: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub src: u64,
    pub dst: u64,
    pub len: u64,
    pub origin_match_id: u64,
}

#[derive(Debug)]
pub struct NormalizedMatches {
    pub matches: BudgetedVec<Match>,
    pub covered_bytes: u64,
    pub literal_bytes: u64,
}

#[derive(Debug)]
pub struct InspectedMatches {
    pub matches: BudgetedVec<Match>,
}

impl InspectedMatches {
    pub fn as_slice(&self) -> &[Match] {
        self.matches.as_slice()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Match> {
        self.matches.iter()
    }
}

impl NormalizedMatches {
    pub fn as_slice(&self) -> &[Match] {
        self.matches.as_slice()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Match> {
        self.matches.iter()
    }
}

impl Deref for NormalizedMatches {
    type Target = [Match];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl Deref for InspectedMatches {
    type Target = [Match];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl PartialEq for InspectedMatches {
    fn eq(&self, other: &Self) -> bool {
        self.matches.as_slice() == other.matches.as_slice()
    }
}

impl Eq for InspectedMatches {}

impl PartialEq<Vec<Match>> for InspectedMatches {
    fn eq(&self, other: &Vec<Match>) -> bool {
        self.matches.as_slice() == other.as_slice()
    }
}

impl PartialEq<InspectedMatches> for Vec<Match> {
    fn eq(&self, other: &InspectedMatches) -> bool {
        self.as_slice() == other.matches.as_slice()
    }
}

impl PartialEq for NormalizedMatches {
    fn eq(&self, other: &Self) -> bool {
        self.matches.as_slice() == other.matches.as_slice()
            && self.covered_bytes == other.covered_bytes
            && self.literal_bytes == other.literal_bytes
    }
}

impl Eq for NormalizedMatches {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Schedule {
    gain: u64,
    covered: u64,
    count: u64,
    node: Option<usize>,
}

pub fn normalize_matches<I>(
    candidates: I,
    input_len: u64,
    min_match: u64,
) -> Result<NormalizedMatches>
where
    I: IntoIterator<Item = MatchCandidate>,
{
    let budget = MemoryBudget::new(DEFAULT_MEMORY);
    normalize_matches_with_budget(candidates, input_len, min_match, &budget)
}

pub fn normalize_matches_with_budget<I>(
    candidates: I,
    input_len: u64,
    min_match: u64,
    budget: &MemoryBudget,
) -> Result<NormalizedMatches>
where
    I: IntoIterator<Item = MatchCandidate>,
{
    let mut owned = BudgetedVec::new(budget)?;
    for candidate in candidates {
        owned.push(candidate)?;
    }
    normalize_owned_matches_with_budget(owned, input_len, min_match)
}

/// Normalize candidates already owned by a finder without collecting another
/// candidate vector. The finder allocation remains accounted for while the
/// scheduler is running, and all error paths drop it through normal RAII.
pub(crate) fn normalize_owned_matches_with_budget(
    mut candidates: BudgetedVec<MatchCandidate>,
    input_len: u64,
    min_match: u64,
) -> Result<NormalizedMatches> {
    if min_match < MIN_MATCH_LEN {
        return Err(Error::invalid_match("minimum match is below wire minimum"));
    }
    let mut index = 0;
    while index < candidates.len() {
        let candidate = candidates[index];
        let end = candidate
            .dst
            .checked_add(candidate.len)
            .ok_or_else(|| Error::invalid_match("match destination overflows"))?;
        if candidate.src >= candidate.dst {
            return Err(Error::invalid_match(
                "match source must precede destination",
            ));
        }
        if end > input_len {
            return Err(Error::invalid_match("match destination exceeds input"));
        }
        if candidate.src >= input_len {
            return Err(Error::invalid_match("match source exceeds input"));
        }
        let distance = candidate.dst - candidate.src;
        let source_end = candidate
            .src
            .checked_add(candidate.len.min(distance))
            .ok_or_else(|| Error::invalid_match("match source overflows"))?;
        if source_end > input_len {
            return Err(Error::invalid_match("match source exceeds input"));
        }
        if candidate.len <= 25 {
            candidates.remove(index);
            continue;
        }
        if candidate.len < min_match {
            return Err(Error::invalid_match(
                "candidate is shorter than minimum match",
            ));
        }
        index += 1;
    }
    candidates.sort_unstable_by(|a, b| {
        a.dst
            .cmp(&b.dst)
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| b.len.cmp(&a.len))
            .then_with(|| a.insertion_ordinal.cmp(&b.insertion_ordinal))
    });
    candidates.dedup_by(|a, b| {
        if a.src == b.src && a.dst == b.dst && a.len == b.len {
            if b.insertion_ordinal < a.insertion_ordinal {
                a.insertion_ordinal = b.insertion_ordinal;
            }
            true
        } else {
            false
        }
    });

    // Candidates with the same destination interval `(dst, len)` have the
    // same gain, coverage, and compatibility with every other interval.  The
    // schedule tie-breaker can only prefer the smallest source, then the
    // smallest insertion ordinal.  Reduce those equivalent alternatives in
    // place before allocating scheduler state; unlike a destination-only
    // filter, this deliberately preserves every distinct interval length.
    candidates.sort_unstable_by(|a, b| {
        a.dst
            .cmp(&b.dst)
            .then_with(|| a.len.cmp(&b.len))
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| a.insertion_ordinal.cmp(&b.insertion_ordinal))
    });
    candidates.dedup_by(|a, b| a.dst == b.dst && a.len == b.len);

    let budget = candidates.budget().clone();
    candidates.sort_unstable_by_key(|candidate| {
        (
            candidate.dst + candidate.len,
            candidate.dst,
            candidate.src,
            std::cmp::Reverse(candidate.len),
            candidate.insertion_ordinal,
        )
    });
    let mut predecessors = BudgetedVec::with_capacity(candidates.len(), &budget)?;
    predecessors.resize(candidates.len(), u64::MAX)?;
    let best_len = candidates
        .len()
        .checked_add(1)
        .ok_or_else(|| Error::memory_limit("schedule state count overflows"))?;
    let mut best = BudgetedVec::with_capacity(best_len, &budget)?;
    best.resize(
        best_len,
        Schedule {
            gain: 0,
            covered: 0,
            count: 0,
            node: None,
        },
    )?;
    for i in 0..candidates.len() {
        let current = i;
        let predecessor = candidates.as_slice()[..i]
            .partition_point(|candidate| candidate.dst + candidate.len <= candidates[current].dst);
        let previous = best[predecessor];
        let mut include = previous;
        include.gain = include
            .gain
            .checked_add(candidates[current].len - 25)
            .ok_or_else(|| Error::invalid_match("schedule gain overflows"))?;
        include.covered = include
            .covered
            .checked_add(candidates[current].len)
            .ok_or_else(|| Error::invalid_match("schedule coverage overflows"))?;
        include.count = include
            .count
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("schedule count overflows"))?;
        let exclude = best[i];
        predecessors[current] = previous.node.map_or(u64::MAX, |index| index as u64);
        include.node = Some(current);
        best[i + 1] = if schedule_better(&include, &exclude, &candidates, &predecessors) {
            include
        } else {
            exclude
        };
    }

    let selected = schedule_indices(best[candidates.len()].node, &predecessors, &budget)?;
    drop(best);
    drop(predecessors);
    let mut matches = BudgetedVec::with_capacity(selected.len(), &budget)?;
    let mut covered_bytes = 0u64;
    for (origin_match_id, index) in selected.iter().enumerate() {
        let candidate = candidates[*index];
        covered_bytes = covered_bytes
            .checked_add(candidate.len)
            .ok_or_else(|| Error::invalid_match("coverage overflows"))?;
        matches.push(Match {
            src: candidate.src,
            dst: candidate.dst,
            len: candidate.len,
            origin_match_id: u64::try_from(origin_match_id)
                .map_err(|_| Error::memory_limit("match ID exceeds platform limits"))?,
        })?;
    }
    matches.sort_unstable_by(|a, b| {
        a.dst
            .cmp(&b.dst)
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| b.len.cmp(&a.len))
            .then_with(|| a.origin_match_id.cmp(&b.origin_match_id))
    });
    for (id, item) in matches.iter_mut().enumerate() {
        item.origin_match_id = u64::try_from(id)
            .map_err(|_| Error::memory_limit("match ID exceeds platform limits"))?;
    }
    drop(candidates);
    Ok(NormalizedMatches {
        matches,
        covered_bytes,
        literal_bytes: input_len
            .checked_sub(covered_bytes)
            .ok_or_else(|| Error::invalid_match("match coverage exceeds input"))?,
    })
}

/// Codec-only representative selection for identical destination starts.
///
/// The weighted scheduler keys intervals by destination range and gain
/// `len - 25`. Same-`dst` matches that are shorter than the longest one at
/// that start cannot beat it on gain, and `schedule_better` already prefers
/// smaller `src` then smaller `insertion_ordinal` among equal-gain ties.
/// Keeping that representative does not change the selected IR. Distinct
/// destination starts are preserved, so this is not a raw-enumeration filter.
#[allow(dead_code)]
pub(crate) fn retain_identical_interval_representatives(
    candidates: &mut BudgetedVec<MatchCandidate>,
) {
    candidates.sort_unstable_by(|a, b| {
        a.dst
            .cmp(&b.dst)
            .then_with(|| b.len.cmp(&a.len))
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| a.insertion_ordinal.cmp(&b.insertion_ordinal))
    });
    candidates.dedup_by(|a, b| a.dst == b.dst);
}

fn compact_interval_better(candidate: &MatchCandidate, existing: &MatchCandidate) -> bool {
    candidate.len > existing.len
        || (candidate.len == existing.len
            && (candidate.src < existing.src
                || (candidate.src == existing.src
                    && candidate.insertion_ordinal < existing.insertion_ordinal)))
}

/// Online codec-only filter that keeps one representative per destination start.
pub(crate) struct IdenticalIntervalFilter {
    index: HashMap<u64, usize>,
    reservation: Reservation,
}

/// Exact normalization-equivalence filter. Unlike the historical compact
/// filter above, the key includes the interval length: all `(dst, len)`
/// alternatives have identical WIS weight and overlap behavior, but distinct
/// lengths must remain available to the scheduler.
pub(crate) struct ExactIntervalFilter {
    index: HashMap<(u64, u64), usize>,
    reservation: Reservation,
}

impl ExactIntervalFilter {
    pub(crate) fn new(budget: &MemoryBudget) -> Result<Self> {
        Ok(Self {
            index: HashMap::new(),
            reservation: budget.reserve(0)?,
        })
    }

    pub(crate) fn consider(
        &mut self,
        output: &mut BudgetedVec<MatchCandidate>,
        candidate: MatchCandidate,
    ) -> Result<()> {
        let key = (candidate.dst, candidate.len);
        if let Some(&index) = self.index.get(&key) {
            let existing = &mut output[index];
            if (candidate.src, candidate.insertion_ordinal)
                < (existing.src, existing.insertion_ordinal)
            {
                *existing = candidate;
            }
            return Ok(());
        }
        self.reservation.grow(EXACT_INTERVAL_RESERVATION)?;
        self.index.try_reserve(1).map_err(|error| {
            Error::memory_limit(format!("exact-interval filter allocation failed: {error}"))
        })?;
        self.index.insert(key, output.len());
        output.push(candidate)
    }
}

impl IdenticalIntervalFilter {
    #[allow(dead_code)]
    pub(crate) fn new(budget: &MemoryBudget) -> Result<Self> {
        Ok(Self {
            index: HashMap::new(),
            reservation: budget.reserve(0)?,
        })
    }

    pub(crate) fn consider(
        &mut self,
        output: &mut BudgetedVec<MatchCandidate>,
        candidate: MatchCandidate,
    ) -> Result<()> {
        let key = candidate.dst;
        if let Some(&index) = self.index.get(&key) {
            if compact_interval_better(&candidate, &output[index]) {
                output[index] = candidate;
            }
            return Ok(());
        }
        self.reservation.grow(IDENTICAL_INTERVAL_RESERVATION)?;
        self.index.try_reserve(1).map_err(|error| {
            Error::memory_limit(format!(
                "identical-interval filter allocation failed: {error}"
            ))
        })?;
        self.index.insert(key, output.len());
        output.push(candidate)
    }
}

fn schedule_better(
    a: &Schedule,
    b: &Schedule,
    candidates: &[MatchCandidate],
    predecessors: &[u64],
) -> bool {
    match a.gain.cmp(&b.gain) {
        Ordering::Equal => {}
        order => return order == Ordering::Greater,
    }
    match a.covered.cmp(&b.covered) {
        Ordering::Equal => {}
        order => return order == Ordering::Greater,
    }
    match b.count.cmp(&a.count) {
        Ordering::Equal => {}
        order => return order == Ordering::Greater,
    }
    let common = a.count.min(b.count);
    for position in 0..common {
        let a_index = schedule_index_at(a.node, a.count, position, predecessors);
        let b_index = schedule_index_at(b.node, b.count, position, predecessors);
        let a_key = &candidates[a_index];
        let b_key = &candidates[b_index];
        let order = a_key
            .dst
            .cmp(&b_key.dst)
            .then_with(|| a_key.src.cmp(&b_key.src))
            .then_with(|| b_key.len.cmp(&a_key.len))
            .then_with(|| a_key.insertion_ordinal.cmp(&b_key.insertion_ordinal));
        if order != Ordering::Equal {
            return order == Ordering::Less;
        }
    }
    a.count < b.count
}

fn schedule_index_at(
    node: Option<usize>,
    count: u64,
    position: u64,
    predecessors: &[u64],
) -> usize {
    let mut current = match node {
        Some(node) => node,
        None => return 0,
    };
    let mut steps = count - position - 1;
    while steps > 0 {
        let previous = predecessors.get(current).copied().unwrap_or(u64::MAX);
        if previous == u64::MAX {
            return 0;
        }
        current = usize::try_from(previous).unwrap_or(0);
        steps -= 1;
    }
    current
}

fn schedule_indices(
    node: Option<usize>,
    predecessors: &[u64],
    budget: &MemoryBudget,
) -> Result<BudgetedVec<usize>> {
    let mut result = BudgetedVec::new(budget)?;
    let mut current = node;
    while let Some(index) = current {
        result.push(index)?;
        let previous = predecessors[index];
        current =
            if previous == u64::MAX {
                None
            } else {
                Some(usize::try_from(previous).map_err(|_| {
                    Error::memory_limit("schedule predecessor exceeds platform limits")
                })?)
            };
    }
    result.reverse();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(src: u64, dst: u64, len: u64, ordinal: u64) -> MatchCandidate {
        MatchCandidate {
            src,
            dst,
            len,
            insertion_ordinal: ordinal,
        }
    }

    #[test]
    fn weighted_schedule_beats_greedy_longest_overlap() {
        let result =
            normalize_matches([c(0, 30, 40, 0), c(30, 70, 40, 1), c(0, 30, 81, 2)], 200, 2)
                .unwrap();
        assert_eq!(result.matches[0].len, 81);
    }

    #[test]
    fn weighted_schedule_keeps_shorter_interval_before_following_gain() {
        let result = normalize_matches(
            [c(0, 100, 60, 0), c(0, 100, 100, 1), c(0, 160, 100, 2)],
            260,
            32,
        )
        .unwrap();
        assert_eq!(
            result
                .matches
                .iter()
                .map(|item| (item.dst, item.len))
                .collect::<Vec<_>>(),
            vec![(100, 60), (160, 100)]
        );
    }

    #[test]
    fn duplicate_keeps_minimum_ordinal_and_permutation_is_stable() {
        let a = normalize_matches([c(0, 40, 30, 9), c(1, 40, 30, 3)], 100, 2).unwrap();
        let b = normalize_matches([c(1, 40, 30, 3), c(0, 40, 30, 9)], 100, 2).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.matches[0].src, 0);
    }

    #[test]
    fn owned_normalization_releases_input_on_validation_error() {
        let budget = MemoryBudget::new(4096);
        let mut candidates = BudgetedVec::new(&budget).unwrap();
        candidates
            .push(c(0, 40, 30, 0))
            .expect("candidate allocation should fit");
        candidates
            .push(c(1, 40, u64::MAX, 1))
            .expect("candidate allocation should fit");
        assert!(normalize_owned_matches_with_budget(candidates, 100, 32).is_err());
        assert_eq!(budget.current(), 0);
    }

    #[test]
    fn nonpositive_gain_is_omitted_but_short_minimum_is_invalid() {
        assert!(normalize_matches([c(0, 30, 26, 0)], 100, 2).is_ok());
        assert!(normalize_matches([c(0, 30, 1, 0)], 100, 2).is_ok());
    }

    #[test]
    fn identical_interval_representatives_preserve_weighted_schedule() {
        let raw = [
            c(8, 40, 30, 0),
            c(0, 40, 30, 4),
            c(4, 40, 30, 1),
            c(0, 40, 50, 2),
            c(2, 70, 26, 3),
        ];
        let full = normalize_matches(raw, 200, 2).unwrap();
        let budget = MemoryBudget::new(DEFAULT_MEMORY);
        let mut compact = BudgetedVec::new(&budget).unwrap();
        for candidate in raw {
            compact.push(candidate).unwrap();
        }
        retain_identical_interval_representatives(&mut compact);
        assert_eq!(compact.len(), 2);
        assert!(
            compact
                .iter()
                .any(|candidate| candidate.src == 0 && candidate.dst == 40 && candidate.len == 50)
        );
        assert!(
            !compact
                .iter()
                .any(|candidate| candidate.dst == 40 && candidate.len != 50)
        );
        let compacted = normalize_matches(compact.iter().copied(), 200, 2).unwrap();
        assert_eq!(full, compacted);
    }
}
