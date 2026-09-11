use std::io::{Read, Seek, SeekFrom};

use super::source::DataSource;
use crate::codec::InputSpool;
use crate::error::{Error, Result};
use crate::resource::{BudgetedVec, ResourceContext};

pub(crate) const INPUT_SNAPSHOT_LIMIT: u64 = 16 * 1024 * 1024;
const COMPARE_CHUNK: usize = 4096;
const POLY_BASE: u64 = 153_191;

pub(crate) struct InputSnapshot {
    bytes: BudgetedVec<u8>,
    prefixes: Option<BudgetedVec<u64>>,
    powers: Option<BudgetedVec<u64>>,
    pub(crate) forward_lce: Option<ExactLce>,
    pub(crate) reverse_lce: Option<ExactLce>,
}

pub(crate) struct ExactLce {
    inverse: BudgetedVec<u32>,
    tree: BudgetedVec<u32>,
    tree_base: usize,
}

impl InputSnapshot {
    pub(crate) fn try_new(spool: &InputSpool, context: &ResourceContext) -> Result<Option<Self>> {
        if spool.len > INPUT_SNAPSHOT_LIMIT {
            return Ok(None);
        }
        let length = match usize::try_from(spool.len) {
            Ok(length) => length,
            Err(_) => return Ok(None),
        };
        let mut bytes = match BudgetedVec::with_capacity(length, &context.memory) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == crate::error::ErrorKind::MemoryBudgetExceeded => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        bytes.resize(length, 0)?;
        let mut file = spool.file.try_clone().map_err(Error::temp_storage)?;
        file.seek(SeekFrom::Start(0)).map_err(Error::temp_storage)?;
        file.read_exact(bytes.as_mut_slice())
            .map_err(Error::temp_storage)?;
        let (prefixes, powers) = match prefix_tables(bytes.as_slice(), context) {
            Ok((prefixes, powers)) => (Some(prefixes), Some(powers)),
            Err(error) if error.kind() == crate::error::ErrorKind::MemoryBudgetExceeded => {
                (None, None)
            }
            Err(error) => return Err(error),
        };
        let forward_lce = match ExactLce::try_new(bytes.as_slice(), context) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == crate::error::ErrorKind::MemoryBudgetExceeded => None,
            Err(error) => return Err(error),
        };
        let reverse_bytes = match BudgetedVec::with_capacity(length, &context.memory) {
            Ok(mut reverse) => {
                reverse.resize(length, 0)?;
                for (index, byte) in bytes.iter().rev().enumerate() {
                    reverse[index] = *byte;
                }
                Some(reverse)
            }
            Err(error) if error.kind() == crate::error::ErrorKind::MemoryBudgetExceeded => None,
            Err(error) => return Err(error),
        };
        let reverse_lce = match reverse_bytes.as_ref() {
            Some(reverse) => match ExactLce::try_new(reverse.as_slice(), context) {
                Ok(value) => Some(value),
                Err(error) if error.kind() == crate::error::ErrorKind::MemoryBudgetExceeded => None,
                Err(error) => return Err(error),
            },
            None => None,
        };
        Ok(Some(Self {
            bytes,
            prefixes,
            powers,
            forward_lce,
            reverse_lce,
        }))
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    pub(crate) fn polynomial_hash_at(&self, position: u64, length: u64) -> Option<u64> {
        let prefixes = self.prefixes.as_ref()?;
        let powers = self.powers.as_ref()?;
        let start = usize::try_from(position).ok()?;
        let count = usize::try_from(length).ok()?;
        let end = start.checked_add(count)?;
        if end > self.bytes.len() || end >= prefixes.len() || count >= powers.len() {
            return None;
        }
        Some(prefixes[end].wrapping_sub(prefixes[start].wrapping_mul(powers[count])))
    }

