use std::io::Read;

use crate::candidate_index::{CandidateIndex, IndexEntry, new_candidate_index};
use crate::codec::{InputSpool, spool_input};
use crate::config::{CompressionConfig, Method};
use crate::error::{Error, Result};
use crate::match_ir::MatchCandidate;
use crate::resource::{BudgetedVec, ResourceContext};

use super::m0::{M0Parameters, find_matches_m0_spooled_with_parameters_into};
use super::source::{DataSource, SpoolDataSource};

const COMPARE_CHUNK: usize = 4096;
const M3_KEY_KIND: u8 = 3;
const M4_KEY_KIND: u8 = 4;

pub fn find_matches_m3<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches(input, config, context, Method::M3FixedDigest)
}

pub fn find_matches_m4<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches(input, config, context, Method::M4Reread)
}

pub fn find_matches_m3_with_resources<R: Read>(
    input: R,
    config: &CompressionConfig,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    let context = ResourceContext::with_resources(&config.resources)?;
    find_matches_m3(input, config, &context)
}

pub fn find_matches_m4_with_resources<R: Read>(
    input: R,
    config: &CompressionConfig,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    let context = ResourceContext::with_resources(&config.resources)?;
    find_matches_m4(input, config, &context)
}

pub fn find_matches_m3_with_context<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches_m3(input, config, context)
}

pub fn find_matches_m4_with_context<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
) -> Result<BudgetedVec<MatchCandidate>> {
    find_matches_m4(input, config, context)
}

pub(crate) fn find_matches_spooled(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    method: Method,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    if config.method != method {
        return Err(Error::invalid_config(
            "fixed finder method does not match configuration",
        ));
    }
    let base = match method {
        Method::M3FixedDigest => find_m3_spooled_with_digest(spool, config, context, blake3_at)?,
        Method::M4Reread => find_m4_spooled_with_hash(spool, config, context, polynomial_at)?,
        _ => return Err(Error::invalid_config("method is not a fixed finder")),
    };
    append_overlay(spool, config, context, base)
}

fn find_matches<R: Read>(
    input: R,
    config: &CompressionConfig,
    context: &ResourceContext,
    method: Method,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    if config.method != method {
        return Err(Error::invalid_config(
            "fixed finder method does not match configuration",
        ));
    }
    let spool = spool_input(input, &config.resources, context)?;
    find_matches_spooled(&spool, config, context, method)
}

pub(crate) fn append_overlay(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    mut base: BudgetedVec<MatchCandidate>,
) -> Result<BudgetedVec<MatchCandidate>> {
    let Some(overlay) = config.rep_overlay.as_ref() else {
        return Ok(base);
    };
    let effective_distance = match config.max_distance {
        Some(limit) if limit != 0 => limit.min(overlay.distance),
        _ => overlay.distance,
    };
    let parameters = M0Parameters {
        min_match: overlay.min_match,
        region: (overlay.min_match / 8).max(1),
        max_distance: Some(effective_distance),
    };
    let ordinal_start = match base
        .iter()
        .map(|candidate| candidate.insertion_ordinal)
        .max()
    {
        Some(ordinal) => ordinal
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("overlay insertion ordinal overflows"))?,
        None => 0,
    };
    find_matches_m0_spooled_with_parameters_into(
        spool,
        &parameters,
        context,
        ordinal_start,
        crate::match_finder::m0::hash_at_dyn,
        &mut base,
        None,
    )?;
    Ok(base)
}

