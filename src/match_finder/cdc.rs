use std::io::Read;

use crate::candidate_index::{CandidateIndex, IndexEntry, new_candidate_index};
use crate::codec::{InputSpool, block_len_at, expected_block_count, spool_input};
use crate::config::{CompressionConfig, M1_SEED_SIZE, Method};
use crate::error::{Error, Result};
use crate::match_ir::MatchCandidate;
use crate::polynomial::polynomial_hash;
use crate::resource::{BudgetedVec, MemoryBudget, ResourceContext};

use super::source::{DataSource, SpoolDataSource};

const BLAKE3_KEY_LEN: usize = 24;
const COMPARE_CHUNK: usize = 4096;

#[derive(Clone, Copy, Debug)]
pub(crate) enum CdcMethod {
    M1,
    M2,
}

impl CdcMethod {
    const fn kind(self) -> u8 {
        match self {
            Self::M1 => 1,
            Self::M2 => 2,
        }
    }

    const fn expected_method(self) -> Method {
        match self {
            Self::M1 => Method::M1RollingCdc,
            Self::M2 => Method::M2Order1Cdc,
        }
    }
}

pub fn find_matches_m1<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches(input, config, context, CdcMethod::M1)
}

pub fn find_matches_m2<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches(input, config, context, CdcMethod::M2)
}

pub fn find_matches_m1_with_resources<R: Read>(
    input: R,
    config: &CompressionConfig,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    let context = ResourceContext::with_resources(&config.resources)?;
    find_matches_m1(input, config, &context)
}

pub fn find_matches_m2_with_resources<R: Read>(
    input: R,
    config: &CompressionConfig,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    let context = ResourceContext::with_resources(&config.resources)?;
    find_matches_m2(input, config, &context)
}

pub fn find_matches_m1_with_context<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches_m1(input, config, context)
}

pub fn find_matches_m2_with_context<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches_m2(input, config, context)
}

fn find_matches<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
    method: CdcMethod,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    if config.method != method.expected_method() {
        return Err(Error::invalid_config(
            "CDC finder method does not match configuration",
        ));
    }
    let spool = spool_input(input, &config.resources, context)?;
    find_matches_spooled(&spool, config, context, method)
}

pub(crate) fn find_matches_spooled(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    method: CdcMethod,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches_spooled_with_digest(spool, config, context, method, blake3_at)
}

fn find_matches_spooled_with_digest<D>(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    method: CdcMethod,
    mut digest_fn: D,
) -> Result<BudgetedVec<MatchCandidate>>
where
    D: FnMut(&mut dyn DataSource, u64, u64) -> Result<[u8; 16]>,
{
    let mut source = SpoolDataSource::new(spool)?;
    let mut index = new_candidate_index(context, &config.resources)?;
    let mut output = BudgetedVec::new(&context.memory)?;
    let block_count = expected_block_count(spool.len, config.block_size)?;
    let mut source_ordinal = 0u64;
    let mut candidate_ordinal = 0u64;
    for block_id in 0..block_count {
        let block_len = block_len_at(spool.len, config.block_size, block_id)?;
        let block_start = block_id
            .checked_mul(config.block_size)
            .ok_or_else(|| Error::invalid_match("CDC block start overflows"))?;
        let block_end = block_start
            .checked_add(block_len)
            .ok_or_else(|| Error::invalid_match("CDC block end overflows"))?;
        let boundaries = match method {
            CdcMethod::M1 => m1_boundaries(
                &mut source,
                block_start,
                block_end,
                config
                    .target_chunk
                    .ok_or_else(|| Error::invalid_config("target chunk is required for m1"))?,
                config.min_match,
                &context.memory,
            )?,
            CdcMethod::M2 => m2_boundaries(
                &mut source,
                block_start,
                block_end,
                config
                    .target_chunk
                    .ok_or_else(|| Error::invalid_config("target chunk is required for m2"))?,
                config.min_match,
                &context.memory,
            )?,
        };
        for &(chunk_start, chunk_end) in &boundaries {
            let chunk_len = chunk_end
                .checked_sub(chunk_start)
                .ok_or_else(|| Error::invalid_match("CDC chunk bounds are reversed"))?;
            if chunk_len == 0 {
                return Err(Error::invalid_match("CDC emitted an empty chunk"));
            }
            let digest = digest_fn(&mut source, chunk_start, chunk_len)?;
            let key = cdc_key(chunk_len, &digest);
            index.for_each_candidate(
                method.kind(),
                &key,
                chunk_start,
                config.max_distance.unwrap_or(0),
                &mut |entry| {
                    if compare_contiguous(&mut source, entry.position, chunk_start, chunk_len)?
                        && chunk_len >= config.min_match
                    {
                        output.push(MatchCandidate {
                            src: entry.position,
                            dst: chunk_start,
                            len: chunk_len,
                            insertion_ordinal: candidate_ordinal,
                        })?;
                        candidate_ordinal = candidate_ordinal.checked_add(1).ok_or_else(|| {
                            Error::invalid_match("CDC insertion ordinal overflows")
                        })?;
                    }
                    Ok(())
                },
            )?;
            let ordinal = source_ordinal;
            source_ordinal = source_ordinal
                .checked_add(1)
                .ok_or_else(|| Error::invalid_match("CDC insertion ordinal overflows"))?;
            index.insert(IndexEntry::new(
                method.kind(),
                &key,
                chunk_start,
                ordinal,
                &[],
            )?)?;
        }
    }
    Ok(output)
}

