use std::io::{Read, Seek, SeekFrom};

use crate::candidate_index::{CandidateIndex, IndexEntry, new_candidate_index};
use crate::codec::{InputSpool, spool_input};
use crate::config::{CompressionConfig, Method};
use crate::error::{Error, ErrorKind, Result};
use crate::match_ir::{ExactIntervalFilter, IdenticalIntervalFilter, MatchCandidate};
use crate::resource::{BudgetedVec, ResourceContext};

use super::m0::{M0Parameters, find_matches_m0_spooled_with_parameters_into};
use super::source::{DataSource, SpoolDataSource};

const COMPARE_CHUNK: usize = 4096;
const M3_KEY_KIND: u8 = 3;
const M4_KEY_KIND: u8 = 4;
const ACCELERATION_THRESHOLD: u64 = 256 * 1024;

#[cfg(test)]
thread_local! {
    static TEST_DISABLE_ACCELERATION: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static TEST_ACCELERATION_MEMORY_AVAILABLE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
    static TEST_ACCELERATION_DENY_STAGE: std::cell::Cell<u8> =
        const { std::cell::Cell::new(0) };
    static TEST_LAST_ACCELERATION_STATE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
    static TEST_DIGEST_COMPARE_COUNT: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
    static TEST_EXACT_COMPARE_COUNT: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) struct DisableAccelerationGuard {
    previous: bool,
}

#[cfg(test)]
impl Drop for DisableAccelerationGuard {
    fn drop(&mut self) {
        TEST_DISABLE_ACCELERATION.with(|flag| flag.set(self.previous));
    }
}

#[cfg(test)]
pub(crate) fn disable_acceleration() -> DisableAccelerationGuard {
    let previous = TEST_DISABLE_ACCELERATION.with(|flag| {
        let previous = flag.get();
        flag.set(true);
        previous
    });
    DisableAccelerationGuard { previous }
}

#[cfg(test)]
pub(crate) struct AccelerationMemoryAvailableGuard {
    previous: Option<u64>,
}

#[cfg(test)]
impl Drop for AccelerationMemoryAvailableGuard {
    fn drop(&mut self) {
        TEST_ACCELERATION_MEMORY_AVAILABLE.with(|available| available.set(self.previous));
    }
}

/// Reserve all but `available` bytes of the context budget while building
/// acceleration. Allocations still go through `BudgetedVec`; the reservation
/// is released before candidate generation continues.
#[cfg(test)]
pub(crate) fn limit_acceleration_memory(available: u64) -> AccelerationMemoryAvailableGuard {
    let previous = TEST_ACCELERATION_MEMORY_AVAILABLE.with(|current| {
        let previous = current.get();
        current.set(Some(available));
        previous
    });
    AccelerationMemoryAvailableGuard { previous }
}

#[cfg(test)]
pub(crate) struct AccelerationStageDenialGuard {
    previous: u8,
}

#[cfg(test)]
impl Drop for AccelerationStageDenialGuard {
    fn drop(&mut self) {
        TEST_ACCELERATION_DENY_STAGE.with(|stage| stage.set(self.previous));
    }
}

#[cfg(test)]
pub(crate) fn deny_acceleration_stage(stage: u8) -> AccelerationStageDenialGuard {
    let previous = TEST_ACCELERATION_DENY_STAGE.with(|current| {
        let previous = current.get();
        current.set(stage);
        previous
    });
    AccelerationStageDenialGuard { previous }
}

#[cfg(test)]
pub(crate) fn last_acceleration_active() -> Option<bool> {
    TEST_LAST_ACCELERATION_STATE.with(std::cell::Cell::get)
}

#[cfg(test)]
fn record_digest_compare() {
    TEST_DIGEST_COMPARE_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn record_exact_compare() {
    TEST_EXACT_COMPARE_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn take_digest_compares() -> u64 {
    TEST_DIGEST_COMPARE_COUNT.with(|count| count.replace(0))
}

#[cfg(test)]
fn take_exact_compares() -> u64 {
    TEST_EXACT_COMPARE_COUNT.with(|count| count.replace(0))
}

#[cfg(test)]
fn reset_compare_counters() {
    TEST_DIGEST_COMPARE_COUNT.with(|count| count.set(0));
    TEST_EXACT_COMPARE_COUNT.with(|count| count.set(0));
}

struct FixedAcceleration {
    bytes: BudgetedVec<u8>,
    run: BudgetedVec<u32>,
    rev_run: BudgetedVec<u32>,
}

struct SnapshotSource<'a> {
    bytes: &'a [u8],
}

impl DataSource for SnapshotSource<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_at(&mut self, position: u64, destination: &mut [u8]) -> Result<()> {
        let start = usize::try_from(position)
            .map_err(|_| Error::memory_limit("snapshot position exceeds platform limits"))?;
        let end = start
            .checked_add(destination.len())
            .ok_or_else(|| Error::invalid_match("snapshot read endpoint overflows"))?;
        let bytes = self
            .bytes
            .get(start..end)
            .ok_or_else(|| Error::truncated("snapshot read exceeds input"))?;
        destination.copy_from_slice(bytes);
        Ok(())
    }
}