fn find_m3_spooled_with_digest<D>(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    mut digest_fn: D,
) -> Result<BudgetedVec<MatchCandidate>>
where
    D: FnMut(&mut dyn DataSource, u64, u64) -> Result<[u8; 16]>,
{
    let seed = config
        .seed_size
        .ok_or_else(|| Error::invalid_config("seed-size is required for m3"))?;
    let q_min = checked_ceil_div(config.min_match, seed)?;
    let mut source = SpoolDataSource::new(spool)?;
    let mut index = new_candidate_index(context, &config.resources)?;
    let mut output = BudgetedVec::new(&context.memory)?;
    let mut source_ordinal = 0u64;
    let mut candidate_ordinal = 0u64;
    let mut next_source = 0u64;
    for target in fixed_targets(spool.len, seed) {
        insert_visible_sources(
            &mut index,
            &mut source,
            target,
            seed,
            &mut source_ordinal,
            &mut next_source,
            &mut digest_fn,
        )?;
        let digest = digest_fn(&mut source, target, seed)?;
        let key = m3_key(seed, &digest);
        index.for_each_candidate(
            M3_KEY_KIND,
            &key,
            target,
            config.max_distance.unwrap_or(0),
            &mut |entry| {
                if compare_contiguous(&mut source, entry.position, target, seed)? {
                    let mut length = seed;
                    let q_limit = (spool.len - target) / seed;
                    while length / seed < q_limit {
                        let next = target
                            .checked_add(length)
                            .ok_or_else(|| Error::invalid_match("m3 target position overflows"))?;
                        let distance = target - entry.position;
                        let expected_digest = blake3_periodic(
                            &mut source,
                            entry.position,
                            length % distance,
                            distance,
                            seed,
                        )?;
                        let actual_digest = digest_fn(&mut source, next, seed)?;
                        if expected_digest != actual_digest
                            || !compare_periodic(
                                &mut source,
                                entry.position,
                                length % distance,
                                distance,
                                next,
                                seed,
                            )?
                        {
                            break;
                        }
                        length = length
                            .checked_add(seed)
                            .ok_or_else(|| Error::invalid_match("m3 match length overflows"))?;
                    }
                    if length / seed >= q_min {
                        output.push(MatchCandidate {
                            src: entry.position,
                            dst: target,
                            len: length,
                            insertion_ordinal: candidate_ordinal,
                        })?;
                        candidate_ordinal = candidate_ordinal.checked_add(1).ok_or_else(|| {
                            Error::invalid_match("m3 insertion ordinal overflows")
                        })?;
                    }
                }
                Ok(())
            },
        )?;
    }
    Ok(output)
}

fn find_m4_spooled_with_hash<H>(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    mut hash_fn: H,
) -> Result<BudgetedVec<MatchCandidate>>
where
    H: FnMut(&mut dyn DataSource, u64, u64) -> Result<u64>,
{
    let seed = config
        .seed_size
        .ok_or_else(|| Error::invalid_config("seed-size is required for m4"))?;
    let mut source = SpoolDataSource::new(spool)?;
    let mut index = new_candidate_index(context, &config.resources)?;
    let mut output = BudgetedVec::new(&context.memory)?;
    let mut source_ordinal = 0u64;
    let mut candidate_ordinal = 0u64;
    let mut next_source = 0u64;
    for target in fixed_targets(spool.len, seed) {
        insert_visible_sources_with_hash(
            &mut index,
            &mut source,
            target,
            seed,
            &mut source_ordinal,
            &mut next_source,
            &mut hash_fn,
        )?;
        let key = m4_key(seed, hash_fn(&mut source, target, seed)?);
        index.for_each_candidate(
            M4_KEY_KIND,
            &key,
            target,
            config.max_distance.unwrap_or(0),
            &mut |entry| {
                if compare_contiguous(&mut source, entry.position, target, seed)? {
                    let distance = target - entry.position;
                    let mut backward = 0u64;
                    while backward < entry.position
                        && backward < target
                        && equal_byte(
                            &mut source,
                            entry.position - backward - 1,
                            target - backward - 1,
                        )?
                    {
                        backward += 1;
                    }
                    let src = entry.position - backward;
                    let dst = target - backward;
                    let mut length = backward
                        .checked_add(seed)
                        .ok_or_else(|| Error::invalid_match("m4 match length overflows"))?;
                    while dst
                        .checked_add(length)
                        .ok_or_else(|| Error::invalid_match("m4 target position overflows"))?
                        < spool.len
                        && equal_periodic_byte(
                            &mut source,
                            src,
                            length % distance,
                            distance,
                            dst.checked_add(length).ok_or_else(|| {
                                Error::invalid_match("m4 target position overflows")
                            })?,
                        )?
                    {
                        length = length
                            .checked_add(1)
                            .ok_or_else(|| Error::invalid_match("m4 match length overflows"))?;
                    }
                    if length >= config.min_match {
                        output.push(MatchCandidate {
                            src,
                            dst,
                            len: length,
                            insertion_ordinal: candidate_ordinal,
                        })?;
                        candidate_ordinal = candidate_ordinal.checked_add(1).ok_or_else(|| {
                            Error::invalid_match("m4 insertion ordinal overflows")
                        })?;
                    }
                }
                Ok(())
            },
        )?;
    }
    Ok(output)
}

fn insert_visible_sources<D>(
    index: &mut dyn CandidateIndex,
    source: &mut impl DataSource,
    target: u64,
    seed: u64,
    ordinal: &mut u64,
    next_source: &mut u64,
    digest_fn: &mut D,
) -> Result<()>
where
    D: FnMut(&mut dyn DataSource, u64, u64) -> Result<[u8; 16]>,
{
    while *next_source < target {
        let position = *next_source;
        let end = position
            .checked_add(seed)
            .ok_or_else(|| Error::invalid_match("m3 source seed endpoint overflows"))?;
        if end > source.len() {
            break;
        }
        let digest = digest_fn(source, position, seed)?;
        index.insert(IndexEntry::new(
            M3_KEY_KIND,
            &m3_key(seed, &digest),
            position,
            *ordinal,
            &[],
        )?)?;
        *ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("source insertion ordinal overflows"))?;
        *next_source = position
            .checked_add(seed)
            .ok_or_else(|| Error::invalid_match("source grid position overflows"))?;
    }
    Ok(())
}

