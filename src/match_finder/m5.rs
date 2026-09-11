use std::io::Read;

use super::fixed::append_overlay;
use super::m0::CandidateEmissionFilter;
use super::snapshot::{InputSnapshot, extend_candidate};
use super::source::{DataSource, SpoolDataSource};
use crate::candidate_index::{CandidateIndex, IndexEntry, new_candidate_index};
use crate::codec::{InputSpool, spool_input};
use crate::config::{CompressionConfig, Method, m5_seed_size};
use crate::error::{Error, Result};
use crate::match_ir::MatchCandidate;
use crate::polynomial::polynomial_hash;
use crate::resource::{BudgetedVec, ResourceContext};

const COMPARE_CHUNK: usize = 4096;
const M5_KEY_KIND: u8 = 5;

pub fn find_matches_m5<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    let spool = spool_input(input, &config.resources, context)?;
    find_matches_spooled(&spool, config, context)
}

pub fn find_matches_m5_with_resources<R: Read>(
    input: R,
    config: &CompressionConfig,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    let context = ResourceContext::with_resources(&config.resources)?;
    find_matches_m5(input, config, &context)
}

pub fn find_matches_m5_with_context<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches_m5(input, config, context)
}

pub(crate) fn find_matches_spooled(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    if config.method != Method::M5Exhaustive {
        return Err(Error::invalid_config(
            "m5 finder method does not match configuration",
        ));
    }
    let base = find_m5_spooled_with_functions(
        spool,
        config,
        context,
        polynomial_hash_at,
        polynomial_hash_at,
        compare_contiguous,
        true,
    )?;
    let mut combined = append_overlay(spool, config, context, base)?;
    deduplicate_candidates(&mut combined);
    Ok(combined)
}

fn find_m5_spooled_with_functions<F, S, C>(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    mut full_hash_fn: F,
    mut slice_hash_fn: S,
    mut compare_fn: C,
    accelerate_keys: bool,
) -> Result<BudgetedVec<MatchCandidate>>
where
    F: FnMut(&mut dyn DataSource, u64, u64) -> Result<u64>,
    S: FnMut(&mut dyn DataSource, u64, u64) -> Result<u64>,
    C: FnMut(&mut dyn DataSource, u64, u64, u64) -> Result<bool>,
{
    let minimum = config.min_match;
    let seed = m5_seed_size(minimum)?;
    let target_count = if spool.len < seed {
        0
    } else {
        spool
            .len
            .checked_sub(seed)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| Error::invalid_match("m5 target count overflows"))?
    };
    let mut source = SpoolDataSource::new(spool)?;
    let snapshot = InputSnapshot::try_new(spool, context)?;
    let mut index = new_candidate_index(context, &config.resources)?;
    let mut output = BudgetedVec::new(&context.memory)?;
    let mut emission_filter = CandidateEmissionFilter::new(context)?;
    let mut source_ordinal = 0u64;
    let mut candidate_ordinal = 0u64;
    let mut next_source = 0u64;

    for target in 0..target_count {
        while next_source < target {
            let source_end = next_source
                .checked_add(seed)
                .ok_or_else(|| Error::invalid_match("m5 source seed endpoint overflows"))?;
            if source_end > source.len() {
                break;
            }
            let full_hash = hash_with_snapshot(
                accelerate_keys.then_some(snapshot.as_ref()).flatten(),
                &mut source,
                next_source,
                seed,
                &mut full_hash_fn,
            )?;
            let metadata = packed_slice_metadata_with_snapshot(
                accelerate_keys.then_some(snapshot.as_ref()).flatten(),
                &mut source,
                next_source,
                seed,
                &mut slice_hash_fn,
            )?;
            index.insert(IndexEntry::new(
                M5_KEY_KIND,
                &m5_key(seed, full_hash),
                next_source,
                source_ordinal,
                &metadata.to_le_bytes(),
            )?)?;
            source_ordinal = source_ordinal
                .checked_add(1)
                .ok_or_else(|| Error::invalid_match("m5 source ordinal overflows"))?;
            next_source = next_source
                .checked_add(seed)
                .ok_or_else(|| Error::invalid_match("m5 source grid position overflows"))?;
        }

        let target_hash = hash_with_snapshot(
            accelerate_keys.then_some(snapshot.as_ref()).flatten(),
            &mut source,
            target,
            seed,
            &mut full_hash_fn,
        )?;
        let target_metadata = packed_slice_metadata_with_snapshot(
            accelerate_keys.then_some(snapshot.as_ref()).flatten(),
            &mut source,
            target,
            seed,
            &mut slice_hash_fn,
        )?;
        let target_metadata_bytes = target_metadata.to_le_bytes();
        index.for_each_candidate(
            M5_KEY_KIND,
            &m5_key(seed, target_hash),
            target,
            config.max_distance.unwrap_or(0),
            &mut |entry| {
                if entry.metadata_slice() == target_metadata_bytes
                    && compare_with_snapshot(
                        accelerate_keys.then_some(snapshot.as_ref()).flatten(),
                        &mut source,
                        entry.position,
                        target,
                        seed,
                        &mut compare_fn,
                    )?
                {
                    let (src, dst, length) = extend_candidate(
                        &mut source,
                        entry.position,
                        target,
                        seed,
                        snapshot.as_ref(),
                    )?;
                    if length >= minimum {
                        let candidate = MatchCandidate {
                            src,
                            dst,
                            len: length,
                            insertion_ordinal: candidate_ordinal,
                        };
                        candidate_ordinal = candidate_ordinal.checked_add(1).ok_or_else(|| {
                            Error::invalid_match("m5 insertion ordinal overflows")
                        })?;
                        if emission_filter.first(candidate)? {
                            output.push(candidate)?;
                        }
                    }
                }
                Ok(())
            },
        )?;
    }

    deduplicate_candidates(&mut output);
    Ok(output)
}