    pub(crate) fn packed_slice_metadata_at(&self, position: u64, length: u64) -> Option<u32> {
        let quotient = length / 8;
        let remainder = length % 8;
        let mut metadata = 0u32;
        let mut offset = 0u64;
        for slice in 0..8u64 {
            let slice_len = quotient.checked_add(u64::from(slice < remainder))?;
            let slice_position = position.checked_add(offset)?;
            let hash = self.polynomial_hash_at(slice_position, slice_len)?;
            metadata |= ((hash as u32) & 0x0f) << (slice as u32 * 4);
            offset = offset.checked_add(slice_len)?;
        }
        Some(metadata)
    }

    pub(crate) fn compare_contiguous(&self, first: u64, second: u64, length: u64) -> Option<bool> {
        let start = usize::try_from(first).ok()?;
        let other = usize::try_from(second).ok()?;
        let count = usize::try_from(length).ok()?;
        let end = start.checked_add(count)?;
        let other_end = other.checked_add(count)?;
        if end > self.bytes.len() || other_end > self.bytes.len() {
            return None;
        }
        Some(self.bytes.as_slice()[start..end] == self.bytes.as_slice()[other..other_end])
    }
}

fn prefix_tables(
    bytes: &[u8],
    context: &ResourceContext,
) -> Result<(BudgetedVec<u64>, BudgetedVec<u64>)> {
    let length = bytes
        .len()
        .checked_add(1)
        .ok_or_else(|| Error::memory_limit("snapshot prefix length overflows"))?;
    let mut prefixes = BudgetedVec::with_capacity(length, &context.memory)?;
    prefixes.push(0)?;
    let mut hash = 0u64;
    for &byte in bytes {
        hash = hash.wrapping_mul(POLY_BASE).wrapping_add(u64::from(byte));
        prefixes.push(hash)?;
    }
    let mut powers = BudgetedVec::with_capacity(length, &context.memory)?;
    powers.push(1)?;
    let mut power = 1u64;
    for _ in 1..length {
        power = power.wrapping_mul(POLY_BASE);
        powers.push(power)?;
    }
    Ok((prefixes, powers))
}

impl ExactLce {
    fn try_new(bytes: &[u8], context: &ResourceContext) -> Result<Self> {
        let length = bytes.len();
        if length == 0 {
            return Ok(Self {
                inverse: BudgetedVec::new(&context.memory)?,
                tree: BudgetedVec::with_capacity(2, &context.memory)?,
                tree_base: 1,
            });
        }
        let mut suffixes = BudgetedVec::with_capacity(length, &context.memory)?;
        let mut rank = BudgetedVec::with_capacity(length, &context.memory)?;
        let mut next_rank = BudgetedVec::with_capacity(length, &context.memory)?;
        for (index, &byte) in bytes.iter().enumerate() {
            suffixes
                .push(u32::try_from(index).map_err(|_| {
                    Error::memory_limit("suffix position exceeds platform limits")
                })?)?;
            rank.push(byte as u32)?;
            next_rank.push(0)?;
        }
        let mut radix_scratch = BudgetedVec::with_capacity(length, &context.memory)?;
        radix_scratch.resize(length, 0)?;
        let mut counts = BudgetedVec::with_capacity(length.max(257), &context.memory)?;
        counts.resize(length.max(257), 0)?;
        let mut width = 1usize;
        let mut classes = 256usize;
        while width < length {
            let context = SortContext {
                width,
                length,
                classes,
            };
            counting_sort_suffixes(
                suffixes.as_slice(),
                radix_scratch.as_mut_slice(),
                rank.as_slice(),
                &context,
                true,
                counts.as_mut_slice(),
            );
            counting_sort_suffixes(
                radix_scratch.as_slice(),
                suffixes.as_mut_slice(),
                rank.as_slice(),
                &context,
                false,
                counts.as_mut_slice(),
            );
            next_rank[suffixes[0] as usize] = 0;
            let mut new_classes = 1usize;
            for index in 1..length {
                let previous = suffixes[index - 1] as usize;
                let current = suffixes[index] as usize;
                let previous_key = (
                    rank[previous],
                    previous
                        .checked_add(width)
                        .map_or(0, |end| if end < length { rank[end] + 1 } else { 0 }),
                );
                let current_key = (
                    rank[current],
                    current
                        .checked_add(width)
                        .map_or(0, |end| if end < length { rank[end] + 1 } else { 0 }),
                );
                if previous_key != current_key {
                    new_classes += 1;
                }
                next_rank[current] = (new_classes - 1) as u32;
            }
            std::mem::swap(&mut rank, &mut next_rank);
            classes = new_classes;
            if classes == length {
                break;
            }
            width = width
                .checked_mul(2)
                .ok_or_else(|| Error::memory_limit("suffix width overflows"))?;
        }
        let mut inverse = BudgetedVec::with_capacity(length, &context.memory)?;
        inverse.resize(length, 0)?;
        let mut lcp = BudgetedVec::with_capacity(length, &context.memory)?;
        lcp.resize(length, 0)?;
        let mut common = 0usize;
        for (position, &suffix) in suffixes.iter().enumerate() {
            inverse[suffix as usize] = u32::try_from(position)
                .map_err(|_| Error::memory_limit("suffix rank exceeds platform limits"))?;
        }
        for index in 0..length {
            let position = inverse[index] as usize;
            if position == 0 {
                continue;
            }
            let other = suffixes[position - 1] as usize;
            while index + common < length
                && other + common < length
                && bytes[index + common] == bytes[other + common]
            {
                common += 1;
            }
            lcp[position] = u32::try_from(common)
                .map_err(|_| Error::memory_limit("LCE length exceeds platform limits"))?;
            common = common.saturating_sub(1);
        }
        let tree_base = length.next_power_of_two();
        let tree_len = tree_base
            .checked_mul(2)
            .ok_or_else(|| Error::memory_limit("LCE tree size overflows"))?;
        let mut tree = BudgetedVec::with_capacity(tree_len, &context.memory)?;
        tree.resize(tree_len, u32::MAX)?;
        for index in 0..length {
            tree[tree_base + index] = lcp[index];
        }
        for index in (1..tree_base).rev() {
            tree[index] = tree[index * 2].min(tree[index * 2 + 1]);
        }
        Ok(Self {
            inverse,
            tree,
            tree_base,
        })
    }

