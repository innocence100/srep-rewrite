use std::collections::HashSet;
use std::io::Read;

use super::snapshot::{InputSnapshot, extend_candidate};
use super::source::{DataSource, SpoolDataSource};
use crate::candidate_index::{CandidateIndex, IndexEntry, new_candidate_index};
use crate::codec::{InputSpool, spool_input};
use crate::config::CompressionConfig;
use crate::error::{Error, Result};
use crate::match_ir::MatchCandidate;
use crate::polynomial::polynomial_hash;
use crate::resource::{BudgetedVec, ResourceContext};

const COMPARE_CHUNK: usize = 4096;
const CANONICAL_KEY_RESERVATION: u64 = 128;

/// Emission dedup filter keyed by canonical `(src, dst, len)` triple.
///
/// This is **not** an extension memo.  Each m0 candidate extension is fully
/// independent and uses no cross-candidate interval state; the finder emits
/// every candidate it discovers through its exact enumeration phases.  The
/// filter suppresses the final emission of a triple that has already been
/// emitted, preventing unbounded memory growth for highly repetitive inputs.
/// The authoritative dedup and minimum-ordinal selection happen in
/// `reference::canonicalize_candidates` before exact periodic validation.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct CandidateKey {
    src: u64,
    dst: u64,
    len: u64,
}

pub(crate) struct CandidateEmissionFilter {
    seen: HashSet<CandidateKey>,
    reservation: crate::resource::Reservation,
}

impl CandidateEmissionFilter {
    pub(crate) fn new(context: &ResourceContext) -> Result<Self> {
        Ok(Self {
            seen: HashSet::new(),
            reservation: context.memory.reserve(0)?,
        })
    }

    pub(crate) fn first(&mut self, candidate: MatchCandidate) -> Result<bool> {
        let key = CandidateKey {
            src: candidate.src,
            dst: candidate.dst,
            len: candidate.len,
        };
        if self.seen.contains(&key) {
            return Ok(false);
        }
        self.reservation.grow(CANONICAL_KEY_RESERVATION)?;
        self.seen.try_reserve(1).map_err(|error| {
            Error::memory_limit(format!(
                "candidate emission filter allocation failed: {error}"
            ))
        })?;
        self.seen.insert(key);
        Ok(true)
    }
}

pub fn find_matches_m0<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    let spool = spool_input(input, &config.resources, context)?;
    find_matches_m0_spooled(&spool, config, context)
}

pub fn find_matches_m0_with_resources<R: Read>(
    input: R,
    config: &CompressionConfig,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    let context = ResourceContext::with_resources(&config.resources)?;
    find_matches_m0(input, config, &context)
}

pub fn find_matches_m0_with_context<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches_m0(input, config, context)
}

pub(crate) fn find_matches_m0_spooled(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    let parameters = M0Parameters {
        min_match: config.min_match,
        region: (config.min_match / 8).max(1),
        max_distance: config.max_distance,
    };
    find_matches_m0_spooled_with_parameters(spool, &parameters, context, 0, hash_at_dyn)
}