fn deduplicate_candidates(output: &mut BudgetedVec<MatchCandidate>) {
    output.sort_unstable_by(|a, b| {
        (a.src, a.dst, a.len, a.insertion_ordinal).cmp(&(b.src, b.dst, b.len, b.insertion_ordinal))
    });
    output.dedup_by(|a, b| {
        if (a.src, a.dst, a.len) == (b.src, b.dst, b.len) {
            a.insertion_ordinal = a.insertion_ordinal.min(b.insertion_ordinal);
            true
        } else {
            false
        }
    });
    output.sort_unstable_by_key(|candidate| candidate.insertion_ordinal);
}

pub fn packed_slice_metadata(bytes: &[u8]) -> u32 {
    let quotient = bytes.len() / 8;
    let remainder = bytes.len() % 8;
    let mut metadata = 0u32;
    let mut offset = 0usize;
    for slice in 0..8 {
        let slice_len = quotient + usize::from(slice < remainder);
        let hash = polynomial_hash(&bytes[offset..offset + slice_len]);
        metadata |= ((hash as u32) & 0x0f) << (slice * 4);
        offset += slice_len;
    }
    metadata
}

fn hash_with_snapshot<F>(
    snapshot: Option<&InputSnapshot>,
    source: &mut impl DataSource,
    position: u64,
    length: u64,
    hash_fn: &mut F,
) -> Result<u64>
where
    F: FnMut(&mut dyn DataSource, u64, u64) -> Result<u64>,
{
    if let Some(snapshot) = snapshot
        && let Some(hash) = snapshot.polynomial_hash_at(position, length)
    {
        return Ok(hash);
    }
    hash_fn(source, position, length)
}