    fn query(&self, first: usize, second: usize, limit: usize) -> usize {
        if first >= self.inverse.len() || second >= self.inverse.len() {
            return 0;
        }
        if first == second {
            return limit;
        }
        let lower = (self.inverse[first] as usize).min(self.inverse[second] as usize);
        let upper = (self.inverse[first] as usize).max(self.inverse[second] as usize);
        let mut left = self.tree_base + lower + 1;
        let mut right = self.tree_base + upper;
        let mut result = u32::MAX;
        while left <= right {
            if left % 2 == 1 {
                result = result.min(self.tree[left]);
                left += 1;
            }
            if right.is_multiple_of(2) {
                result = result.min(self.tree[right]);
                right -= 1;
            }
            left /= 2;
            right /= 2;
        }
        (result as usize).min(limit)
    }
}

struct SortContext {
    width: usize,
    length: usize,
    classes: usize,
}

fn counting_sort_suffixes(
    input: &[u32],
    output: &mut [u32],
    rank: &[u32],
    context: &SortContext,
    second: bool,
    counts: &mut [u32],
) {
    counts[..=context.classes].fill(0);
    for &position in input {
        let position = position as usize;
        let key = if second {
            position
                .checked_add(context.width)
                .filter(|&end| end < context.length)
                .map_or(0, |end| rank[end] as usize + 1)
        } else {
            rank[position] as usize + 1
        };
        counts[key] += 1;
    }
    let mut start = 0u32;
    for count in &mut counts[..=context.classes] {
        let next = start + *count;
        *count = start;
        start = next;
    }
    for &position in input {
        let position_usize = position as usize;
        let key = if second {
            position_usize
                .checked_add(context.width)
                .filter(|&end| end < context.length)
                .map_or(0, |end| rank[end] as usize + 1)
        } else {
            rank[position_usize] as usize + 1
        };
        let output_index = counts[key] as usize;
        output[output_index] = position;
        counts[key] += 1;
    }
}

