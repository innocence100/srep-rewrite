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
    let (base, next_ordinal) = find_m5_spooled_with_functions(
        spool,
        config,
        context,
        polynomial_hash_at,
        polynomial_hash_at,
        compare_contiguous,
        true,
    )?;
    let mut combined = append_overlay(spool, config, context, base, next_ordinal)?;
    deduplicate_candidates(&mut combined);
    Ok(combined)
}

#[allow(dead_code)]
pub(crate) fn find_matches_spooled_compact(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches_spooled(spool, config, context)
}

fn find_m5_spooled_with_functions<F, S, C>(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    mut full_hash_fn: F,
    mut slice_hash_fn: S,
    mut compare_fn: C,
    accelerate_keys: bool,
) -> Result<(BudgetedVec<MatchCandidate>, u64)>
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
    Ok((output, candidate_ordinal))
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
    use crate::error::ErrorKind;
    use crate::match_finder::snapshot::{
        TestSnapshotState, test_deny_lces, test_last_snapshot_state, test_snapshot_limit,
        test_snapshot_memory_budget,
    };

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
        .unwrap()
        .0;
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
        .unwrap()
        .0;
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

    #[test]
    fn forced_snapshot_resource_fallback_matches_fully_accelerated_candidates() {
        let input = b"0123456789abcdef".repeat(64);
        let (spool, context) = test_spool(&input);
        let config = test_config(8);
        let accelerated = find_m5_spooled_with_functions(
            &spool,
            &config,
            &context,
            polynomial_hash_at,
            polynomial_hash_at,
            compare_contiguous,
            true,
        )
        .unwrap()
        .0
        .as_slice()
        .to_vec();
        let _limit = test_snapshot_limit(0);
        let fallback = find_m5_spooled_with_functions(
            &spool,
            &config,
            &context,
            polynomial_hash_at,
            polynomial_hash_at,
            compare_contiguous,
            true,
        )
        .unwrap()
        .0
        .as_slice()
        .to_vec();
        assert_eq!(fallback, accelerated);
        assert!(
            fallback
                .iter()
                .any(|candidate| candidate.dst > candidate.src)
        );
    }

    fn independent_m5_oracle(input: &[u8], minimum: usize) -> Vec<MatchCandidate> {
        independent_m5_oracle_with_distance(input, minimum, None).0
    }

    fn independent_m5_oracle_with_distance(
        input: &[u8],
        minimum: usize,
        max_distance: Option<u64>,
    ) -> (Vec<MatchCandidate>, u64) {
        let seed = m5_seed_size(minimum as u64).unwrap() as usize;
        if seed == 0 || input.len() < seed {
            return (Vec::new(), 0);
        }
        let mut result = Vec::new();
        let last_source = input.len() - seed;
        for target in 0..=last_source {
            let sources = (0..=last_source)
                .step_by(seed)
                .filter(|&source| source < target)
                .filter(|&source| match max_distance {
                    Some(limit) if limit != 0 => (target - source) as u64 <= limit,
                    _ => true,
                })
                .collect::<Vec<_>>();
            for source in sources.into_iter().rev() {
                if input[source..source + seed] != input[target..target + seed] {
                    continue;
                }
                let distance = target - source;
                let mut backward = 0;
                while backward < source
                    && backward < target
                    && input[source - backward - 1] == input[target - backward - 1]
                {
                    backward += 1;
                }
                let source_start = source - backward;
                let target_start = target - backward;
                let mut length = backward + seed;
                while target_start + length < input.len()
                    && input[target_start + length] == input[source_start + length % distance]
                {
                    length += 1;
                }
                if length >= minimum {
                    result.push(MatchCandidate {
                        src: source_start as u64,
                        dst: target_start as u64,
                        len: length as u64,
                        insertion_ordinal: result.len() as u64,
                    });
                }
            }
        }
        let raw_count = result.len() as u64;
        result.sort_unstable_by(|a, b| {
            (a.src, a.dst, a.len, a.insertion_ordinal).cmp(&(
                b.src,
                b.dst,
                b.len,
                b.insertion_ordinal,
            ))
        });
        result.dedup_by(|a, b| {
            if (a.src, a.dst, a.len) == (b.src, b.dst, b.len) {
                a.insertion_ordinal = a.insertion_ordinal.min(b.insertion_ordinal);
                true
            } else {
                false
            }
        });
        result.sort_unstable_by_key(|candidate| candidate.insertion_ordinal);
        (result, raw_count)
    }

    #[test]
    fn m5_real_generator_matches_oracle_for_every_snapshot_fallback_state() {
        let input = b"0123456789abcdef0123456789abcdef--0123456789abcdef0123456789abcdef".repeat(4);
        let (spool, context) = test_spool(&input);
        let config = test_config(8);
        let expected = independent_m5_oracle(&input, 8);
        let cases = [
            ("bytes", TestSnapshotState::BytesOnly),
            ("prefix", TestSnapshotState::PrefixTablesOnly),
            ("forward", TestSnapshotState::ForwardLceOnly),
            ("reverse", TestSnapshotState::ReverseLceOnly),
            ("full", TestSnapshotState::FullLce),
        ];
        for (name, expected_state) in cases {
            let _limit = test_snapshot_limit(spool.len);
            let _memory = if name == "bytes" {
                Some(test_snapshot_memory_budget(spool.len))
            } else {
                None
            };
            let _denial = match name {
                "forward" => Some(test_deny_lces(false, true)),
                "reverse" => Some(test_deny_lces(true, false)),
                "full" => Some(test_deny_lces(false, false)),
                "prefix" => Some(test_deny_lces(true, true)),
                "bytes" => Some(test_deny_lces(true, true)),
                _ => unreachable!(),
            };
            let actual = find_m5_spooled_with_functions(
                &spool,
                &config,
                &context,
                polynomial_hash_at,
                polynomial_hash_at,
                compare_contiguous,
                true,
            )
            .unwrap()
            .0
            .as_slice()
            .to_vec();
            assert_eq!(
                test_last_snapshot_state(),
                Some(expected_state),
                "state={name}"
            );
            assert_eq!(actual, expected, "state={name}");
        }
        let _no_snapshot = test_snapshot_limit(0);
        let no_snapshot = find_m5_spooled_with_functions(
            &spool,
            &config,
            &context,
            polynomial_hash_at,
            polynomial_hash_at,
            compare_contiguous,
            true,
        )
        .unwrap()
        .0
        .as_slice()
        .to_vec();
        assert_eq!(no_snapshot, expected, "state=none");
        drop(spool);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }

    #[test]
    fn independent_oracle_enumerates_aligned_source_zero_and_keeps_raw_counter() {
        let zeros = [0u8; 64];
        let (dedup, raw_count) = independent_m5_oracle_with_distance(&zeros, 32, None);
        assert_eq!(raw_count, 80);
        assert!(
            dedup
                .iter()
                .any(|candidate| candidate.src == 0 && candidate.dst == 2 && candidate.len == 62),
            "missing known witness (0,2,62): {dedup:?}"
        );
        let max_retained = dedup
            .iter()
            .map(|candidate| candidate.insertion_ordinal)
            .max()
            .unwrap();
        assert_eq!(max_retained + 1, 48);
        assert!(raw_count > max_retained + 1);

        let nonaligned = [1u8; 9];
        let (min8, min8_raw) = independent_m5_oracle_with_distance(&nonaligned, 8, None);
        assert!(min8_raw > 0);
        assert!(
            min8.iter()
                .any(|candidate| candidate.src == 0 && candidate.dst == 1 && candidate.len == 8),
            "target=1 seed=4 must include aligned source 0: {min8:?}"
        );
        assert!(independent_m5_oracle(b"", 8).is_empty());
        assert!(independent_m5_oracle(b"abc", 8).is_empty());
    }

    #[test]
    fn compact_and_public_m5_emit_identical_exact_triples_and_ordinals() {
        let input = [0u8; 64];
        let (spool, context) = test_spool(&input);
        let config = test_config(32);
        let public = find_matches_spooled(&spool, &config, &context)
            .unwrap()
            .as_slice()
            .to_vec();
        let compact = find_matches_spooled_compact(&spool, &config, &context)
            .unwrap()
            .as_slice()
            .to_vec();
        assert_eq!(compact, public);
        let expected = independent_m5_oracle(&input, 32);
        assert_eq!(public, expected);
        drop(spool);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }

    #[test]
    fn overlay_continues_from_full_pre_dedup_next_ordinal_not_max_retained() {
        let input = [0u8; 64];
        let (spool, context) = test_spool(&input);
        let mut config = test_config(32);
        let (expected, raw_next) = independent_m5_oracle_with_distance(&input, 32, None);
        assert_eq!(raw_next, 80);
        let (base, next_ordinal) = find_m5_spooled_with_functions(
            &spool,
            &config,
            &context,
            polynomial_hash_at,
            polynomial_hash_at,
            compare_contiguous,
            true,
        )
        .unwrap();
        assert_eq!(next_ordinal, raw_next);
        let max_retained = base
            .iter()
            .map(|candidate| candidate.insertion_ordinal)
            .max()
            .unwrap();
        assert_eq!(max_retained + 1, 48);
        assert_eq!(base.as_slice(), expected.as_slice());
        let base_vec = base.as_slice().to_vec();

        config.rep_overlay = Some(crate::config::RepConfig {
            distance: 64,
            min_match: 16,
        });
        let raw_overlay = append_overlay(&spool, &config, &context, base, next_ordinal).unwrap();
        assert!(
            raw_overlay
                .iter()
                .any(|candidate| candidate.insertion_ordinal == raw_next),
            "raw overlay pipeline must start at full next {raw_next}, not max retained {}",
            max_retained + 1
        );
        assert!(
            raw_overlay
                .iter()
                .filter(|candidate| candidate.insertion_ordinal >= raw_next)
                .all(|candidate| candidate.insertion_ordinal >= raw_next)
        );

        let combined = find_matches_spooled(&spool, &config, &context).unwrap();
        let overlay_only: Vec<_> = combined
            .iter()
            .copied()
            .filter(|candidate| {
                !base_vec.iter().any(|base_candidate| {
                    (base_candidate.src, base_candidate.dst, base_candidate.len)
                        == (candidate.src, candidate.dst, candidate.len)
                })
            })
            .collect();
        assert!(
            overlay_only
                .iter()
                .all(|candidate| candidate.insertion_ordinal >= raw_next),
            "surviving overlay-only after exact-triple dedup may start later than {raw_next}: {overlay_only:?}"
        );

        let mut no_overlay = config.clone();
        no_overlay.rep_overlay = None;
        let again = find_matches_spooled(&spool, &no_overlay, &context).unwrap();
        assert_eq!(again.as_slice(), base_vec.as_slice());
        drop(raw_overlay);
        drop(combined);
        drop(again);
        drop(spool);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }

    #[test]
    fn overlay_ordinal_overflow_helper_rejects_max_without_dead_code() {
        let input = [0u8; 64];
        let (spool, context) = test_spool(&input);
        let mut config = test_config(32);
        config.rep_overlay = Some(crate::config::RepConfig {
            distance: 64,
            min_match: 16,
        });
        let (base, next_ordinal) = find_m5_spooled_with_functions(
            &spool,
            &config,
            &context,
            polynomial_hash_at,
            polynomial_hash_at,
            compare_contiguous,
            true,
        )
        .unwrap();
        assert_eq!(next_ordinal, 80);
        let preserved = base.as_slice().to_vec();
        assert!(!preserved.is_empty());
        let error = append_overlay(&spool, &config, &context, base, u64::MAX).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidMatch);
        assert!(
            error.to_string().contains("overflow"),
            "production overlay ordinal must fail checked_add, got {error}"
        );
        assert_eq!(preserved, independent_m5_oracle(&input, 32));
        drop(spool);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }
}
