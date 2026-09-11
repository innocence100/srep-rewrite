use std::cmp::Ordering;
use std::ops::Deref;

use crate::config::{DEFAULT_MEMORY, MIN_MATCH_LEN};
use crate::error::{Error, Result};
use crate::resource::{BudgetedVec, MemoryBudget};

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
struct ValidCandidate {
    candidate: MatchCandidate,
    gain: u64,
    end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Schedule {
    gain: u64,
    covered: u64,
    count: u64,
    node: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
struct ScheduleNode {
    candidate: usize,
    previous: Option<usize>,
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
    normalize_matches_impl(candidates, input_len, min_match, Some(&budget))
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
    normalize_matches_impl(candidates, input_len, min_match, Some(budget))
}

fn normalize_matches_impl<I>(
    candidates: I,
    input_len: u64,
    min_match: u64,
    budget: Option<&MemoryBudget>,
) -> Result<NormalizedMatches>
where
    I: IntoIterator<Item = MatchCandidate>,
{
    if min_match < MIN_MATCH_LEN {
        return Err(Error::invalid_match("minimum match is below wire minimum"));
    }
    let default_budget;
    let budget = match budget {
        Some(budget) => budget,
        None => {
            default_budget = MemoryBudget::new(DEFAULT_MEMORY);
            &default_budget
        }
    };
    let mut valid = BudgetedVec::new(budget)?;
    for candidate in candidates {
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
            continue;
        }
        if candidate.len < min_match {
            return Err(Error::invalid_match(
                "candidate is shorter than minimum match",
            ));
        }
        valid.push(ValidCandidate {
            candidate,
            gain: candidate.len - 25,
            end,
        })?;
    }
    valid.sort_unstable_by(|a, b| {
        a.candidate
            .dst
            .cmp(&b.candidate.dst)
            .then_with(|| a.candidate.src.cmp(&b.candidate.src))
            .then_with(|| b.candidate.len.cmp(&a.candidate.len))
            .then_with(|| {
                a.candidate
                    .insertion_ordinal
                    .cmp(&b.candidate.insertion_ordinal)
            })
    });
    valid.dedup_by(|a, b| {
        if a.candidate.src == b.candidate.src
            && a.candidate.dst == b.candidate.dst
            && a.candidate.len == b.candidate.len
        {
            if b.candidate.insertion_ordinal < a.candidate.insertion_ordinal {
                a.candidate.insertion_ordinal = b.candidate.insertion_ordinal;
            }
            true
        } else {
            false
        }
    });

    let mut by_end = BudgetedVec::with_capacity(valid.len(), budget)?;
    for index in 0..valid.len() {
        by_end.push(index)?;
    }
    by_end.sort_unstable_by_key(|&index| {
        (
            valid[index].end,
            valid[index].candidate.dst,
            valid[index].candidate.src,
            std::cmp::Reverse(valid[index].candidate.len),
            valid[index].candidate.insertion_ordinal,
        )
    });
    let mut nodes = BudgetedVec::with_capacity(valid.len(), budget)?;
    let best_len = valid
        .len()
        .checked_add(1)
        .ok_or_else(|| Error::memory_limit("schedule state count overflows"))?;
    let mut best = BudgetedVec::with_capacity(best_len, budget)?;
    best.resize(
        best_len,
        Schedule {
            gain: 0,
            covered: 0,
            count: 0,
            node: None,
        },
    )?;
    for i in 0..by_end.len() {
        let current = by_end[i];
        let predecessor = by_end.as_slice()[..i]
            .partition_point(|&index| valid[index].end <= valid[current].candidate.dst);
        let previous = best[predecessor];
        let mut include = previous;
        include.gain = include
            .gain
            .checked_add(valid[current].gain)
            .ok_or_else(|| Error::invalid_match("schedule gain overflows"))?;
        include.covered = include
            .covered
            .checked_add(valid[current].candidate.len)
            .ok_or_else(|| Error::invalid_match("schedule coverage overflows"))?;
        include.count = include
            .count
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("schedule count overflows"))?;
        let node = nodes.len();
        nodes.push(ScheduleNode {
            candidate: current,
            previous: previous.node,
        })?;
        include.node = Some(node);
        let exclude = best[i];
        best[i + 1] = if schedule_better(&include, &exclude, &valid, &nodes) {
            include
        } else {
            exclude
        };
    }

    let selected = schedule_indices(best[valid.len()].node, &nodes, budget)?;
    let mut matches = BudgetedVec::with_capacity(selected.len(), budget)?;
    let mut covered_bytes = 0u64;
    for (origin_match_id, index) in selected.iter().enumerate() {
        let candidate = valid[*index].candidate;
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
    Ok(NormalizedMatches {
        matches,
        covered_bytes,
        literal_bytes: input_len
            .checked_sub(covered_bytes)
            .ok_or_else(|| Error::invalid_match("match coverage exceeds input"))?,
    })
}

fn schedule_better(
    a: &Schedule,
    b: &Schedule,
    candidates: &[ValidCandidate],
    nodes: &[ScheduleNode],
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
        let a_index = schedule_index_at(a.node, a.count, position, nodes);
        let b_index = schedule_index_at(b.node, b.count, position, nodes);
        let a_key = &candidates[a_index].candidate;
        let b_key = &candidates[b_index].candidate;
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
    nodes: &[ScheduleNode],
) -> usize {
    let mut current = match node {
        Some(node) => node,
        None => return 0,
    };
    let mut steps = count - position - 1;
    while steps > 0 {
        current = match nodes.get(current).and_then(|node| node.previous) {
            Some(previous) => previous,
            None => return 0,
        };
        steps -= 1;
    }
    nodes.get(current).map_or(0, |node| node.candidate)
}

fn schedule_indices(
    node: Option<usize>,
    nodes: &[ScheduleNode],
    budget: &MemoryBudget,
) -> Result<BudgetedVec<usize>> {
    let mut result = BudgetedVec::with_capacity(nodes.len(), budget)?;
    let mut current = node;
    while let Some(index) = current {
        result.push(nodes[index].candidate)?;
        current = nodes[index].previous;
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
    fn duplicate_keeps_minimum_ordinal_and_permutation_is_stable() {
        let a = normalize_matches([c(0, 40, 30, 9), c(1, 40, 30, 3)], 100, 2).unwrap();
        let b = normalize_matches([c(1, 40, 30, 3), c(0, 40, 30, 9)], 100, 2).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.matches[0].src, 0);
    }

    #[test]
    fn nonpositive_gain_is_omitted_but_short_minimum_is_invalid() {
        assert!(normalize_matches([c(0, 30, 26, 0)], 100, 2).is_ok());
        assert!(normalize_matches([c(0, 30, 1, 0)], 100, 2).is_ok());
    }
}