fn packed_slice_metadata_with_snapshot<S>(
    snapshot: Option<&InputSnapshot>,
    source: &mut impl DataSource,
    position: u64,
    length: u64,
    slice_hash_fn: &mut S,
) -> Result<u32>
where
    S: FnMut(&mut dyn DataSource, u64, u64) -> Result<u64>,
{
    if let Some(snapshot) = snapshot
        && let Some(metadata) = snapshot.packed_slice_metadata_at(position, length)
    {
        return Ok(metadata);
    }
    let quotient = length / 8;
    let remainder = length % 8;
    let mut metadata = 0u32;
    let mut offset = 0u64;
    for slice in 0..8u64 {
        let slice_len = quotient
            .checked_add(u64::from(slice < remainder))
            .ok_or_else(|| Error::invalid_match("m5 slice length overflows"))?;
        let slice_position = position
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("m5 slice position overflows"))?;
        let hash = slice_hash_fn(source, slice_position, slice_len)?;
        metadata |= ((hash as u32) & 0x0f) << (slice as u32 * 4);
        offset = offset
            .checked_add(slice_len)
            .ok_or_else(|| Error::invalid_match("m5 slice offset overflows"))?;
    }
    Ok(metadata)
}

fn compare_with_snapshot<C>(
    snapshot: Option<&InputSnapshot>,
    source: &mut impl DataSource,
    first: u64,
    second: u64,
    length: u64,
    compare_fn: &mut C,
) -> Result<bool>
where
    C: FnMut(&mut dyn DataSource, u64, u64, u64) -> Result<bool>,
{
    if let Some(snapshot) = snapshot
        && let Some(equal) = snapshot.compare_contiguous(first, second, length)
    {
        return Ok(equal);
    }
    compare_fn(source, first, second, length)
}

fn m5_key(seed: u64, hash: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&seed.to_le_bytes());
    key[8..].copy_from_slice(&hash.to_le_bytes());
    key
}

fn polynomial_hash_at(source: &mut dyn DataSource, position: u64, length: u64) -> Result<u64> {
    let mut hash = 0u64;
    let mut buffer = [0u8; COMPARE_CHUNK];
    let mut offset = 0u64;
    while offset < length {
        let count = usize::try_from((length - offset).min(COMPARE_CHUNK as u64))
            .map_err(|_| Error::memory_limit("m5 hash length exceeds platform limits"))?;
        source.read_at(
            position
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("m5 hash position overflows"))?,
            &mut buffer[..count],
        )?;
        for &byte in &buffer[..count] {
            hash = hash.wrapping_mul(153_191).wrapping_add(u64::from(byte));
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("m5 hash offset overflows"))?;
    }
    Ok(hash)
}