pub(crate) fn extend_candidate(
    source: &mut impl DataSource,
    source_position: u64,
    target: u64,
    seed_len: u64,
    snapshot: Option<&InputSnapshot>,
) -> Result<(u64, u64, u64)> {
    if let Some(snapshot) = snapshot {
        if snapshot.forward_lce.is_some() || snapshot.reverse_lce.is_some() {
            return extend_candidate_snapshot_with_lce(
                snapshot.as_slice(),
                snapshot.forward_lce.as_ref(),
                snapshot.reverse_lce.as_ref(),
                source_position,
                target,
                seed_len,
            );
        }
        return extend_candidate_snapshot(snapshot.as_slice(), source_position, target, seed_len);
    }
    extend_candidate_file(source, source_position, target, seed_len)
}

fn extend_candidate_file(
    source: &mut impl DataSource,
    source_position: u64,
    target: u64,
    seed_len: u64,
) -> Result<(u64, u64, u64)> {
    let mut backward = 0u64;
    let mut source_bytes = [0u8; COMPARE_CHUNK];
    let mut target_bytes = [0u8; COMPARE_CHUNK];
    let distance = target
        .checked_sub(source_position)
        .ok_or_else(|| Error::invalid_match("representative is not before target"))?;
    if distance == 0 {
        return Err(Error::invalid_match("match distance is zero"));
    }
    while backward < source_position && backward < target {
        let count = (source_position - backward)
            .min(target - backward)
            .min(COMPARE_CHUNK as u64);
        let source_at = source_position - backward - count;
        let target_at = target - backward - count;
        let count_usize = usize::try_from(count)
            .map_err(|_| Error::invalid_match("backward length exceeds platform limits"))?;
        source.read_at(source_at, &mut source_bytes[..count_usize])?;
        source.read_at(target_at, &mut target_bytes[..count_usize])?;
        let Some(equal_count) = source_bytes[..count_usize]
            .iter()
            .rev()
            .zip(target_bytes[..count_usize].iter().rev())
            .position(|(a, b)| a != b)
        else {
            backward = backward
                .checked_add(count)
                .ok_or_else(|| Error::invalid_match("backward length overflows"))?;
            continue;
        };
        backward = backward
            .checked_add(equal_count as u64)
            .ok_or_else(|| Error::invalid_match("backward length overflows"))?;
        break;
    }
    let src = source_position - backward;
    let dst = target - backward;
    let mut length = backward
        .checked_add(seed_len)
        .ok_or_else(|| Error::invalid_match("candidate length overflows"))?;
    let mut expected = [0u8; COMPARE_CHUNK];
    let mut actual = [0u8; COMPARE_CHUNK];
    while let Some(remaining) = source.len().checked_sub(
        dst.checked_add(length)
            .ok_or_else(|| Error::invalid_match("target endpoint overflows"))?,
    ) {
        if remaining == 0 {
            break;
        }
        let count = remaining.min(COMPARE_CHUNK as u64);
        let count_usize = usize::try_from(count)
            .map_err(|_| Error::invalid_match("forward length exceeds platform limits"))?;
        read_periodic(
            source,
            src,
            length % distance,
            distance,
            &mut expected[..count_usize],
        )?;
        source.read_at(
            dst.checked_add(length)
                .ok_or_else(|| Error::invalid_match("target position overflows"))?,
            &mut actual[..count_usize],
        )?;
        if expected[..count_usize] != actual[..count_usize] {
            let mismatch = expected[..count_usize]
                .iter()
                .zip(&actual[..count_usize])
                .position(|(a, b)| a != b)
                .ok_or_else(|| Error::invalid_match("forward mismatch is unavailable"))?;
            length = length
                .checked_add(mismatch as u64)
                .ok_or_else(|| Error::invalid_match("candidate length overflows"))?;
            break;
        }
        length = length
            .checked_add(count)
            .ok_or_else(|| Error::invalid_match("candidate length overflows"))?;
    }
    Ok((src, dst, length))
}