fn cdc_key(length: u64, digest: &[u8; 16]) -> [u8; BLAKE3_KEY_LEN] {
    let mut key = [0u8; BLAKE3_KEY_LEN];
    key[..8].copy_from_slice(&length.to_le_bytes());
    key[8..].copy_from_slice(digest);
    key
}

fn blake3_at(source: &mut dyn DataSource, position: u64, length: u64) -> Result<[u8; 16]> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; COMPARE_CHUNK];
    let mut offset = 0u64;
    while offset < length {
        let count = usize::try_from((length - offset).min(COMPARE_CHUNK as u64))
            .map_err(|_| Error::memory_limit("CDC digest length exceeds platform limits"))?;
        let at = position
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("CDC digest position overflows"))?;
        source.read_at(at, &mut buffer[..count])?;
        hasher.update(&buffer[..count]);
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("CDC digest offset overflows"))?;
    }
    let digest = hasher.finalize();
    let mut result = [0u8; 16];
    result.copy_from_slice(&digest.as_bytes()[..16]);
    Ok(result)
}

fn compare_contiguous(
    source: &mut impl DataSource,
    first: u64,
    second: u64,
    length: u64,
) -> Result<bool> {
    let mut first_bytes = [0u8; COMPARE_CHUNK];
    let mut second_bytes = [0u8; COMPARE_CHUNK];
    let mut offset = 0u64;
    while offset < length {
        let count = usize::try_from((length - offset).min(COMPARE_CHUNK as u64))
            .map_err(|_| Error::memory_limit("CDC comparison length exceeds platform limits"))?;
        let first_at = first
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("CDC source position overflows"))?;
        let second_at = second
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("CDC target position overflows"))?;
        source.read_at(first_at, &mut first_bytes[..count])?;
        source.read_at(second_at, &mut second_bytes[..count])?;
        if first_bytes[..count] != second_bytes[..count] {
            return Ok(false);
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("CDC comparison offset overflows"))?;
    }
    Ok(true)
}

fn threshold_u64(target: u64) -> u64 {
    u64::MAX - u64::MAX / target
}

fn threshold_u32(target: u64) -> u32 {
    u32::MAX - (u32::MAX / u32::try_from(target).unwrap_or(u32::MAX))
}

fn m1_boundaries(
    source: &mut impl DataSource,
    block_start: u64,
    block_end: u64,
    target_chunk: u64,
    min_chunk: u64,
    budget: &MemoryBudget,
) -> Result<BudgetedVec<(u64, u64)>> {
    let threshold = threshold_u64(target_chunk);
    m1_boundaries_core(
        source,
        block_start,
        block_end,
        budget,
        |_, hash, chunk_len| hash > threshold && chunk_len >= min_chunk,
    )
}