impl FixedAcceleration {
    fn try_new(spool: &InputSpool, context: &ResourceContext) -> Result<Option<Self>> {
        #[cfg(test)]
        if TEST_DISABLE_ACCELERATION.with(std::cell::Cell::get) {
            TEST_LAST_ACCELERATION_STATE.with(|state| state.set(Some(false)));
            return Ok(None);
        }
        #[cfg(test)]
        let _test_memory_reservation = TEST_ACCELERATION_MEMORY_AVAILABLE.with(|available| {
            available
                .get()
                .map(|available| {
                    context
                        .memory
                        .reserve(context.memory.limit().saturating_sub(available))
                })
                .transpose()
        })?;
        let result = (|| -> Result<Option<Self>> {
            let length = match usize::try_from(spool.len) {
                Ok(length) if length as u64 >= ACCELERATION_THRESHOLD => length,
                _ => return Ok(None),
            };
            #[cfg(test)]
            let _bytes_denial = Self::stage_denial(context, 1);
            let mut bytes = match BudgetedVec::with_capacity(length, &context.memory) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == ErrorKind::MemoryBudgetExceeded => return Ok(None),
                Err(error) => return Err(error),
            };
            bytes.resize(length, 0)?;
            let mut file = spool.file.try_clone().map_err(Error::temp_storage)?;
            file.seek(SeekFrom::Start(0)).map_err(Error::temp_storage)?;
            file.read_exact(bytes.as_mut_slice())
                .map_err(Error::temp_storage)?;
            #[cfg(test)]
            drop(_bytes_denial);
            #[cfg(test)]
            let _run_denial = Self::stage_denial(context, 2);
            let mut run = match BudgetedVec::with_capacity(length, &context.memory) {
                Ok(run) => run,
                Err(error) if error.kind() == ErrorKind::MemoryBudgetExceeded => return Ok(None),
                Err(error) => return Err(error),
            };
            run.resize(length, 0)?;
            let mut remaining = 0u32;
            for index in (0..length).rev() {
                remaining = if index + 1 < length && bytes[index] == bytes[index + 1] {
                    remaining.saturating_add(1)
                } else {
                    1
                };
                run[index] = remaining;
            }
            #[cfg(test)]
            drop(_run_denial);
            #[cfg(test)]
            let _rev_denial = Self::stage_denial(context, 3);
            let mut rev_run = match BudgetedVec::with_capacity(length, &context.memory) {
                Ok(rev_run) => rev_run,
                Err(error) if error.kind() == ErrorKind::MemoryBudgetExceeded => return Ok(None),
                Err(error) => return Err(error),
            };
            rev_run.resize(length, 0)?;
            let mut seen = 0u32;
            for index in 0..length {
                seen = if index > 0 && bytes[index] == bytes[index - 1] {
                    seen.saturating_add(1)
                } else {
                    1
                };
                rev_run[index] = seen;
            }
            Ok(Some(Self {
                bytes,
                run,
                rev_run,
            }))
        })();
        #[cfg(test)]
        {
            drop(_test_memory_reservation);
            TEST_LAST_ACCELERATION_STATE
                .with(|state| state.set(Some(matches!(&result, Ok(Some(_))))));
        }
        result
    }

    #[cfg(test)]
    fn stage_denial(context: &ResourceContext, stage: u8) -> Option<crate::resource::Reservation> {
        if TEST_ACCELERATION_DENY_STAGE.with(std::cell::Cell::get) != stage {
            return None;
        }
        let remaining = context
            .memory
            .limit()
            .saturating_sub(context.memory.current());
        Some(context.memory.reserve(remaining).unwrap_or_else(|_| {
            context
                .memory
                .reserve(0)
                .expect("zero reservation must succeed")
        }))
    }

    fn uniform_run(&self, position: u64, limit: u64) -> Result<u64> {
        if limit == 0 {
            return Ok(0);
        }
        let start = usize::try_from(position)
            .map_err(|_| Error::memory_limit("fixed run position exceeds platform limits"))?;
        let available = self.run.get(start).copied().unwrap_or(0) as u64;
        Ok(available.min(limit))
    }

    fn same_uniform_run(&self, previous: u64, position: u64, length: u64) -> Result<bool> {
        if position < previous || length == 0 {
            return Ok(false);
        }
        let Some(previous_byte) = self.bytes.get(previous as usize) else {
            return Ok(false);
        };
        if self.bytes.get(position as usize) != Some(previous_byte) {
            return Ok(false);
        }
        if self.uniform_run(previous, length)? < length
            || self.uniform_run(position, length)? < length
        {
            return Ok(false);
        }
        let Some(span) = position
            .checked_add(length)
            .and_then(|end| end.checked_sub(previous))
        else {
            return Ok(false);
        };
        Ok(self.uniform_run(previous, span)? >= span)
    }

    fn common_prefix(&self, first: u64, second: u64, limit: u64) -> Result<u64> {
        if limit == 0 {
            return Ok(0);
        }
        let first_index = usize::try_from(first)
            .map_err(|_| Error::memory_limit("fixed source exceeds platform limits"))?;
        let second_index = usize::try_from(second)
            .map_err(|_| Error::memory_limit("fixed target exceeds platform limits"))?;
        let limit_index = usize::try_from(limit)
            .map_err(|_| Error::memory_limit("fixed comparison exceeds platform limits"))?;
        if first_index >= self.bytes.len() || second_index >= self.bytes.len() {
            return Ok(0);
        }
        let first_run = self.uniform_run(first, limit)?;
        let second_run = self.uniform_run(second, limit)?;
        if first_run == limit && second_run == limit {
            return Ok(
                u64::from(self.bytes.get(first_index) == self.bytes.get(second_index)) * limit,
            );
        }
        let available = (self.bytes.len() - first_index)
            .min(self.bytes.len() - second_index)
            .min(limit_index);
        let first_bytes = &self.bytes.as_slice()[first_index..first_index + available];
        let second_bytes = &self.bytes.as_slice()[second_index..second_index + available];
        Ok(first_bytes
            .iter()
            .zip(second_bytes)
            .take_while(|(left, right)| left == right)
            .count() as u64)
    }

    fn exact_contiguous(&self, first: u64, second: u64, length: u64) -> Result<bool> {
        Ok(self.common_prefix(first, second, length)? == length)
    }

    fn exact_periodic(
        &self,
        base: u64,
        mut offset: u64,
        period: u64,
        target: u64,
        length: u64,
    ) -> Result<bool> {
        if period == 0 {
            return Err(Error::invalid_match("periodic distance is zero"));
        }
        let mut written = 0u64;
        while written < length {
            let segment = (length - written).min(period - (offset % period));
            let source_position = base
                .checked_add(offset % period)
                .ok_or_else(|| Error::invalid_match("periodic source position overflows"))?;
            let target_position = target
                .checked_add(written)
                .ok_or_else(|| Error::invalid_match("periodic target position overflows"))?;
            if self.common_prefix(source_position, target_position, segment)? < segment {
                return Ok(false);
            }
            written = written
                .checked_add(segment)
                .ok_or_else(|| Error::invalid_match("periodic comparison length overflows"))?;
            offset = 0;
        }
        Ok(true)
    }

    fn backward_equal(&self, source: u64, target: u64) -> Result<u64> {
        if source == 0 || target == 0 {
            return Ok(0);
        }
        let mut matched = 0u64;
        let limit = source.min(target);
        while matched < limit {
            let source_index = usize::try_from(source - matched - 1)
                .map_err(|_| Error::memory_limit("m4 source exceeds platform limits"))?;
            let target_index = usize::try_from(target - matched - 1)
                .map_err(|_| Error::memory_limit("m4 target exceeds platform limits"))?;
            if self.bytes.get(source_index) != self.bytes.get(target_index) {
                break;
            }
            let run = u64::from(
                self.rev_run
                    .get(source_index)
                    .copied()
                    .unwrap_or(1)
                    .min(self.rev_run.get(target_index).copied().unwrap_or(1)),
            )
            .max(1)
            .min(limit - matched);
            matched = matched
                .checked_add(run)
                .ok_or_else(|| Error::invalid_match("m4 backward search overflows"))?;
        }
        Ok(matched)
    }

    fn extend_periodic(
        &self,
        source: u64,
        target: u64,
        mut length: u64,
        distance: u64,
        input_len: u64,
    ) -> Result<u64> {
        if distance == 0 {
            return Err(Error::invalid_match("periodic distance is zero"));
        }
        loop {
            let next = target
                .checked_add(length)
                .ok_or_else(|| Error::invalid_match("m4 target position overflows"))?;
            if next >= input_len {
                break;
            }
            let offset = length % distance;
            let remaining = input_len - next;
            if self.uniform_run(source, distance)? == distance {
                let matched = if self.bytes.get(source as usize) == self.bytes.get(next as usize) {
                    self.uniform_run(next, remaining)?
                } else {
                    0
                };
                length = length
                    .checked_add(matched)
                    .ok_or_else(|| Error::invalid_match("m4 match length overflows"))?;
                break;
            }
            let segment = remaining.min(distance - offset);
            let source_position = source
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("m4 source position overflows"))?;
            let matched = self.common_prefix(source_position, next, segment)?;
            length = length
                .checked_add(matched)
                .ok_or_else(|| Error::invalid_match("m4 match length overflows"))?;
            if matched < segment {
                break;
            }
        }
        Ok(length)
    }
}

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
    find_matches_spooled_with_filters(spool, config, context, method, None, None)
}