fn insert_visible_sources_with_hash<H>(
    index: &mut dyn CandidateIndex,
    source: &mut impl DataSource,
    target: u64,
    seed: u64,
    ordinal: &mut u64,
    next_source: &mut u64,
    hash_fn: &mut H,
) -> Result<()>
where
    H: FnMut(&mut dyn DataSource, u64, u64) -> Result<u64>,
{
    while *next_source < target {
        let position = *next_source;
        let end = position
            .checked_add(seed)
            .ok_or_else(|| Error::invalid_match("m4 source seed endpoint overflows"))?;
        if end > source.len() {
            break;
        }
        let hash = hash_fn(source, position, seed)?;
        index.insert(IndexEntry::new(
            M4_KEY_KIND,
            &m4_key(seed, hash),
            position,
            *ordinal,
            &[],
        )?)?;
        *ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("source insertion ordinal overflows"))?;
        *next_source = position
            .checked_add(seed)
            .ok_or_else(|| Error::invalid_match("source grid position overflows"))?;
    }
    Ok(())
}

fn fixed_targets(length: u64, seed: u64) -> impl Iterator<Item = u64> {
    let count = if length < seed { 0 } else { length - seed + 1 };
    0..count
}

fn checked_ceil_div(value: u64, divisor: u64) -> Result<u64> {
    if divisor == 0 {
        return Err(Error::invalid_config("seed-size must be nonzero"));
    }
    value
        .checked_add(divisor - 1)
        .map(|value| value / divisor)
        .ok_or_else(|| Error::invalid_match("m3 minimum quotient overflows"))
}

fn m3_key(seed: u64, digest: &[u8; 16]) -> [u8; 24] {
    let mut key = [0u8; 24];
    key[..8].copy_from_slice(&seed.to_le_bytes());
    key[8..].copy_from_slice(digest);
    key
}

fn m4_key(seed: u64, hash: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&seed.to_le_bytes());
    key[8..].copy_from_slice(&hash.to_le_bytes());
    key
}

fn blake3_at(source: &mut dyn DataSource, position: u64, length: u64) -> Result<[u8; 16]> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; COMPARE_CHUNK];
    let mut offset = 0u64;
    while offset < length {
        let count = usize::try_from((length - offset).min(COMPARE_CHUNK as u64))
            .map_err(|_| Error::memory_limit("m3 digest length exceeds platform limits"))?;
        source.read_at(
            position
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("m3 digest position overflows"))?,
            &mut buffer[..count],
        )?;
        hasher.update(&buffer[..count]);
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("m3 digest offset overflows"))?;
    }
    let digest = hasher.finalize();
    Ok(digest.as_bytes()[..16]
        .try_into()
        .expect("BLAKE3 digest width"))
}

fn blake3_periodic(
    source: &mut impl DataSource,
    base: u64,
    mut offset: u64,
    period: u64,
    length: u64,
) -> Result<[u8; 16]> {
    let mut hasher = blake3::Hasher::new();
    let mut bytes = [0u8; COMPARE_CHUNK];
    let mut remaining = length;
    while remaining > 0 {
        let count = usize::try_from(remaining.min(COMPARE_CHUNK as u64))
            .map_err(|_| Error::memory_limit("m3 periodic digest exceeds platform limits"))?;
        read_periodic(source, base, offset, period, &mut bytes[..count])?;
        hasher.update(&bytes[..count]);
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("m3 periodic digest offset overflows"))?
            % period;
        remaining -= count as u64;
    }
    let digest = hasher.finalize();
    Ok(digest.as_bytes()[..16]
        .try_into()
        .expect("BLAKE3 digest width"))
}

fn polynomial_at(source: &mut dyn DataSource, position: u64, length: u64) -> Result<u64> {
    let mut hash = 0u64;
    let mut buffer = [0u8; COMPARE_CHUNK];
    let mut offset = 0u64;
    while offset < length {
        let count = usize::try_from((length - offset).min(COMPARE_CHUNK as u64))
            .map_err(|_| Error::memory_limit("m4 seed exceeds platform limits"))?;
        source.read_at(
            position
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("m4 hash position overflows"))?,
            &mut buffer[..count],
        )?;
        hash = polynomial_hash_update(hash, &buffer[..count]);
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("m4 hash offset overflows"))?;
    }
    Ok(hash)
}