fn m1_boundaries_core<P>(
    source: &mut impl DataSource,
    block_start: u64,
    block_end: u64,
    budget: &MemoryBudget,
    mut trigger: P,
) -> Result<BudgetedVec<(u64, u64)>>
where
    P: FnMut(u64, u64, u64) -> bool,
{
    let mut boundaries = BudgetedVec::new(budget)?;
    if block_start == block_end {
        return Ok(boundaries);
    }
    if block_end - block_start <= M1_SEED_SIZE {
        boundaries.push((block_start, block_end))?;
        return Ok(boundaries);
    }
    let mut window = [0u8; M1_SEED_SIZE as usize];
    source.read_at(block_start, &mut window)?;
    let mut hash = polynomial_hash(&window);
    let outgoing_factor = base_power(47);
    let mut last_boundary = block_start;
    let mut p = block_start
        .checked_add(M1_SEED_SIZE)
        .ok_or_else(|| Error::invalid_match("m1 scan position overflows"))?;
    while p < block_end {
        let mut incoming = [0u8; 1];
        let old_position = p
            .checked_sub(M1_SEED_SIZE)
            .ok_or_else(|| Error::invalid_match("m1 outgoing position underflows"))?;
        source.read_at(old_position, &mut incoming)?;
        let old = incoming[0];
        source.read_at(p, &mut incoming)?;
        let new = incoming[0];
        hash = hash
            .wrapping_sub((old as u64).wrapping_mul(outgoing_factor))
            .wrapping_mul(153_191)
            .wrapping_add(new as u64);
        let chunk_len = p
            .checked_sub(last_boundary)
            .ok_or_else(|| Error::invalid_match("m1 chunk length underflows"))?;
        if trigger(p, hash, chunk_len) {
            boundaries.push((last_boundary, p))?;
            last_boundary = p;
        }
        p = p
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("m1 scan position overflows"))?;
    }
    if last_boundary < block_end {
        boundaries.push((last_boundary, block_end))?;
    }
    Ok(boundaries)
}

fn m2_boundaries(
    source: &mut impl DataSource,
    block_start: u64,
    block_end: u64,
    target_chunk: u64,
    min_chunk: u64,
    budget: &MemoryBudget,
) -> Result<BudgetedVec<(u64, u64)>> {
    let threshold = threshold_u32(target_chunk);
    m2_boundaries_core(
        source,
        block_start,
        block_end,
        budget,
        |_, hash, chunk_len| hash > u64::from(threshold) && chunk_len >= min_chunk,
    )
}

fn m2_boundaries_core<P>(
    source: &mut impl DataSource,
    block_start: u64,
    block_end: u64,
    budget: &MemoryBudget,
    mut trigger: P,
) -> Result<BudgetedVec<(u64, u64)>>
where
    P: FnMut(u64, u64, u64) -> bool,
{
    let mut boundaries = BudgetedVec::new(budget)?;
    if block_start == block_end {
        return Ok(boundaries);
    }
    let mut predict = [0u8; 256];
    let mut previous = 0u8;
    let mut hash = 0u32;
    let mut last_boundary = block_start;
    let mut p = block_start;
    while p < block_end {
        let mut byte = [0u8; 1];
        source.read_at(p, &mut byte)?;
        let c = byte[0];
        let multiplier = if c != predict[previous as usize] {
            271_828_182u32
        } else {
            314_159_265u32
        };
        hash = hash.wrapping_add(u32::from(c) + 1).wrapping_mul(multiplier);
        predict[previous as usize] = c;
        previous = c;
        let chunk_len = p
            .checked_sub(last_boundary)
            .ok_or_else(|| Error::invalid_match("m2 chunk length underflows"))?;
        if trigger(p, u64::from(hash), chunk_len) {
            boundaries.push((last_boundary, p))?;
            last_boundary = p;
            predict = [0u8; 256];
            previous = 0;
            hash = 0;
        }
        p = p
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("m2 scan position overflows"))?;
    }
    if last_boundary < block_end {
        boundaries.push((last_boundary, block_end))?;
    }
    Ok(boundaries)
}