pub(crate) fn find_matches_spooled_for_normalization(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    method: Method,
) -> Result<BudgetedVec<MatchCandidate>> {
    let mut reducer = ExactIntervalFilter::new(&context.memory)?;
    find_matches_spooled_with_filters(spool, config, context, method, None, Some(&mut reducer))
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn find_matches_spooled_compact(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    method: Method,
) -> Result<BudgetedVec<MatchCandidate>> {
    let mut filter = IdenticalIntervalFilter::new(&context.memory)?;
    let mut candidates =
        find_matches_spooled_with_filters(spool, config, context, method, Some(&mut filter), None)?;
    crate::match_ir::retain_identical_interval_representatives(&mut candidates);
    Ok(candidates)
}

fn find_matches_spooled_with_filters(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    method: Method,
    mut filter: Option<&mut IdenticalIntervalFilter>,
    mut exact_filter: Option<&mut ExactIntervalFilter>,
) -> Result<BudgetedVec<MatchCandidate>> {
    config.validate()?;
    if config.method != method {
        return Err(Error::invalid_config(
            "fixed finder method does not match configuration",
        ));
    }
    let (mut base, next_ordinal) = match method {
        Method::M3FixedDigest => find_m3_spooled_with_digest_filtered(
            spool,
            config,
            context,
            blake3_at,
            filter.as_deref_mut(),
            exact_filter.as_deref_mut(),
        )?,
        Method::M4Reread => find_m4_spooled_with_hash_filtered(
            spool,
            config,
            context,
            polynomial_at,
            filter,
            exact_filter.as_deref_mut(),
        )?,
        _ => return Err(Error::invalid_config("method is not a fixed finder")),
    };
    if let Some(exact_filter) = exact_filter {
        append_overlay_into(
            spool,
            config,
            context,
            &mut base,
            next_ordinal,
            Some(exact_filter),
        )?;
        Ok(base)
    } else {
        append_overlay_from(spool, config, context, base, next_ordinal, None)
    }
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
    base: BudgetedVec<MatchCandidate>,
    next_ordinal: u64,
) -> Result<BudgetedVec<MatchCandidate>> {
    append_overlay_from(spool, config, context, base, next_ordinal, None)
}

#[cfg(test)]
fn overlay_ordinal_start(base: &[MatchCandidate]) -> Result<u64> {
    match base
        .iter()
        .map(|candidate| candidate.insertion_ordinal)
        .max()
    {
        Some(ordinal) => ordinal
            .checked_add(1)
            .ok_or_else(|| Error::invalid_match("overlay insertion ordinal overflows")),
        None => Ok(0),
    }
}

fn append_overlay_from(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    mut base: BudgetedVec<MatchCandidate>,
    ordinal_start: u64,
    exact_filter: Option<&mut ExactIntervalFilter>,
) -> Result<BudgetedVec<MatchCandidate>> {
    append_overlay_into(
        spool,
        config,
        context,
        &mut base,
        ordinal_start,
        exact_filter,
    )?;
    Ok(base)
}

fn append_overlay_into(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    base: &mut BudgetedVec<MatchCandidate>,
    ordinal_start: u64,
    exact_filter: Option<&mut ExactIntervalFilter>,
) -> Result<()> {
    let Some(overlay) = config.rep_overlay.as_ref() else {
        return Ok(());
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
    find_matches_m0_spooled_with_parameters_into(
        spool,
        &parameters,
        context,
        ordinal_start,
        crate::match_finder::m0::hash_at_dyn,
        base,
        None,
        exact_filter,
    )?;
    Ok(())
}

#[cfg(test)]
fn find_m3_spooled_with_digest<D>(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    digest_fn: D,
    filter: Option<&mut IdenticalIntervalFilter>,
) -> Result<(BudgetedVec<MatchCandidate>, u64)>
where
    D: FnMut(&mut dyn DataSource, u64, u64) -> Result<[u8; 16]>,
{
    find_m3_spooled_with_digest_filtered(spool, config, context, digest_fn, filter, None)
}

fn find_m3_spooled_with_digest_filtered<D>(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    mut digest_fn: D,
    mut filter: Option<&mut IdenticalIntervalFilter>,
    mut exact_filter: Option<&mut ExactIntervalFilter>,
) -> Result<(BudgetedVec<MatchCandidate>, u64)>
where
    D: FnMut(&mut dyn DataSource, u64, u64) -> Result<[u8; 16]>,
{
    let seed = config
        .seed_size
        .ok_or_else(|| Error::invalid_config("seed-size is required for m3"))?;
    let q_min = checked_ceil_div(config.min_match, seed)?;
    let snapshot = FixedAcceleration::try_new(spool, context)?;
    let mut spool_source = SpoolDataSource::new(spool)?;
    let mut digest_cache =
        match SeedDigestCache::try_new(spool.len, seed, snapshot.is_some(), context) {
            Ok(cache) => cache,
            Err(error) if error.kind() == ErrorKind::MemoryBudgetExceeded => {
                SeedDigestCache::disabled(seed)
            }
            Err(error) => return Err(error),
        };
    let mut index = new_candidate_index(context, &config.resources)?;
    let mut output = BudgetedVec::new(&context.memory)?;
    let mut source_ordinal = 0u64;
    let mut candidate_ordinal = 0u64;
    let mut next_source = 0u64;
    let mut last_uniform_target: Option<u64> = None;
    let mut last_visible_source: Option<u64> = None;
    for target in fixed_targets(spool.len, seed) {
        let digest = if let Some(accelerator) = snapshot.as_ref() {
            let mut snapshot_source = SnapshotSource {
                bytes: accelerator.bytes.as_slice(),
            };
            insert_visible_sources(
                &mut index,
                &mut snapshot_source,
                target,
                seed,
                &mut source_ordinal,
                &mut next_source,
                &mut |source, position, length| {
                    digest_cache.get(&mut digest_fn, source, position, length)
                },
            )?;
            if filter.is_some()
                && last_visible_source == Some(next_source)
                && let Some(previous) = last_uniform_target
                && accelerator.same_uniform_run(previous, target, seed)?
            {
                continue;
            }
            digest_cache.get(&mut digest_fn, &mut snapshot_source, target, seed)?
        } else {
            insert_visible_sources(
                &mut index,
                &mut spool_source,
                target,
                seed,
                &mut source_ordinal,
                &mut next_source,
                &mut digest_fn,
            )?;
            digest_fn(&mut spool_source, target, seed)?
        };
        let key = m3_key(seed, &digest);
        index.for_each_candidate(
            M3_KEY_KIND,
            &key,
            target,
            config.max_distance.unwrap_or(0),
            &mut |entry| {
                let seed_equal = match snapshot.as_ref() {
                    Some(accelerator) => {
                        accelerator.exact_contiguous(entry.position, target, seed)?
                    }
                    None => compare_contiguous(&mut spool_source, entry.position, target, seed)?,
                };
                if seed_equal {
                    let mut length = seed;
                    let q_limit = (spool.len - target) / seed;
                    let distance = target - entry.position;
                    while length / seed < q_limit {
                        let next = target
                            .checked_add(length)
                            .ok_or_else(|| Error::invalid_match("m3 target position overflows"))?;
                        let (expected_digest, actual_digest) =
                            if let Some(accelerator) = snapshot.as_ref() {
                                let mut snapshot_source = SnapshotSource {
                                    bytes: accelerator.bytes.as_slice(),
                                };
                                let expected = extension_expected_digest(
                                    &mut digest_cache,
                                    &mut digest_fn,
                                    &mut snapshot_source,
                                    entry.position,
                                    length % distance,
                                    distance,
                                    seed,
                                )?;
                                let actual = digest_cache.get(
                                    &mut digest_fn,
                                    &mut snapshot_source,
                                    next,
                                    seed,
                                )?;
                                (expected, actual)
                            } else {
                                (
                                    extension_expected_digest(
                                        &mut digest_cache,
                                        &mut digest_fn,
                                        &mut spool_source,
                                        entry.position,
                                        length % distance,
                                        distance,
                                        seed,
                                    )?,
                                    digest_fn(&mut spool_source, next, seed)?,
                                )
                            };
                        #[cfg(test)]
                        record_digest_compare();
                        if expected_digest != actual_digest {
                            break;
                        }
                        let exact = match snapshot.as_ref() {
                            Some(accelerator) => accelerator.exact_periodic(
                                entry.position,
                                length % distance,
                                distance,
                                next,
                                seed,
                            )?,
                            None => compare_periodic(
                                &mut spool_source,
                                entry.position,
                                length % distance,
                                distance,
                                next,
                                seed,
                            )?,
                        };
                        #[cfg(test)]
                        record_exact_compare();
                        if !exact {
                            break;
                        }
                        let mut matched = 1u64;
                        if let Some(accelerator) = snapshot.as_ref()
                            && accelerator.uniform_run(entry.position, distance)? == distance
                        {
                            let remaining_after = (q_limit - length / seed).saturating_sub(1);
                            let source_phase = (length + seed) % distance;
                            let digest_jump = if remaining_after == 0 {
                                1
                            } else {
                                let mut snapshot_source = SnapshotSource {
                                    bytes: accelerator.bytes.as_slice(),
                                };
                                let extra = digest_cache.matching_stride_run(
                                    &mut digest_fn,
                                    &mut snapshot_source,
                                    StrideRun {
                                        source_position: entry.position,
                                        source_phase,
                                        distance,
                                        target: next.checked_add(seed).ok_or_else(|| {
                                            Error::invalid_match("m3 target position overflows")
                                        })?,
                                        remaining_quanta: remaining_after,
                                    },
                                )?;
                                #[cfg(test)]
                                TEST_DIGEST_COMPARE_COUNT
                                    .with(|count| count.set(count.get().saturating_add(extra)));
                                extra.checked_add(1).ok_or_else(|| {
                                    Error::invalid_match("m3 match length overflows")
                                })?
                            };
                            let remaining_bytes = (remaining_after + 1)
                                .checked_mul(seed)
                                .ok_or_else(|| Error::invalid_match("m3 match length overflows"))?;
                            let exact_jump = accelerator.uniform_run(next, remaining_bytes)? / seed;
                            #[cfg(test)]
                            if exact_jump > 1 {
                                TEST_EXACT_COMPARE_COUNT.with(|count| {
                                    count.set(
                                        count.get().saturating_add(exact_jump.saturating_sub(1)),
                                    )
                                });
                            }
                            matched = digest_jump.min(exact_jump).min(remaining_after + 1).max(1);
                        }
                        let added = matched
                            .checked_mul(seed)
                            .ok_or_else(|| Error::invalid_match("m3 match length overflows"))?;
                        length = length
                            .checked_add(added)
                            .ok_or_else(|| Error::invalid_match("m3 match length overflows"))?;
                    }
                    if length / seed >= q_min {
                        let candidate = MatchCandidate {
                            src: entry.position,
                            dst: target,
                            len: length,
                            insertion_ordinal: candidate_ordinal,
                        };
                        candidate_ordinal = candidate_ordinal.checked_add(1).ok_or_else(|| {
                            Error::invalid_match("m3 insertion ordinal overflows")
                        })?;
                        if output.len() == output.capacity() && output.capacity() >= 1_048_576 {
                            digest_cache = SeedDigestCache::disabled(seed);
                        }
                        if output.len() == output.capacity() && output.capacity() >= 1_048_576 {
                            digest_cache = SeedDigestCache::disabled(seed);
                        }
                        if let Some(exact_filter) = exact_filter.as_deref_mut() {
                            exact_filter.consider(&mut output, candidate)?;
                        } else if let Some(filter) = filter.as_deref_mut() {
                            filter.consider(&mut output, candidate)?;
                        } else {
                            output.push(candidate)?;
                        }
                    }
                }
                Ok(())
            },
        )?;
        last_visible_source = Some(next_source);
        last_uniform_target = if filter.is_some()
            && let Some(accelerator) = snapshot.as_ref()
            && accelerator.uniform_run(target, seed)? >= seed
        {
            Some(target)
        } else {
            None
        };
    }
    Ok((output, candidate_ordinal))
}

#[cfg(test)]
fn find_m4_spooled_with_hash<H>(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    hash_fn: H,
    filter: Option<&mut IdenticalIntervalFilter>,
) -> Result<(BudgetedVec<MatchCandidate>, u64)>
where
    H: FnMut(&mut dyn DataSource, u64, u64) -> Result<u64>,
{
    find_m4_spooled_with_hash_filtered(spool, config, context, hash_fn, filter, None)
}

fn find_m4_spooled_with_hash_filtered<H>(
    spool: &InputSpool,
    config: &CompressionConfig,
    context: &ResourceContext,
    mut hash_fn: H,
    mut filter: Option<&mut IdenticalIntervalFilter>,
    mut exact_filter: Option<&mut ExactIntervalFilter>,
) -> Result<(BudgetedVec<MatchCandidate>, u64)>
where
    H: FnMut(&mut dyn DataSource, u64, u64) -> Result<u64>,
{
    let seed = config
        .seed_size
        .ok_or_else(|| Error::invalid_config("seed-size is required for m4"))?;
    let snapshot = FixedAcceleration::try_new(spool, context)?;
    let mut spool_source = SpoolDataSource::new(spool)?;
    let mut hash_cache: Option<BudgetedVec<Option<u64>>> = match snapshot.as_ref() {
        Some(_) => {
            let mut cache = match BudgetedVec::with_capacity(spool.len as usize, &context.memory) {
                Ok(cache) => cache,
                Err(error) if error.kind() == ErrorKind::MemoryBudgetExceeded => {
                    BudgetedVec::new(&context.memory)?
                }
                Err(error) => return Err(error),
            };
            if cache.capacity() >= spool.len as usize {
                cache.resize(spool.len as usize, None)?;
                Some(cache)
            } else {
                None
            }
        }
        None => None,
    };
    let mut index = new_candidate_index(context, &config.resources)?;
    let mut output = BudgetedVec::new(&context.memory)?;
    let mut source_ordinal = 0u64;
    let mut candidate_ordinal = 0u64;
    let mut next_source = 0u64;
    let mut last_uniform_target: Option<u64> = None;
    let mut last_visible_source: Option<u64> = None;
    for target in fixed_targets(spool.len, seed) {
        let key = if let Some(accelerator) = snapshot.as_ref() {
            let mut snapshot_source = SnapshotSource {
                bytes: accelerator.bytes.as_slice(),
            };
            insert_visible_sources_with_hash(
                &mut index,
                &mut snapshot_source,
                target,
                seed,
                &mut source_ordinal,
                &mut next_source,
                &mut |source, position, length| {
                    cached_hash(&mut hash_cache, &mut hash_fn, source, position, length)
                },
            )?;
            if filter.is_some()
                && last_visible_source == Some(next_source)
                && let Some(previous) = last_uniform_target
                && accelerator.same_uniform_run(previous, target, seed)?
            {
                continue;
            }
            m4_key(
                seed,
                cached_hash(
                    &mut hash_cache,
                    &mut hash_fn,
                    &mut snapshot_source,
                    target,
                    seed,
                )?,
            )
        } else {
            insert_visible_sources_with_hash(
                &mut index,
                &mut spool_source,
                target,
                seed,
                &mut source_ordinal,
                &mut next_source,
                &mut hash_fn,
            )?;
            m4_key(seed, hash_fn(&mut spool_source, target, seed)?)
        };
        index.for_each_candidate(
            M4_KEY_KIND,
            &key,
            target,
            config.max_distance.unwrap_or(0),
            &mut |entry| {
                let seed_equal = match snapshot.as_ref() {
                    Some(accelerator) => {
                        accelerator.exact_contiguous(entry.position, target, seed)?
                    }
                    None => compare_contiguous(&mut spool_source, entry.position, target, seed)?,
                };
                if seed_equal {
                    let distance = target - entry.position;
                    let backward = match snapshot.as_ref() {
                        Some(accelerator) => accelerator.backward_equal(entry.position, target)?,
                        None => {
                            let mut backward = 0u64;
                            while backward < entry.position
                                && backward < target
                                && equal_byte(
                                    &mut spool_source,
                                    entry.position - backward - 1,
                                    target - backward - 1,
                                )?
                            {
                                backward += 1;
                            }
                            backward
                        }
                    };
                    let src = entry.position - backward;
                    let dst = target - backward;
                    let mut length = backward
                        .checked_add(seed)
                        .ok_or_else(|| Error::invalid_match("m4 match length overflows"))?;
                    length = match snapshot.as_ref() {
                        Some(accelerator) => {
                            accelerator.extend_periodic(src, dst, length, distance, spool.len)?
                        }
                        None => {
                            while dst.checked_add(length).ok_or_else(|| {
                                Error::invalid_match("m4 target position overflows")
                            })? < spool.len
                                && equal_periodic_byte(
                                    &mut spool_source,
                                    src,
                                    length % distance,
                                    distance,
                                    dst.checked_add(length).ok_or_else(|| {
                                        Error::invalid_match("m4 target position overflows")
                                    })?,
                                )?
                            {
                                length = length.checked_add(1).ok_or_else(|| {
                                    Error::invalid_match("m4 match length overflows")
                                })?;
                            }
                            length
                        }
                    };
                    if length >= config.min_match {
                        let candidate = MatchCandidate {
                            src,
                            dst,
                            len: length,
                            insertion_ordinal: candidate_ordinal,
                        };
                        candidate_ordinal = candidate_ordinal.checked_add(1).ok_or_else(|| {
                            Error::invalid_match("m4 insertion ordinal overflows")
                        })?;
                        if output.len() == output.capacity() && output.capacity() >= 1_048_576 {
                            hash_cache = None;
                        }
                        if output.len() == output.capacity() && output.capacity() >= 1_048_576 {
                            hash_cache = None;
                        }
                        if let Some(exact_filter) = exact_filter.as_deref_mut() {
                            exact_filter.consider(&mut output, candidate)?;
                        } else if let Some(filter) = filter.as_deref_mut() {
                            filter.consider(&mut output, candidate)?;
                        } else {
                            output.push(candidate)?;
                        }
                    }
                }
                Ok(())
            },
        )?;
        last_visible_source = Some(next_source);
        last_uniform_target = if filter.is_some()
            && let Some(accelerator) = snapshot.as_ref()
            && accelerator.uniform_run(target, seed)? >= seed
        {
            Some(target)
        } else {
            None
        };
    }
    Ok((output, candidate_ordinal))
}

struct SeedDigestCache {
    seed: u64,
    digests: Option<BudgetedVec<Option<[u8; 16]>>>,
    equal_run: Option<BudgetedVec<u32>>,
}

impl SeedDigestCache {
    fn disabled(seed: u64) -> Self {
        Self {
            seed,
            digests: None,
            equal_run: None,
        }
    }

    fn try_new(length: u64, seed: u64, enabled: bool, context: &ResourceContext) -> Result<Self> {
        if !enabled || seed == 0 || length < seed {
            return Ok(Self::disabled(seed));
        }
        let count = usize::try_from(length - seed + 1)
            .map_err(|_| Error::memory_limit("digest cache count exceeds platform limits"))?;
        let mut digests = match BudgetedVec::with_capacity(count, &context.memory) {
            Ok(digests) => digests,
            Err(error) if error.kind() == ErrorKind::MemoryBudgetExceeded => {
                return Ok(Self::disabled(seed));
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = digests.resize(count, None) {
            if error.kind() == ErrorKind::MemoryBudgetExceeded {
                return Ok(Self::disabled(seed));
            }
            return Err(error);
        }
        let mut equal_run = match BudgetedVec::with_capacity(count, &context.memory) {
            Ok(equal_run) => equal_run,
            Err(error) if error.kind() == ErrorKind::MemoryBudgetExceeded => {
                return Ok(Self {
                    seed,
                    digests: Some(digests),
                    equal_run: None,
                });
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = equal_run.resize(count, 0) {
            if error.kind() == ErrorKind::MemoryBudgetExceeded {
                return Ok(Self {
                    seed,
                    digests: Some(digests),
                    equal_run: None,
                });
            }
            return Err(error);
        }
        Ok(Self {
            seed,
            digests: Some(digests),
            equal_run: Some(equal_run),
        })
    }

    fn get<D>(
        &mut self,
        digest_fn: &mut D,
        source: &mut dyn DataSource,
        position: u64,
        length: u64,
    ) -> Result<[u8; 16]>
    where
        D: FnMut(&mut dyn DataSource, u64, u64) -> Result<[u8; 16]>,
    {
        if let Some(digests) = self.digests.as_mut() {
            let index = usize::try_from(position).map_err(|_| {
                Error::memory_limit("digest cache position exceeds platform limits")
            })?;
            if let Some(Some(digest)) = digests.get(index).copied() {
                return Ok(digest);
            }
            let digest = digest_fn(source, position, length)?;
            if let Some(slot) = digests.get_mut(index) {
                *slot = Some(digest);
            }
            return Ok(digest);
        }
        digest_fn(source, position, length)
    }

    fn matching_stride_run<D>(
        &mut self,
        digest_fn: &mut D,
        source: &mut dyn DataSource,
        query: StrideRun,
    ) -> Result<u64>
    where
        D: FnMut(&mut dyn DataSource, u64, u64) -> Result<[u8; 16]>,
    {
        if query.remaining_quanta == 0 || self.seed == 0 {
            return Ok(0);
        }
        // Uniform-period stride only: reconstructed wrapping bytes equal the
        // contiguous seed at `source_position + phase`, so the digest cache is
        // valid. This path is never used for non-uniform wrapping.
        if let Some(equal_run) = self.equal_run.as_ref()
            && let Ok(index) = usize::try_from(query.target)
            && let Some(&run) = equal_run.get(index)
            && run > 0
        {
            return Ok(u64::from(run).min(query.remaining_quanta));
        }
        let mut matched = 0u64;
        while matched < query.remaining_quanta {
            let quantum = matched
                .checked_mul(self.seed)
                .ok_or_else(|| Error::invalid_match("m3 match length overflows"))?;
            let expected_position = query
                .source_position
                .checked_add((query.source_phase + quantum) % query.distance)
                .ok_or_else(|| Error::invalid_match("m3 source position overflows"))?;
            let actual_position = query
                .target
                .checked_add(quantum)
                .ok_or_else(|| Error::invalid_match("m3 target position overflows"))?;
            let expected = self.get(digest_fn, source, expected_position, self.seed)?;
            let actual = self.get(digest_fn, source, actual_position, self.seed)?;
            if expected != actual {
                break;
            }
            matched += 1;
        }
        if let Some(equal_run) = self.equal_run.as_mut()
            && let Ok(index) = usize::try_from(query.target)
            && let Some(slot) = equal_run.get_mut(index)
        {
            *slot = u32::try_from(matched).unwrap_or(u32::MAX);
        }
        Ok(matched.min(query.remaining_quanta))
    }
}

struct StrideRun {
    source_position: u64,
    source_phase: u64,
    distance: u64,
    target: u64,
    remaining_quanta: u64,
}

struct PeriodicSource<'a> {
    inner: &'a mut dyn DataSource,
    base: u64,
    start: u64,
    period: u64,
    length: u64,
}

impl DataSource for PeriodicSource<'_> {
    fn len(&self) -> u64 {
        self.length
    }

    fn read_at(&mut self, position: u64, destination: &mut [u8]) -> Result<()> {
        let requested = u64::try_from(destination.len())
            .map_err(|_| Error::invalid_match("periodic digest read exceeds u64"))?;
        let end = position
            .checked_add(requested)
            .ok_or_else(|| Error::invalid_match("periodic digest read endpoint overflows"))?;
        if end > self.length {
            return Err(Error::truncated("periodic digest read exceeds window"));
        }
        let offset = self
            .start
            .checked_add(position)
            .ok_or_else(|| Error::invalid_match("periodic digest offset overflows"))?
            % self.period;
        read_periodic(self.inner, self.base, offset, self.period, destination)
    }
}

fn extension_expected_digest<D>(
    digest_cache: &mut SeedDigestCache,
    digest_fn: &mut D,
    source: &mut dyn DataSource,
    base: u64,
    offset: u64,
    period: u64,
    length: u64,
) -> Result<[u8; 16]>
where
    D: FnMut(&mut dyn DataSource, u64, u64) -> Result<[u8; 16]>,
{
    if period == 0 {
        return Err(Error::invalid_match("periodic distance is zero"));
    }
    let offset = offset % period;
    if length <= period.saturating_sub(offset) {
        let position = base
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("periodic source position overflows"))?;
        return digest_cache.get(digest_fn, source, position, length);
    }
    let mut view = PeriodicSource {
        inner: source,
        base,
        start: offset,
        period,
        length,
    };
    digest_fn(&mut view, 0, length)
}

fn cached_hash<H>(
    cache: &mut Option<BudgetedVec<Option<u64>>>,
    hash_fn: &mut H,
    source: &mut dyn DataSource,
    position: u64,
    length: u64,
) -> Result<u64>
where
    H: FnMut(&mut dyn DataSource, u64, u64) -> Result<u64>,
{
    if let Some(cache) = cache.as_mut() {
        let index = usize::try_from(position)
            .map_err(|_| Error::memory_limit("hash cache position exceeds platform limits"))?;
        if let Some(existing) = cache.get(index).copied().flatten() {
            return Ok(existing);
        }
        let hash = hash_fn(source, position, length)?;
        if let Some(slot) = cache.get_mut(index) {
            *slot = Some(hash);
        }
        return Ok(hash);
    }
    hash_fn(source, position, length)
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
    source: &mut (impl DataSource + ?Sized),
    base: u64,
    mut offset: u64,
    period: u64,
    destination: &mut [u8],
) -> Result<()> {
    if period == 0 {
        return Err(Error::invalid_match("periodic distance is zero"));
    }
    offset %= period;
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

    fn accelerated_source(bytes: &[u8]) -> (InputSpool, ResourceContext) {
        let resources = ResourceConfig {
            memory: 256 * 1024 * 1024,
            temp_limit: 256 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let context = ResourceContext::with_resources(&resources).unwrap();
        let spool = crate::codec::spool_input(bytes, &resources, &context).unwrap();
        (spool, context)
    }

    fn planted_accelerated_input(seed: usize) -> Vec<u8> {
        let mut input = vec![0u8; 256 * 1024];
        let mut state = 0xC0FFEE_u64;
        for byte in input.iter_mut() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *byte = ((state >> 32) as u8) | 1;
        }
        let mut pattern = Vec::with_capacity(seed);
        let mut state = 0xA5A5_u64;
        for _ in 0..seed {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            pattern.push(((state >> 24) as u8) | 1);
        }
        input[..seed].copy_from_slice(&pattern);
        input[seed..2 * seed].copy_from_slice(&pattern);
        input[2 * seed..3 * seed].copy_from_slice(&pattern);
        input
    }

    #[test]
    fn m3_digest_collision_requires_exact_bytes_in_the_production_loop() {
        let (spool, context) = source(b"abcdefghABCDEFGHabcdefgh");
        let mut config = CompressionConfig::for_method(Method::M3FixedDigest);
        config.min_match = 8;
        config.seed_size = Some(8);
        let (candidates, _) =
            find_m3_spooled_with_digest(&spool, &config, &context, constant_digest, None).unwrap();
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
    fn m3_accelerated_extension_consults_injected_digest_before_exact() {
        let seed = 64usize;
        let input = planted_accelerated_input(seed);
        assert!(input.len() >= 256 * 1024);
        let (spool, context) = accelerated_source(&input);
        assert!(
            FixedAcceleration::try_new(&spool, &context)
                .unwrap()
                .is_some()
        );
        let mut config = CompressionConfig::for_method(Method::M3FixedDigest);
        config.min_match = seed as u64;
        config.seed_size = Some(seed as u64);
        reset_compare_counters();
        let mut digest_calls = 0u64;
        let mut rejected_calls = 0u64;
        let (candidates, _) = find_m3_spooled_with_digest(
            &spool,
            &config,
            &context,
            |source, position, length| {
                digest_calls += 1;
                if position == (2 * seed) as u64 && length == seed as u64 {
                    rejected_calls += 1;
                    return Ok([0xff; 16]);
                }
                blake3_at(source, position, length)
            },
            None,
        )
        .unwrap();
        let digest_compares = take_digest_compares();
        let exact_compares = take_exact_compares();
        assert!(rejected_calls >= 1, "extension digest must be queried");
        assert!(digest_calls > rejected_calls);
        assert!(digest_compares >= 1);
        assert_eq!(
            exact_compares, 0,
            "exact must not run after digest rejection"
        );
        assert!(
            candidates.iter().any(|candidate| {
                candidate.src == 0 && candidate.dst == seed as u64 && candidate.len == seed as u64
            }),
            "seed quantum must still emit: {:?}",
            candidates
                .iter()
                .filter(|candidate| candidate.src == 0)
                .copied()
                .collect::<Vec<_>>()
        );
        assert!(
            !candidates.iter().any(|candidate| {
                candidate.src == 0 && candidate.dst == seed as u64 && candidate.len > seed as u64
            }),
            "digest mismatch must stop extension even when exact bytes continue"
        );
    }

    #[test]
    fn m3_accelerated_uniform_and_overlap_still_use_injected_digest() {
        let seed = 32usize;
        let mut input = planted_accelerated_input(seed);
        input[..128].fill(b'A');
        input[256..384].fill(b'A');
        for (index, byte) in input[128..256].iter_mut().enumerate() {
            *byte = (index as u8).wrapping_add(1).max(1);
        }
        let (spool, context) = accelerated_source(&input);
        assert!(
            FixedAcceleration::try_new(&spool, &context)
                .unwrap()
                .is_some()
        );
        let mut config = CompressionConfig::for_method(Method::M3FixedDigest);
        config.min_match = 40;
        config.seed_size = Some(seed as u64);
        let mut digest_calls = 0u64;
        let (candidates, _) = find_m3_spooled_with_digest(
            &spool,
            &config,
            &context,
            |source, position, length| {
                digest_calls += 1;
                blake3_at(source, position, length)
            },
            None,
        )
        .unwrap();
        assert!(digest_calls >= (input.len() as u64 / seed as u64));
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.len >= 64 && candidate.src < candidate.dst)
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.len % seed as u64 == 0)
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.src == 0 && candidate.dst == 256 && candidate.len >= 64)
        );
    }

    #[test]
    fn m3_wrapping_extension_consults_injected_digest_not_hardcoded_blake3() {
        let seed = 24usize;
        let period = 40usize;
        let mut input = planted_accelerated_input(seed);
        let mut pattern = vec![0u8; period];
        for (index, byte) in pattern.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_add(3).max(1);
        }
        input[..period].copy_from_slice(&pattern);
        input[period..2 * period].copy_from_slice(&pattern);
        let (spool, context) = accelerated_source(&input);
        assert!(
            FixedAcceleration::try_new(&spool, &context)
                .unwrap()
                .is_some()
        );
        let mut config = CompressionConfig::for_method(Method::M3FixedDigest);
        config.min_match = seed as u64;
        config.seed_size = Some(seed as u64);
        reset_compare_counters();
        let mut wrapping_calls = 0u64;
        let (candidates, _) = find_m3_spooled_with_digest(
            &spool,
            &config,
            &context,
            |source, position, length| {
                if source.len() == seed as u64 && position == 0 && length == seed as u64 {
                    wrapping_calls += 1;
                    return Ok([0xee; 16]);
                }
                blake3_at(source, position, length)
            },
            None,
        )
        .unwrap();
        assert!(
            wrapping_calls >= 1,
            "wrapping expected digest must use the injected function"
        );
        let exact_compares = take_exact_compares();
        assert_eq!(
            exact_compares, 0,
            "exact must not run after wrapping digest rejection"
        );
        assert!(
            !candidates.iter().any(|candidate| {
                candidate.src == 0 && candidate.dst == period as u64 && candidate.len > seed as u64
            }),
            "injected wrapping mismatch must stop extension: {:?}",
            candidates
                .iter()
                .filter(|candidate| candidate.src == 0)
                .copied()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn m4_polynomial_collision_requires_exact_bytes_in_the_production_loop() {
        let (spool, context) = source(b"abcdefghABCDEFGHabcdefgh");
        let mut config = CompressionConfig::for_method(Method::M4Reread);
        config.min_match = 8;
        config.seed_size = Some(8);
        let (candidates, _) =
            find_m4_spooled_with_hash(&spool, &config, &context, constant_hash, None).unwrap();
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

    fn prefix_key(bytes: &[u8]) -> [u8; 8] {
        let mut key = [0u8; 8];
        let copied = bytes.len().min(8);
        key[..copied].copy_from_slice(&bytes[..copied]);
        key
    }

    fn independent_m3(input: &[u8], seed: usize, minimum: usize) -> Vec<MatchCandidate> {
        let mut by_seed = std::collections::HashMap::<[u8; 8], Vec<usize>>::new();
        for source in (0..=input.len().saturating_sub(seed)).step_by(seed) {
            by_seed
                .entry(prefix_key(&input[source..source + seed]))
                .or_default()
                .push(source);
        }
        let mut result = Vec::new();
        for target in 0..=input.len().saturating_sub(seed) {
            let Some(sources) = by_seed.get(&prefix_key(&input[target..target + seed])) else {
                continue;
            };
            for &source_start in sources.iter().rev() {
                if source_start >= target
                    || input[source_start..source_start + seed] != input[target..target + seed]
                {
                    continue;
                }
                let distance = target - source_start;
                let mut length = seed;
                while target + length + seed <= input.len()
                    && (0..seed).all(|offset| {
                        input[target + length + offset]
                            == input[source_start + (length + offset) % distance]
                    })
                {
                    length += seed;
                }
                if length >= minimum {
                    result.push(MatchCandidate {
                        src: source_start as u64,
                        dst: target as u64,
                        len: length as u64,
                        insertion_ordinal: result.len() as u64,
                    });
                }
            }
        }
        result
    }

    fn independent_m4(input: &[u8], seed: usize, minimum: usize) -> Vec<MatchCandidate> {
        let mut by_seed = std::collections::HashMap::<[u8; 8], Vec<usize>>::new();
        for source in (0..=input.len().saturating_sub(seed)).step_by(seed) {
            by_seed
                .entry(prefix_key(&input[source..source + seed]))
                .or_default()
                .push(source);
        }
        let mut result = Vec::new();
        for target in 0..=input.len().saturating_sub(seed) {
            let Some(sources) = by_seed.get(&prefix_key(&input[target..target + seed])) else {
                continue;
            };
            for &source_start in sources.iter().rev() {
                if source_start >= target
                    || input[source_start..source_start + seed] != input[target..target + seed]
                {
                    continue;
                }
                let distance = target - source_start;
                let mut backward = 0;
                while backward < source_start
                    && backward < target
                    && input[source_start - backward - 1] == input[target - backward - 1]
                {
                    backward += 1;
                }
                let source_shifted = source_start - backward;
                let target_shifted = target - backward;
                let mut length = backward + seed;
                while target_shifted + length < input.len()
                    && input[target_shifted + length] == input[source_shifted + length % distance]
                {
                    length += 1;
                }
                if length >= minimum {
                    result.push(MatchCandidate {
                        src: source_shifted as u64,
                        dst: target_shifted as u64,
                        len: length as u64,
                        insertion_ordinal: result.len() as u64,
                    });
                }
            }
        }
        result
    }

    fn sparse_threshold_input(seed: usize) -> Vec<u8> {
        let mut input = planted_accelerated_input(seed.max(1));
        let pattern = {
            let mut pattern = vec![0u8; seed];
            let mut state = 0x11FFu64;
            for byte in &mut pattern {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                *byte = ((state >> 17) as u8) | 1;
            }
            pattern
        };
        input[..seed].copy_from_slice(&pattern);
        input[seed..2 * seed].copy_from_slice(&pattern);
        input
    }

    fn nonuniform_repeated_blocks() -> Vec<u8> {
        let mut input = planted_accelerated_input(64);
        let mut block = vec![0u8; 96];
        block[..32].copy_from_slice(&(0..32).map(|index| (index + 3) as u8).collect::<Vec<_>>());
        block[32..64].fill(b'B');
        block[64..].copy_from_slice(&(0..32).map(|index| (index + 40) as u8).collect::<Vec<_>>());
        input[..96].copy_from_slice(&block);
        input[96..192].copy_from_slice(&block);
        input
    }

    #[test]
    fn backward_equal_continues_past_equal_length_uniform_suffixes() {
        let mut input = planted_accelerated_input(8);
        input[..8].copy_from_slice(b"ABBBABBB");
        let (spool, context) = accelerated_source(&input);
        let accel = FixedAcceleration::try_new(&spool, &context)
            .unwrap()
            .expect("threshold acceleration");
        assert_eq!(accel.backward_equal(4, 8).unwrap(), 4);

        input[..8].copy_from_slice(b"XACBYACB");
        let (spool, context) = accelerated_source(&input);
        let accel = FixedAcceleration::try_new(&spool, &context)
            .unwrap()
            .expect("threshold acceleration");
        assert_eq!(accel.backward_equal(4, 8).unwrap(), 3);
    }

    #[test]
    fn acceleration_on_off_matches_independent_oracle_and_releases() {
        let seed = 16 * 1024usize;
        let input = sparse_threshold_input(seed);
        assert_eq!(input.len(), 256 * 1024);
        let (spool, context) = accelerated_source(&input);
        assert_eq!(last_acceleration_active(), None);
        assert!(
            FixedAcceleration::try_new(&spool, &context)
                .unwrap()
                .is_some()
        );
        assert_eq!(last_acceleration_active(), Some(true));

        let mut m3 = CompressionConfig::for_method(Method::M3FixedDigest);
        m3.min_match = seed as u64;
        m3.seed_size = Some(seed as u64);
        let mut m4 = CompressionConfig::for_method(Method::M4Reread);
        m4.min_match = seed as u64;
        m4.seed_size = Some(seed as u64);

        let expected_m3 = independent_m3(&input, seed, seed);
        let expected_m4 = independent_m4(&input, seed, seed);
        let (on_m3, on_next) =
            find_m3_spooled_with_digest(&spool, &m3, &context, blake3_at, None).unwrap();
        assert_eq!(last_acceleration_active(), Some(true));
        let (on_m4, _) =
            find_m4_spooled_with_hash(&spool, &m4, &context, polynomial_at, None).unwrap();
        assert_eq!(on_m3.as_slice(), expected_m3.as_slice());
        assert_eq!(on_m4.as_slice(), expected_m4.as_slice());
        assert_eq!(on_next, expected_m3.len() as u64);

        let _guard = disable_acceleration();
        assert!(
            FixedAcceleration::try_new(&spool, &context)
                .unwrap()
                .is_none()
        );
        assert_eq!(last_acceleration_active(), Some(false));
        let (off_m3, _) =
            find_m3_spooled_with_digest(&spool, &m3, &context, blake3_at, None).unwrap();
        let (off_m4, _) =
            find_m4_spooled_with_hash(&spool, &m4, &context, polynomial_at, None).unwrap();
        assert_eq!(off_m3.as_slice(), expected_m3.as_slice());
        assert_eq!(off_m4.as_slice(), expected_m4.as_slice());
        drop(on_m3);
        drop(on_m4);
        drop(off_m3);
        drop(off_m4);
        drop(spool);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }

    #[test]
    fn m4_backward_span_over_uniform_suffix_matches_oracle() {
        let input = nonuniform_repeated_blocks();
        assert_eq!(input.len(), 256 * 1024);
        let (spool, context) = accelerated_source(&input);
        assert!(
            FixedAcceleration::try_new(&spool, &context)
                .unwrap()
                .is_some()
        );
        let mut config = CompressionConfig::for_method(Method::M4Reread);
        config.min_match = 32;
        config.seed_size = Some(32);
        config.max_distance = Some(192);
        let expected = independent_m4(&input, 32, 32);
        let (actual, _) =
            find_m4_spooled_with_hash(&spool, &config, &context, polynomial_at, None).unwrap();
        assert_eq!(last_acceleration_active(), Some(true));
        assert_eq!(actual.as_slice(), expected.as_slice());
        assert!(
            actual.iter().any(|candidate| {
                candidate.src == 0 && candidate.dst == 96 && candidate.len >= 96
            }),
            "backward match must span the uniform B suffix: {:?}",
            actual.as_slice()
        );
    }

    #[test]
    fn compact_overlay_starts_at_full_next_ordinal_not_max_retained() {
        let input = [0u8; 64];
        let (spool, context) = source(&input);
        let mut config = CompressionConfig::for_method(Method::M4Reread);
        config.min_match = 32;
        config.seed_size = Some(8);
        config.rep_overlay = Some(crate::config::RepConfig {
            distance: 32,
            min_match: 32,
        });
        let (full_base, full_next) =
            find_m4_spooled_with_hash(&spool, &config, &context, polynomial_at, None).unwrap();
        let mut filter = IdenticalIntervalFilter::new(&context.memory).unwrap();
        let (compact_base, compact_next) =
            find_m4_spooled_with_hash(&spool, &config, &context, polynomial_at, Some(&mut filter))
                .unwrap();
        assert_eq!(full_next, compact_next);
        assert_eq!(full_next, full_base.len() as u64);
        assert!(compact_base.len() as u64 <= 80, "{}", compact_base.len());
        let compact_max = compact_base
            .iter()
            .map(|candidate| candidate.insertion_ordinal)
            .max()
            .unwrap();
        assert!(
            compact_next > compact_max + 1,
            "full next {compact_next} must outrun max retained {}",
            compact_max + 1
        );
        let with_overlay =
            append_overlay_from(&spool, &config, &context, compact_base, compact_next, None)
                .unwrap();
        assert!(
            with_overlay
                .iter()
                .any(|candidate| candidate.insertion_ordinal == compact_next)
        );
        assert!(
            overlay_ordinal_start(&[MatchCandidate {
                src: 0,
                dst: 1,
                len: 32,
                insertion_ordinal: u64::MAX,
            }])
            .is_err()
        );
    }

    #[test]
    fn acceleration_allocation_failure_falls_back_without_changing_candidates() {
        let seed = 16 * 1024usize;
        let input = sparse_threshold_input(seed);
        let (spool, context) = accelerated_source(&input);
        let mut config = CompressionConfig::for_method(Method::M3FixedDigest);
        config.min_match = seed as u64;
        config.seed_size = Some(seed as u64);
        let expected = independent_m3(&input, seed, seed);
        for stage in 1..=3 {
            let _deny = deny_acceleration_stage(stage);
            assert!(
                FixedAcceleration::try_new(&spool, &context)
                    .unwrap()
                    .is_none(),
                "stage {stage} must deny construction"
            );
            assert_eq!(last_acceleration_active(), Some(false));
        }
        let _guard = limit_acceleration_memory(64);
        assert!(
            FixedAcceleration::try_new(&spool, &context)
                .unwrap()
                .is_none()
        );
        assert_eq!(last_acceleration_active(), Some(false));
        let (actual, _) =
            find_m3_spooled_with_digest(&spool, &config, &context, blake3_at, None).unwrap();
        assert_eq!(actual.as_slice(), expected.as_slice());
        drop(actual);
        drop(spool);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }

    #[test]
    fn accelerated_digest_rejection_skips_exact_on_seed_and_stride() {
        let seed = 64usize;
        let mut input = planted_accelerated_input(seed);
        input[..256].fill(b'Q');
        let (spool, context) = accelerated_source(&input);
        assert!(
            FixedAcceleration::try_new(&spool, &context)
                .unwrap()
                .is_some()
        );
        let mut config = CompressionConfig::for_method(Method::M3FixedDigest);
        config.min_match = seed as u64;
        config.seed_size = Some(seed as u64);
        reset_compare_counters();
        let mut poison_at = std::collections::HashSet::new();
        poison_at.insert(seed as u64);
        poison_at.insert((3 * seed) as u64);
        let (candidates, _) = find_m3_spooled_with_digest(
            &spool,
            &config,
            &context,
            |source, position, length| {
                if poison_at.contains(&position) {
                    return Ok([0x11; 16]);
                }
                blake3_at(source, position, length)
            },
            None,
        )
        .unwrap();
        let digest_compares = take_digest_compares();
        let exact_compares = take_exact_compares();
        assert!(digest_compares >= 1);
        assert!(
            !candidates.iter().any(|candidate| {
                candidate.src == 0 && candidate.dst == seed as u64 && candidate.len > seed as u64
            }),
            "digest rejection must stop stride jump: {:?}",
            candidates.as_slice()
        );
        assert!(
            exact_compares <= digest_compares,
            "exact {exact_compares} must not outrun digest {digest_compares}"
        );
    }
}