fn polynomial_hash_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash = hash.wrapping_mul(153_191).wrapping_add(u64::from(byte));
    }
    hash
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
            .map_err(|_| Error::memory_limit("fixed comparison exceeds platform limits"))?;
        source.read_at(
            first
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("fixed source position overflows"))?,
            &mut first_bytes[..count],
        )?;
        source.read_at(
            second
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("fixed target position overflows"))?,
            &mut second_bytes[..count],
        )?;
        if first_bytes[..count] != second_bytes[..count] {
            return Ok(false);
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("fixed comparison offset overflows"))?;
    }
    Ok(true)
}

fn equal_byte(source: &mut impl DataSource, first: u64, second: u64) -> Result<bool> {
    let mut bytes = [0u8; 2];
    source.read_at(first, &mut bytes[..1])?;
    source.read_at(second, &mut bytes[1..])?;
    Ok(bytes[0] == bytes[1])
}

fn equal_periodic_byte(
    source: &mut impl DataSource,
    base: u64,
    offset: u64,
    period: u64,
    target: u64,
) -> Result<bool> {
    let source_position = base
        .checked_add(offset % period)
        .ok_or_else(|| Error::invalid_match("periodic source position overflows"))?;
    equal_byte(source, source_position, target)
}

fn compare_periodic(
    source: &mut impl DataSource,
    base: u64,
    mut offset: u64,
    period: u64,
    target: u64,
    length: u64,
) -> Result<bool> {
    let mut expected = [0u8; COMPARE_CHUNK];
    let mut actual = [0u8; COMPARE_CHUNK];
    let mut written = 0u64;
    while written < length {
        let count = usize::try_from((length - written).min(COMPARE_CHUNK as u64))
            .map_err(|_| Error::memory_limit("m3 periodic comparison exceeds platform limits"))?;
        read_periodic(source, base, offset, period, &mut expected[..count])?;
        source.read_at(
            target
                .checked_add(written)
                .ok_or_else(|| Error::invalid_match("m3 target position overflows"))?,
            &mut actual[..count],
        )?;
        if expected[..count] != actual[..count] {
            return Ok(false);
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("m3 periodic comparison offset overflows"))?
            % period;
        written = written
            .checked_add(count as u64)
            .ok_or_else(|| Error::invalid_match("m3 periodic comparison length overflows"))?;
    }
    Ok(true)
}

fn read_periodic(
    source: &mut impl DataSource,
    base: u64,
    mut offset: u64,
    period: u64,
    destination: &mut [u8],
) -> Result<()> {
    for byte in destination {
        let position = base
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("periodic read position overflows"))?;
        source.read_at(position, std::slice::from_mut(byte))?;
        offset = offset
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("periodic read offset overflows"))?
            % period;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CompressionConfig, ResourceConfig};

    fn source(bytes: &[u8]) -> (InputSpool, ResourceContext) {
        let resources = ResourceConfig {
            memory: 64 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let context = ResourceContext::with_resources(&resources).unwrap();
        let spool = crate::codec::spool_input(bytes, &resources, &context).unwrap();
        (spool, context)
    }

    fn constant_digest(
        _source: &mut dyn DataSource,
        _position: u64,
        _length: u64,
    ) -> Result<[u8; 16]> {
        Ok([0; 16])
    }

    fn constant_hash(_source: &mut dyn DataSource, _position: u64, _length: u64) -> Result<u64> {
        Ok(0)
    }

    #[test]
    fn m3_digest_collision_requires_exact_bytes_in_the_production_loop() {
        let (spool, context) = source(b"abcdefghABCDEFGHabcdefgh");
        let mut config = CompressionConfig::for_method(Method::M3FixedDigest);
        config.min_match = 8;
        config.seed_size = Some(8);
        let candidates =
            find_m3_spooled_with_digest(&spool, &config, &context, constant_digest).unwrap();
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.src == 0 && candidate.dst == 16)
        );
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.src == 8 && candidate.dst == 16)
        );
    }

    #[test]
    fn m4_polynomial_collision_requires_exact_bytes_in_the_production_loop() {
        let (spool, context) = source(b"abcdefghABCDEFGHabcdefgh");
        let mut config = CompressionConfig::for_method(Method::M4Reread);
        config.min_match = 8;
        config.seed_size = Some(8);
        let candidates =
            find_m4_spooled_with_hash(&spool, &config, &context, constant_hash).unwrap();
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.src == 0 && candidate.dst == 16)
        );
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.src == 8 && candidate.dst == 16)
        );
    }
}