fn compare_contiguous(
    source: &mut (dyn DataSource + '_),
    first: u64,
    second: u64,
    length: u64,
) -> Result<bool> {
    let mut first_bytes = [0u8; COMPARE_CHUNK];
    let mut second_bytes = [0u8; COMPARE_CHUNK];
    let mut offset = 0u64;
    while offset < length {
        let count = usize::try_from((length - offset).min(COMPARE_CHUNK as u64))
            .map_err(|_| Error::memory_limit("m5 comparison exceeds platform limits"))?;
        source.read_at(
            first
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("m5 source comparison overflows"))?,
            &mut first_bytes[..count],
        )?;
        source.read_at(
            second
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("m5 target comparison overflows"))?,
            &mut second_bytes[..count],
        )?;
        if first_bytes[..count] != second_bytes[..count] {
            return Ok(false);
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("m5 comparison offset overflows"))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::Cursor;

    use super::*;
    use crate::config::ResourceConfig;

    #[test]
    fn packed_metadata_splits_first_remainder_slices_and_keeps_empty_slices() {
        assert_eq!(packed_slice_metadata(&[]), 0);
        assert_eq!(packed_slice_metadata(&[1]), 1);
        assert_eq!(packed_slice_metadata(&[1, 2, 3, 4, 5, 6, 7]), 0x7654321);
        assert_eq!(packed_slice_metadata(&[1, 2, 3, 4, 5, 6, 7, 8]), 0x87654321);
        assert_eq!(
            packed_slice_metadata(&(1u8..=16).collect::<Vec<_>>()),
            0x99999999
        );
    }

    #[test]
    fn empty_input_has_no_m5_targets() {
        let config = CompressionConfig::for_method(Method::M5Exhaustive);
        let context = ResourceContext::with_resources(&ResourceConfig::default()).unwrap();
        let matches = find_matches_m5(Cursor::new([]), &config, &context).unwrap();
        assert!(matches.is_empty());
    }

    fn test_config(minimum: u64) -> CompressionConfig {
        let mut config = CompressionConfig::for_method(Method::M5Exhaustive);
        config.min_match = minimum;
        config.block_size = 1024;
        config.resources.memory = 64 * 1024 * 1024;
        config
    }

    fn test_spool(input: &[u8]) -> (InputSpool, ResourceContext) {
        let config = test_config(8);
        let context = ResourceContext::with_resources(&config.resources).unwrap();
        let spool = crate::codec::spool_input(input, &config.resources, &context).unwrap();
        (spool, context)
    }

    fn constant_hash(_source: &mut dyn DataSource, _position: u64, _length: u64) -> Result<u64> {
        Ok(0)
    }

    #[test]
    fn metadata_filter_precedes_exact_confirmation_and_collision_reaches_confirmation() {
        let (spool, context) = test_spool(b"abcdefghabcdefghabcdefgh");
        let exact_comparisons = Cell::new(0u64);
        let mut config = test_config(8);
        config.min_match = 8;
        let result = find_m5_spooled_with_functions(
            &spool,
            &config,
            &context,
            constant_hash,
            |_, position, _| Ok(u64::from(position >= 8)),
            |source, first, second, length| {
                exact_comparisons.set(exact_comparisons.get() + 1);
                compare_contiguous(source, first, second, length)
            },
            false,
        )
        .unwrap();
        assert!(exact_comparisons.get() > 0);
        assert!(
            result
                .iter()
                .any(|candidate| candidate.src == 0 && candidate.dst == 8)
        );
        assert!(
            !result
                .iter()
                .any(|candidate| candidate.src == 8 && candidate.dst == 16)
        );
    }

    #[test]
    fn equal_hash_and_metadata_collision_is_rejected_by_exact_seed_compare() {
        let (spool, context) = test_spool(b"abcdWXYZ");
        let exact_comparisons = Cell::new(0u64);
        let config = test_config(8);
        let result = find_m5_spooled_with_functions(
            &spool,
            &config,
            &context,
            constant_hash,
            constant_hash,
            |source, first, second, length| {
                exact_comparisons.set(exact_comparisons.get() + 1);
                compare_contiguous(source, first, second, length)
            },
            false,
        )
        .unwrap();
        assert!(exact_comparisons.get() > 0);
        assert!(result.is_empty());
    }

    #[test]
    fn snapshot_prefix_hashes_match_polynomial_and_fallback_is_exact() {
        let input = b"abcdefghabcdefghabcdefghXYZXYZXYZXYZ".repeat(8);
        let (spool, context) = test_spool(&input);
        let snapshot = InputSnapshot::try_new(&spool, &context)
            .unwrap()
            .expect("small input should snapshot");
        let seed = 8u64;
        for position in 0..=input.len() as u64 - seed {
            let expected =
                polynomial_hash(&input[position as usize..position as usize + seed as usize]);
            assert_eq!(
                snapshot.polynomial_hash_at(position, seed),
                Some(expected),
                "position={position}"
            );
            assert_eq!(
                snapshot.packed_slice_metadata_at(position, seed),
                Some(packed_slice_metadata(
                    &input[position as usize..position as usize + seed as usize]
                ))
            );
        }
        let fallback = ResourceContext::from_limits(1, context.temp.limit());
        assert!(InputSnapshot::try_new(&spool, &fallback).unwrap().is_none());
    }
}