fn base_power(exponent: usize) -> u64 {
    let mut result = 1u64;
    for _ in 0..exponent {
        result = result.wrapping_mul(153_191);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use crate::config::{CompressionConfig, ResourceConfig};

    fn source(bytes: &[u8]) -> (InputSpool, ResourceContext) {
        let resources = ResourceConfig {
            memory: 64 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let context = ResourceContext::with_resources(&resources).unwrap();
        let spool = spool_input(Cursor::new(bytes), &resources, &context).unwrap();
        (spool, context)
    }

    #[test]
    fn m1_boundary_short_and_seed_lengths_are_forced_without_duplicates() {
        for length in [0, 1, 47, 48, 49, 80] {
            let bytes = vec![0x5a; length];
            let (spool, _context) = source(&bytes);
            let mut data = SpoolDataSource::new(&spool).unwrap();
            let budget = MemoryBudget::new(64 * 1024 * 1024);
            let result = m1_boundaries(&mut data, 0, length as u64, 32, 32, &budget).unwrap();
            let actual = result.as_slice().to_vec();
            assert!(actual.windows(2).all(|pair| pair[0].1 <= pair[1].0));
            assert!(actual.iter().all(|&(start, end)| start < end));
            assert_eq!(actual.first().map(|item| item.0), (length > 0).then_some(0));
            assert_eq!(
                actual.last().map(|item| item.1),
                (length > 0).then_some(length as u64)
            );
        }
    }

    #[test]
    fn m1_boundary_state_matches_an_independent_reference_for_hits_and_misses() {
        let bytes: Vec<u8> = (0..4096)
            .map(|value| ((value * 17 + value / 11) & 0xff) as u8)
            .collect();
        let expected = independent_m1_boundaries(&bytes, 32, 32);
        let (spool, _context) = source(&bytes);
        let mut data = SpoolDataSource::new(&spool).unwrap();
        let budget = MemoryBudget::new(64 * 1024 * 1024);
        let actual = m1_boundaries(&mut data, 0, bytes.len() as u64, 32, 32, &budget).unwrap();
        assert_eq!(actual.as_slice(), expected.as_slice());
        assert!(actual.len() > 1);
    }

    #[test]
    fn m1_exact_edge_vectors_share_the_production_state_machine() {
        for (length, forced_hit) in [
            (48u64, false),
            (48, true),
            (49, false),
            (49, true),
            (80, false),
            (80, true),
        ] {
            let block_start = 16u64;
            let block_end = block_start + length;
            let bytes: Vec<u8> = (0..(block_end as usize + 1))
                .map(|value| ((value * 37 + 9) & 0xff) as u8)
                .collect();
            let (spool, _context) = source(&bytes);
            let mut data = SpoolDataSource::new(&spool).unwrap();
            let budget = MemoryBudget::new(64 * 1024 * 1024);
            let mut observed = Vec::new();
            let actual = m1_boundaries_core(
                &mut data,
                block_start,
                block_end,
                &budget,
                |position, hash, chunk_len| {
                    observed.push((position, hash, chunk_len));
                    forced_hit && position == block_start + 48 && chunk_len >= 32
                },
            )
            .unwrap();
            let expected =
                independent_m1_boundaries_at(&bytes, block_start, block_end, 32, forced_hit);
            assert_eq!(actual.as_slice(), expected.as_slice(), "length={length}");
            assert!(
                actual
                    .as_slice()
                    .windows(2)
                    .all(|pair| pair[0].1 <= pair[1].0)
            );
            assert!(actual.as_slice().iter().all(|&(start, end)| start < end));
            if forced_hit && length > 48 {
                assert_eq!(actual[0], (block_start, block_start + 48));
                assert_eq!(actual[1], (block_start + 48, block_end));
            } else {
                assert_eq!(actual.as_slice(), &[(block_start, block_end)]);
            }
            for (position, rolling, _) in observed {
                let start = position - 47;
                assert_eq!(
                    rolling,
                    polynomial_hash(&bytes[start as usize..=position as usize])
                );
            }
        }
    }

    fn independent_m1_boundaries_at(
        bytes: &[u8],
        block_start: u64,
        block_end: u64,
        min_chunk: u64,
        forced_hit: bool,
    ) -> Vec<(u64, u64)> {
        if block_end - block_start <= 48 {
            return vec![(block_start, block_end)];
        }
        let mut result = Vec::new();
        let mut last = block_start;
        let mut hash = polynomial_hash(&bytes[block_start as usize..(block_start + 48) as usize]);
        let power = base_power(47);
        for position in block_start + 48..block_end {
            hash = hash
                .wrapping_sub(u64::from(bytes[(position - 48) as usize]).wrapping_mul(power))
                .wrapping_mul(153_191)
                .wrapping_add(u64::from(bytes[position as usize]));
            if forced_hit && position == block_start + 48 && position - last >= min_chunk {
                result.push((last, position));
                last = position;
            }
        }
        if last < block_end {
            result.push((last, block_end));
        }
        result
    }

    fn independent_m1_boundaries(bytes: &[u8], min_chunk: usize, target: u64) -> Vec<(u64, u64)> {
        let mut result = Vec::new();
        let mut hash = bytes[..48].iter().fold(0u64, |value, &byte| {
            value.wrapping_mul(153_191).wrapping_add(u64::from(byte))
        });
        let power = (0..47).fold(1u64, |value, _| value.wrapping_mul(153_191));
        let threshold = u64::MAX - u64::MAX / target;
        let mut last = 0u64;
        for p in 48..bytes.len() {
            hash = hash
                .wrapping_sub(u64::from(bytes[p - 48]).wrapping_mul(power))
                .wrapping_mul(153_191)
                .wrapping_add(u64::from(bytes[p]));
            if hash > threshold && p as u64 - last >= min_chunk as u64 {
                result.push((last, p as u64));
                last = p as u64;
            }
        }
        if last < bytes.len() as u64 {
            result.push((last, bytes.len() as u64));
        }
        result
    }

    #[test]
    fn m1_rolling_update_matches_recomputed_window() {
        let bytes: Vec<u8> = (0..100).map(|value| value as u8).collect();
        let (spool, _context) = source(&bytes);
        let mut data = SpoolDataSource::new(&spool).unwrap();
        let mut window = [0u8; 48];
        data.read_at(0, &mut window).unwrap();
        let mut rolling = polynomial_hash(&window);
        for p in 48..bytes.len() {
            rolling = rolling
                .wrapping_sub(u64::from(bytes[p - 48]).wrapping_mul(base_power(47)))
                .wrapping_mul(153_191)
                .wrapping_add(u64::from(bytes[p]));
            assert_eq!(rolling, polynomial_hash(&bytes[p - 47..=p]));
        }
    }

    #[test]
    fn m2_reset_does_not_replay_trigger_byte() {
        let bytes: Vec<u8> = (0..256)
            .map(|value| ((value * 29 + value / 7) & 0xff) as u8)
            .collect();
        let (spool, _context) = source(&bytes);
        let mut data = SpoolDataSource::new(&spool).unwrap();
        let budget = MemoryBudget::new(64 * 1024 * 1024);
        let boundaries = m2_boundaries(&mut data, 0, bytes.len() as u64, 32, 32, &budget).unwrap();
        assert_eq!(
            boundaries.as_slice(),
            independent_m2_boundaries(&bytes, 32, 32)
        );
        assert!(boundaries.len() > 1);
    }

    #[test]
    fn m2_injected_first_and_consecutive_hits_verify_reset_state_and_ownership() {
        let block_start = 17u64;
        let block_end = block_start + 96;
        let bytes: Vec<u8> = (0..(block_end as usize + 1))
            .map(|value| ((value * 13 + 5) & 0xff) as u8)
            .collect();
        let (spool, _context) = source(&bytes);
        let mut data = SpoolDataSource::new(&spool).unwrap();
        let budget = MemoryBudget::new(64 * 1024 * 1024);
        let mut observed = Vec::new();
        let actual = m2_boundaries_core(
            &mut data,
            block_start,
            block_end,
            &budget,
            |position, hash, chunk_len| {
                observed.push((position, hash, chunk_len));
                chunk_len >= 32 && (position == block_start + 32 || position == block_start + 64)
            },
        )
        .unwrap();
        assert_eq!(
            observed,
            independent_m2_states(&bytes, block_start, block_end)
        );
        assert_eq!(
            actual.as_slice(),
            &[
                (block_start, block_start + 32),
                (block_start + 32, block_start + 64),
                (block_start + 64, block_end),
            ]
        );
        assert_eq!(observed[31].0, block_start + 31);
        assert_eq!(observed[32].0, block_start + 32);
        assert_eq!(observed[63].0, block_start + 63);
        assert_eq!(observed[64].0, block_start + 64);
    }

    fn independent_m2_states(
        bytes: &[u8],
        block_start: u64,
        block_end: u64,
    ) -> Vec<(u64, u64, u64)> {
        let mut result = Vec::new();
        let mut predict = [0u8; 256];
        let mut previous = 0u8;
        let mut hash = 0u32;
        let mut last_boundary = block_start;
        for position in block_start..block_end {
            let byte = bytes[position as usize];
            let multiplier = if byte != predict[previous as usize] {
                271_828_182
            } else {
                314_159_265
            };
            hash = hash
                .wrapping_add(u32::from(byte) + 1)
                .wrapping_mul(multiplier);
            predict[previous as usize] = byte;
            previous = byte;
            result.push((position, u64::from(hash), position - last_boundary));
            if position - last_boundary >= 32
                && (position == block_start + 32 || position == block_start + 64)
            {
                last_boundary = position;
                predict = [0; 256];
                previous = 0;
                hash = 0;
            }
        }
        result
    }

    fn independent_m2_boundaries(
        bytes: &[u8],
        min_chunk: u64,
        target_chunk: u64,
    ) -> Vec<(u64, u64)> {
        let mut result = Vec::new();
        let mut predict = [0u8; 256];
        let mut previous = 0u8;
        let mut hash = 0u32;
        let threshold = u32::MAX - u32::MAX / target_chunk as u32;
        let mut last = 0u64;
        for (position, &byte) in bytes.iter().enumerate() {
            let multiplier = if byte != predict[previous as usize] {
                271_828_182
            } else {
                314_159_265
            };
            hash = hash
                .wrapping_add(u32::from(byte) + 1)
                .wrapping_mul(multiplier);
            predict[previous as usize] = byte;
            previous = byte;
            let position = position as u64;
            if hash > threshold && position - last >= min_chunk {
                result.push((last, position));
                last = position;
                predict = [0; 256];
                previous = 0;
                hash = 0;
            }
        }
        if last < bytes.len() as u64 {
            result.push((last, bytes.len() as u64));
        }
        result
    }

    fn boundary_ranges(bytes: &[u8], method: CdcMethod) -> Vec<(u64, u64)> {
        let (spool, _context) = source(bytes);
        let mut data = SpoolDataSource::new(&spool).unwrap();
        let budget = MemoryBudget::new(64 * 1024 * 1024);
        let mut ranges = Vec::new();
        for block_start in (0..bytes.len()).step_by(1024) {
            let start = block_start as u64;
            let end = (block_start + 1024).min(bytes.len()) as u64;
            let block = match method {
                CdcMethod::M1 => m1_boundaries(&mut data, start, end, 32, 32, &budget).unwrap(),
                CdcMethod::M2 => m2_boundaries(&mut data, start, end, 32, 32, &budget).unwrap(),
            };
            ranges.extend_from_slice(block.as_slice());
        }
        ranges
    }

    fn collision_input(method: CdcMethod) -> Vec<u8> {
        let base = b"0123456789abcdef".repeat(512);
        for position in (base.len() - 1024..base.len()).rev() {
            let mut candidate = base.clone();
            candidate[position] ^= 1;
            let ranges = boundary_ranges(&candidate, method);
            let mut equal = false;
            let mut unequal = false;
            for (target_index, &(target_start, target_end)) in ranges.iter().enumerate() {
                let target = &candidate[target_start as usize..target_end as usize];
                for &(source_start, source_end) in &ranges[..target_index] {
                    if source_end - source_start != target_end - target_start {
                        continue;
                    }
                    if candidate[source_start as usize..source_end as usize] == *target {
                        equal = true;
                    } else {
                        unequal = true;
                    }
                }
            }
            if equal && unequal {
                return candidate;
            }
        }
        panic!("could not construct a same-length CDC collision corpus");
    }

    #[test]
    fn m1_and_m2_find_equal_chunks_with_collision_safe_confirmation() {
        for method in [CdcMethod::M1, CdcMethod::M2] {
            let bytes = collision_input(method);
            let mut config = CompressionConfig::for_method(method.expected_method());
            config.block_size = 1024;
            config.min_match = 32;
            config.target_chunk = Some(32);
            let (spool, context) = source(&bytes);
            let mut digest_calls = Vec::new();
            let candidates = find_matches_spooled_with_digest(
                &spool,
                &config,
                &context,
                method,
                |_source, position, length| {
                    digest_calls.push((position, length, cdc_key(length, &[7; 16])));
                    Ok([7; 16])
                },
            )
            .unwrap();
            assert!(!digest_calls.is_empty());
            assert!(digest_calls.iter().all(|&(_, length, key)| {
                key[..8] == length.to_le_bytes() && key[8..] == [7; 16]
            }));
            let mut unequal_pair = None;
            let mut equal_pair = None;
            for (index, &(first, length, _)) in digest_calls.iter().enumerate() {
                for &(second, second_length, _) in &digest_calls[index + 1..] {
                    if length == second_length {
                        let first_end = (first + length) as usize;
                        let second_end = (second + length) as usize;
                        if bytes[first as usize..first_end] == bytes[second as usize..second_end] {
                            equal_pair.get_or_insert((first, second, length));
                        } else {
                            unequal_pair.get_or_insert((first, second, length));
                        }
                    }
                }
            }
            let (equal_source, equal_target, equal_length) = equal_pair.expect("equal chunks");
            assert!(candidates.iter().any(|candidate| {
                candidate.src == equal_source
                    && candidate.dst == equal_target
                    && candidate.len == equal_length
            }));
            let (unequal_source, unequal_target, unequal_length) =
                unequal_pair.expect("unequal same-length chunks");
            assert!(!candidates.iter().any(|candidate| {
                candidate.src == unequal_source
                    && candidate.dst == unequal_target
                    && candidate.len == unequal_length
            }));
            let ranges = boundary_ranges(&bytes, method);
            let mut expected = Vec::new();
            for (target_index, &(target_start, target_end)) in ranges.iter().enumerate() {
                let target = &bytes[target_start as usize..target_end as usize];
                if target_end - target_start < 32 {
                    continue;
                }
                let mut sources = ranges[..target_index]
                    .iter()
                    .copied()
                    .filter(|&(source_start, source_end)| {
                        source_end - source_start == target_end - target_start
                            && bytes[source_start as usize..source_end as usize] == *target
                    })
                    .collect::<Vec<_>>();
                sources.sort_unstable_by_key(|source| std::cmp::Reverse(source.0));
                for (source_start, _) in sources {
                    expected.push(MatchCandidate {
                        src: source_start,
                        dst: target_start,
                        len: target_end - target_start,
                        insertion_ordinal: expected.len() as u64,
                    });
                }
            }
            assert_eq!(candidates.as_slice(), expected.as_slice());
            assert!(candidates.iter().all(|candidate| candidate.len >= 32));
            assert!(candidates.iter().all(|candidate| {
                candidate.src < candidate.dst && candidate.dst + candidate.len <= bytes.len() as u64
            }));
            assert!(candidates.iter().all(|candidate| {
                bytes[candidate.src as usize..(candidate.src + candidate.len) as usize]
                    == bytes[candidate.dst as usize..(candidate.dst + candidate.len) as usize]
            }));
        }
    }
}