pub(crate) fn find_matches_m0_spooled_compact(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    let parameters = M0Parameters {
        min_match: config.min_match,
        region: (config.min_match / 8).max(1),
        max_distance: config.max_distance,
    };
    let mut output = BudgetedVec::new(&context.memory)?;
    let mut filter = CandidateEmissionFilter::new(context)?;
    find_matches_m0_spooled_with_parameters_into(
        spool,
        &parameters,
        context,
        0,
        hash_at_dyn,
        &mut output,
        Some(&mut filter),
    )?;
    Ok(output)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct M0Parameters {
    pub(crate) min_match: u64,
    pub(crate) region: u64,
    pub(crate) max_distance: Option<u64>,
}

pub(crate) fn find_matches_m0_spooled_with_parameters(
    spool: &InputSpool,
    parameters: &M0Parameters,
    context: &ResourceContext,
    ordinal_start: u64,
    hash_fn: fn(&mut dyn DataSource, u64, u64) -> Result<u64>,
) -> Result<BudgetedVec<MatchCandidate>> {
    let mut output = BudgetedVec::new(&context.memory)?;
    find_matches_m0_spooled_with_parameters_into(
        spool,
        parameters,
        context,
        ordinal_start,
        hash_fn,
        &mut output,
        None,
    )?;
    Ok(output)
}

pub(crate) fn find_matches_m0_spooled_with_parameters_into(
    spool: &InputSpool,
    parameters: &M0Parameters,
    context: &ResourceContext,
    ordinal_start: u64,
    hash_fn: fn(&mut dyn DataSource, u64, u64) -> Result<u64>,
    output: &mut BudgetedVec<MatchCandidate>,
    mut emission_filter: Option<&mut CandidateEmissionFilter>,
) -> Result<()> {
    let region = parameters.region;
    if region == 0 {
        return Err(Error::invalid_config("m0 region must be positive"));
    }
    let mut source = SpoolDataSource::new(spool)?;
    let snapshot = InputSnapshot::try_new(spool, context)?;
    let mut representatives = BudgetedVec::new(&context.memory)?;
    let mut index = new_candidate_index(context, &crate::config::ResourceConfig::default())?;
    let eligible_starts = if spool.len < region {
        0
    } else {
        spool
            .len
            .checked_sub(region)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| Error::invalid_match("m0 eligible start count overflows"))?
    };
    let region_count = eligible_starts.div_ceil(region);
    for region_id in 0..region_count {
        let region_start = region_id
            .checked_mul(region)
            .ok_or_else(|| Error::invalid_match("m0 region start overflows"))?;
        let region_end = region_start
            .checked_add(region)
            .ok_or_else(|| Error::invalid_match("m0 region end overflows"))?
            .min(eligible_starts);
        let mut best: Option<(u64, u64)> = None;
        for position in region_start..region_end {
            let hash = hash_fn(&mut source, position, region)?;
            if best.is_none_or(|(best_hash, best_position)| {
                hash > best_hash || (hash == best_hash && position < best_position)
            }) {
                best = Some((hash, position));
            }
        }
        if let Some((hash, position)) = best {
            debug_assert!(
                representatives
                    .as_slice()
                    .last()
                    .is_none_or(|&(previous, _)| previous < position)
            );
            representatives.push((position, hash))?;
        }
    }
    debug_assert!(
        representatives
            .as_slice()
            .windows(2)
            .all(|pair| pair[0].0 < pair[1].0)
    );

    let mut next_representative = 0usize;
    let mut ordinal = 0u64;
    for target in 0..eligible_starts {
        while next_representative < representatives.len()
            && representatives[next_representative].0 < target
            && representative_complete(
                representatives[next_representative].0,
                region,
                eligible_starts,
            ) <= target
        {
            let (position, hash) = representatives[next_representative];
            index.insert(IndexEntry::new(
                0,
                &hash.to_le_bytes(),
                position,
                next_representative as u64,
                &[],
            )?)?;
            next_representative += 1;
        }
        let hash = hash_fn(&mut source, target, region)?;
        index.for_each_candidate(
            0,
            &hash.to_le_bytes(),
            target,
            parameters.max_distance.unwrap_or(0),
            &mut |entry| {
                if seed_equal(&mut source, entry.position, target, region)? {
                    let (src, dst, len) = extend_candidate(
                        &mut source,
                        entry.position,
                        target,
                        region,
                        snapshot.as_ref(),
                    )?;
                    if len >= parameters.min_match {
                        let candidate = MatchCandidate {
                            src,
                            dst,
                            len,
                            insertion_ordinal: ordinal_start.checked_add(ordinal).ok_or_else(
                                || Error::invalid_match("m0 insertion ordinal overflows"),
                            )?,
                        };
                        ordinal = ordinal.checked_add(1).ok_or_else(|| {
                            Error::invalid_match("m0 insertion ordinal overflows")
                        })?;
                        let keep = if let Some(filter) = emission_filter.as_deref_mut() {
                            filter.first(candidate)?
                        } else {
                            true
                        };
                        if keep {
                            output.push(candidate)?;
                        }
                    }
                }
                Ok(())
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
fn constant_hash_at(source: &mut dyn DataSource, position: u64, length: u64) -> Result<u64> {
    let mut bytes = [0u8; 8];
    let count =
        usize::try_from(length).map_err(|_| Error::invalid_match("test hash length overflows"))?;
    let count = count.min(bytes.len());
    source.read_at(position, &mut bytes[..count])?;
    Ok(0)
}

fn representative_complete(position: u64, region: u64, eligible_end: u64) -> u64 {
    position
        .checked_div(region)
        .and_then(|region_id| region_id.checked_add(1))
        .and_then(|region_id| region_id.checked_mul(region))
        .unwrap_or(eligible_end)
        .min(eligible_end)
}

fn hash_at(source: &mut dyn DataSource, position: u64, length: u64) -> Result<u64> {
    let length = usize::try_from(length)
        .map_err(|_| Error::memory_limit("m0 region exceeds platform limits"))?;
    let mut bytes = [0u8; 8];
    if length > bytes.len() {
        return hash_at_slow(source, position, length);
    }
    source.read_at(position, &mut bytes[..length])?;
    Ok(polynomial_hash(&bytes[..length]))
}

pub(crate) fn hash_at_dyn(source: &mut dyn DataSource, position: u64, length: u64) -> Result<u64> {
    hash_at(source, position, length)
}

fn hash_at_slow(source: &mut dyn DataSource, position: u64, length: usize) -> Result<u64> {
    let mut hash = 0u64;
    let mut bytes = [0u8; COMPARE_CHUNK];
    let mut offset = 0usize;
    while offset < length {
        let count = (length - offset).min(bytes.len());
        let absolute = position
            .checked_add(
                u64::try_from(offset)
                    .map_err(|_| Error::invalid_match("m0 hash offset exceeds u64"))?,
            )
            .ok_or_else(|| Error::invalid_match("m0 hash position overflows"))?;
        source.read_at(absolute, &mut bytes[..count])?;
        for &byte in &bytes[..count] {
            hash = hash.wrapping_mul(153_191).wrapping_add(byte as u64);
        }
        offset += count;
    }
    Ok(hash)
}

fn seed_equal(
    source: &mut impl DataSource,
    source_position: u64,
    target: u64,
    length: u64,
) -> Result<bool> {
    Ok(compare_contiguous(source, source_position, target, length)?.is_none())
}

fn compare_contiguous(
    source: &mut impl DataSource,
    first: u64,
    second: u64,
    length: u64,
) -> Result<Option<u64>> {
    let mut first_bytes = [0u8; COMPARE_CHUNK];
    let mut second_bytes = [0u8; COMPARE_CHUNK];
    let mut offset = 0u64;
    while offset < length {
        let count = usize::try_from((length - offset).min(COMPARE_CHUNK as u64))
            .map_err(|_| Error::invalid_match("m0 comparison length exceeds platform limits"))?;
        let first_at = first
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("m0 first comparison position overflows"))?;
        let second_at = second
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("m0 second comparison position overflows"))?;
        source.read_at(first_at, &mut first_bytes[..count])?;
        source.read_at(second_at, &mut second_bytes[..count])?;
        if first_bytes[..count] != second_bytes[..count] {
            let mismatch = first_bytes[..count]
                .iter()
                .zip(&second_bytes[..count])
                .position(|(a, b)| a != b)
                .ok_or_else(|| Error::invalid_match("m0 comparison mismatch is unavailable"))?;
            return Ok(Some(offset + mismatch as u64));
        }
        offset += count as u64;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_hash_collision_uses_production_seed_and_extension_path() {
        let mut config = CompressionConfig::for_method(crate::config::Method::M0Rep);
        config.min_match = 8;
        let context = ResourceContext::with_resources(&config.resources).unwrap();
        let spool = crate::codec::spool_input(
            &b"abcdefghABCDEFGHabcdefgh"[..],
            &config.resources,
            &context,
        )
        .unwrap();
        let candidates = find_matches_m0_spooled_with_parameters(
            &spool,
            &M0Parameters {
                min_match: config.min_match,
                region: (config.min_match / 8).max(1),
                max_distance: config.max_distance,
            },
            &context,
            0,
            constant_hash_at,
        )
        .unwrap();
        assert!(
            candidates.iter().any(|candidate| {
                candidate.src == 0 && candidate.dst == 16 && candidate.len >= 8
            })
        );
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.src == 0 && candidate.dst == 8)
        );
    }

    #[test]
    fn input_snapshot_allocation_failure_selects_exact_fallback() {
        let input = vec![0u8; 4096];
        let config = CompressionConfig::for_method(crate::config::Method::M0Rep);
        let spool_context = ResourceContext::with_resources(&config.resources).unwrap();
        let spool =
            crate::codec::spool_input(&input[..], &config.resources, &spool_context).unwrap();
        let fallback_context = ResourceContext::from_limits(1, config.resources.temp_limit);

        assert!(
            InputSnapshot::try_new(&spool, &fallback_context)
                .unwrap()
                .is_none()
        );
        drop(spool);
        assert_eq!(spool_context.memory.current(), 0);
        assert_eq!(spool_context.temp.current(), 0);
        assert_eq!(fallback_context.memory.current(), 0);
    }

    #[test]
    fn input_snapshot_extension_matches_file_backed_extension() {
        let input = b"0123456789abcdef".repeat(1024);
        let config = CompressionConfig::for_method(crate::config::Method::M0Rep);
        let context = ResourceContext::with_resources(&config.resources).unwrap();
        let spool = crate::codec::spool_input(&input[..], &config.resources, &context).unwrap();
        let snapshot = InputSnapshot::try_new(&spool, &context)
            .unwrap()
            .expect("small input should fit the snapshot budget");
        let mut source = SpoolDataSource::new(&spool).unwrap();

        let file_backed = extend_candidate(&mut source, 0, 4096, 512, None).unwrap();
        let snapshot_backed = extend_candidate(&mut source, 0, 4096, 512, Some(&snapshot)).unwrap();

        assert_eq!(snapshot_backed, file_backed);
    }
}