fn extend_candidate_snapshot_with_lce(
    bytes: &[u8],
    forward_lce: Option<&ExactLce>,
    reverse_lce: Option<&ExactLce>,
    source_position: u64,
    target: u64,
    seed_len: u64,
) -> Result<(u64, u64, u64)> {
    let input_len = u64::try_from(bytes.len())
        .map_err(|_| Error::memory_limit("snapshot exceeds platform limits"))?;
    let distance = target
        .checked_sub(source_position)
        .ok_or_else(|| Error::invalid_match("representative is not before target"))?;
    if distance == 0 {
        return Err(Error::invalid_match("match distance is zero"));
    }
    let backward = if let Some(lce) = reverse_lce {
        let first = usize::try_from(input_len - source_position)
            .map_err(|_| Error::invalid_match("reverse source exceeds platform limits"))?;
        let second = usize::try_from(input_len - target)
            .map_err(|_| Error::invalid_match("reverse target exceeds platform limits"))?;
        lce.query(
            first,
            second,
            usize::try_from(source_position.min(target)).unwrap_or(usize::MAX),
        ) as u64
    } else {
        let mut backward = 0u64;
        while backward < source_position
            && backward < target
            && bytes[(source_position - backward - 1) as usize]
                == bytes[(target - backward - 1) as usize]
        {
            backward += 1;
        }
        backward
    };
    let src = source_position - backward;
    let dst = target - backward;
    let mut length = backward
        .checked_add(seed_len)
        .ok_or_else(|| Error::invalid_match("candidate length overflows"))?;
    while let Some(remaining) = input_len.checked_sub(
        dst.checked_add(length)
            .ok_or_else(|| Error::invalid_match("target endpoint overflows"))?,
    ) {
        if remaining == 0 {
            break;
        }
        let segment = remaining.min(distance - (length % distance));
        let matched = if let Some(lce) = forward_lce {
            lce.query(
                usize::try_from(src + (length % distance))
                    .map_err(|_| Error::invalid_match("source exceeds platform limits"))?,
                usize::try_from(dst + length)
                    .map_err(|_| Error::invalid_match("target exceeds platform limits"))?,
                usize::try_from(segment)
                    .map_err(|_| Error::invalid_match("segment exceeds platform limits"))?,
            ) as u64
        } else {
            let mut matched = 0u64;
            while matched < segment
                && bytes[(src + length % distance + matched) as usize]
                    == bytes[(dst + length + matched) as usize]
            {
                matched += 1;
            }
            matched
        };
        length = length
            .checked_add(matched)
            .ok_or_else(|| Error::invalid_match("candidate length overflows"))?;
        if matched < segment {
            break;
        }
    }
    Ok((src, dst, length))
}

fn extend_candidate_snapshot(
    bytes: &[u8],
    source_position: u64,
    target: u64,
    seed_len: u64,
) -> Result<(u64, u64, u64)> {
    let length = u64::try_from(bytes.len())
        .map_err(|_| Error::memory_limit("snapshot exceeds platform limits"))?;
    let distance = target
        .checked_sub(source_position)
        .ok_or_else(|| Error::invalid_match("representative is not before target"))?;
    if distance == 0 {
        return Err(Error::invalid_match("match distance is zero"));
    }
    let mut backward = 0u64;
    while backward < source_position && backward < target {
        let count = (source_position - backward)
            .min(target - backward)
            .min(COMPARE_CHUNK as u64);
        let source_at = usize::try_from(source_position - backward - count)
            .map_err(|_| Error::invalid_match("backward source exceeds platform limits"))?;
        let target_at = usize::try_from(target - backward - count)
            .map_err(|_| Error::invalid_match("backward target exceeds platform limits"))?;
        let count = usize::try_from(count)
            .map_err(|_| Error::invalid_match("backward length exceeds platform limits"))?;
        let equal_count = bytes[source_at..source_at + count]
            .iter()
            .rev()
            .zip(bytes[target_at..target_at + count].iter().rev())
            .position(|(a, b)| a != b);
        match equal_count {
            Some(equal_count) => {
                backward = backward
                    .checked_add(equal_count as u64)
                    .ok_or_else(|| Error::invalid_match("backward length overflows"))?;
                break;
            }
            None => {
                backward = backward
                    .checked_add(count as u64)
                    .ok_or_else(|| Error::invalid_match("backward length overflows"))?;
            }
        }
    }
    let src = source_position - backward;
    let dst = target - backward;
    let mut match_len = backward
        .checked_add(seed_len)
        .ok_or_else(|| Error::invalid_match("candidate length overflows"))?;
    let mut expected = [0u8; COMPARE_CHUNK];
    while let Some(remaining) = length.checked_sub(
        dst.checked_add(match_len)
            .ok_or_else(|| Error::invalid_match("target endpoint overflows"))?,
    ) {
        if remaining == 0 {
            break;
        }
        let count = usize::try_from(remaining.min(COMPARE_CHUNK as u64))
            .map_err(|_| Error::invalid_match("forward length exceeds platform limits"))?;
        read_periodic_bytes(
            bytes,
            src,
            match_len % distance,
            distance,
            &mut expected[..count],
        )?;
        let actual_at = usize::try_from(
            dst.checked_add(match_len)
                .ok_or_else(|| Error::invalid_match("target position overflows"))?,
        )
        .map_err(|_| Error::invalid_match("target exceeds platform limits"))?;
        if expected[..count] != bytes[actual_at..actual_at + count] {
            let mismatch = expected[..count]
                .iter()
                .zip(&bytes[actual_at..actual_at + count])
                .position(|(expected, actual)| expected != actual)
                .ok_or_else(|| Error::invalid_match("forward mismatch is unavailable"))?;
            match_len = match_len
                .checked_add(mismatch as u64)
                .ok_or_else(|| Error::invalid_match("candidate length overflows"))?;
            break;
        }
        match_len = match_len
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("candidate length overflows"))?;
    }
    Ok((src, dst, match_len))
}

fn read_periodic(
    source: &mut impl DataSource,
    base: u64,
    mut offset: u64,
    period: u64,
    destination: &mut [u8],
) -> Result<()> {
    if period <= destination.len() as u64 {
        let period_len = usize::try_from(period)
            .map_err(|_| Error::invalid_match("period exceeds platform limits"))?;
        let mut period_bytes = [0u8; COMPARE_CHUNK];
        source.read_at(base, &mut period_bytes[..period_len])?;
        for (index, byte) in destination.iter_mut().enumerate() {
            *byte = period_bytes[(index + offset as usize) % period_len];
        }
        return Ok(());
    }
    let mut written = 0usize;
    while written < destination.len() {
        let count = usize::try_from((period - offset).min((destination.len() - written) as u64))
            .map_err(|_| Error::invalid_match("periodic read exceeds platform limits"))?;
        source.read_at(
            base.checked_add(offset)
                .ok_or_else(|| Error::invalid_match("periodic source position overflows"))?,
            &mut destination[written..written + count],
        )?;
        written += count;
        offset = 0;
    }
    Ok(())
}

fn read_periodic_bytes(
    bytes: &[u8],
    base: u64,
    mut offset: u64,
    period: u64,
    destination: &mut [u8],
) -> Result<()> {
    if period == 0 {
        return Err(Error::invalid_match("periodic distance is zero"));
    }
    let base = usize::try_from(base)
        .map_err(|_| Error::invalid_match("periodic base exceeds platform limits"))?;
    let period = usize::try_from(period)
        .map_err(|_| Error::invalid_match("periodic period exceeds platform limits"))?;
    let mut written = 0usize;
    while written < destination.len() {
        let offset_usize = usize::try_from(offset)
            .map_err(|_| Error::invalid_match("periodic offset exceeds platform limits"))?;
        let count = (period - offset_usize).min(destination.len() - written);
        let source_end = base
            .checked_add(offset_usize)
            .and_then(|start| start.checked_add(count))
            .ok_or_else(|| Error::invalid_match("periodic source endpoint overflows"))?;
        if source_end > bytes.len() {
            return Err(Error::truncated("periodic source exceeds input"));
        }
        destination[written..written + count]
            .copy_from_slice(&bytes[base + offset_usize..source_end]);
        written += count;
        offset = 0;
    }
    Ok(())
}
