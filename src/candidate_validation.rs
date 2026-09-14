//! Candidate collection and authoritative byte validation for the NG v3 writer.
//!
//! This module intentionally contains no archive-format code.  Finder output is
//! checked against the spooled source before the shared Match IR normalizer is
//! allowed to select it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::codec::InputSpool;
use crate::error::{Error, Result};
use crate::match_ir::MatchCandidate;
use crate::resource::{BudgetedVec, MemoryBudget};

pub(crate) fn collect_candidates<I: IntoIterator<Item = MatchCandidate>>(
    candidates: I,
    budget: &MemoryBudget,
) -> Result<BudgetedVec<MatchCandidate>> {
    let mut collected = BudgetedVec::new(budget)?;
    for candidate in candidates {
        collected.push(candidate)?;
    }
    Ok(collected)
}

pub(crate) fn validate_candidates(
    spool: &InputSpool,
    candidates: &mut BudgetedVec<MatchCandidate>,
    min_match: u64,
) -> Result<()> {
    canonicalize_candidates(candidates);
    for candidate in candidates.as_slice() {
        let end = candidate
            .dst
            .checked_add(candidate.len)
            .ok_or_else(|| Error::invalid_match("candidate destination overflows"))?;
        if candidate.src >= candidate.dst || end > spool.len || candidate.src >= spool.len {
            return Err(Error::invalid_match("candidate interval is invalid"));
        }
        if candidate.len <= 25 {
            continue;
        }
        if candidate.len < min_match {
            return Err(Error::invalid_match(
                "candidate is shorter than minimum match",
            ));
        }
        let distance = candidate.dst - candidate.src;
        let mut expected = [0u8; 4096];
        let mut actual = [0u8; 4096];
        let mut file = spool.file.try_clone().map_err(Error::temp_storage)?;
        let mut offset = 0u64;
        let mut period = [0u8; 4096];
        let period_len = if distance <= period.len() as u64 {
            let len = usize::try_from(distance)
                .map_err(|_| Error::invalid_match("candidate distance exceeds platform limits"))?;
            file.seek(SeekFrom::Start(candidate.src))
                .map_err(Error::temp_storage)?;
            file.read_exact(&mut period[..len])
                .map_err(|error| Error::map_eof(error, "candidate source is unavailable"))?;
            len
        } else {
            0
        };
        while offset < candidate.len {
            let count = usize::try_from((candidate.len - offset).min(expected.len() as u64))
                .map_err(|_| {
                    Error::invalid_match("candidate validation length exceeds platform limits")
                })?;
            if period_len == 0 {
                read_periodic_file(
                    &mut file,
                    candidate.src,
                    offset % distance,
                    distance,
                    &mut expected[..count],
                )?;
            } else {
                let phase = usize::try_from(offset % distance)
                    .map_err(|_| Error::invalid_match("candidate periodic phase overflows"))?;
                for (index, byte) in expected[..count].iter_mut().enumerate() {
                    *byte = period[(phase + index) % period_len];
                }
            }
            let target_position = candidate
                .dst
                .checked_add(offset)
                .ok_or_else(|| Error::invalid_match("candidate destination overflows"))?;
            file.seek(SeekFrom::Start(target_position))
                .map_err(Error::temp_storage)?;
            file.read_exact(&mut actual[..count])
                .map_err(|error| Error::map_eof(error, "candidate destination is unavailable"))?;
            if expected[..count] != actual[..count] {
                return Err(Error::invalid_match("candidate bytes do not match"));
            }
            offset = offset
                .checked_add(count as u64)
                .ok_or_else(|| Error::invalid_match("candidate validation offset overflows"))?;
        }
    }
    Ok(())
}

fn canonicalize_candidates(candidates: &mut BudgetedVec<MatchCandidate>) {
    candidates.sort_unstable_by(|a, b| {
        (a.src, a.dst, a.len, a.insertion_ordinal).cmp(&(b.src, b.dst, b.len, b.insertion_ordinal))
    });
    candidates.dedup_by(|a, b| {
        if (a.src, a.dst, a.len) == (b.src, b.dst, b.len) {
            a.insertion_ordinal = a.insertion_ordinal.min(b.insertion_ordinal);
            true
        } else {
            false
        }
    });
}

fn read_periodic_file(
    file: &mut File,
    base: u64,
    mut offset: u64,
    period: u64,
    destination: &mut [u8],
) -> Result<()> {
    if period == 0 {
        return Err(Error::invalid_match("candidate periodic distance is zero"));
    }
    let mut written = 0usize;
    while written < destination.len() {
        let count = usize::try_from((period - offset).min((destination.len() - written) as u64))
            .map_err(|_| Error::invalid_match("candidate periodic read exceeds platform limits"))?;
        let position = base
            .checked_add(offset)
            .ok_or_else(|| Error::invalid_match("candidate periodic source overflows"))?;
        file.seek(SeekFrom::Start(position))
            .map_err(Error::temp_storage)?;
        file.read_exact(&mut destination[written..written + count])
            .map_err(|error| Error::map_eof(error, "candidate source is unavailable"))?;
        written += count;
        offset = 0;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::codec::spool_input;
    use crate::config::ResourceConfig;
    use crate::resource::ResourceContext;

    fn candidate(src: u64, dst: u64, len: u64, ordinal: u64) -> MatchCandidate {
        MatchCandidate {
            src,
            dst,
            len,
            insertion_ordinal: ordinal,
        }
    }

    #[test]
    fn validation_canonicalizes_interleaved_duplicate_triples_and_keeps_minimum_ordinal() {
        let input = b"0123456789abcdef".repeat(1024);
        let resources = ResourceConfig::default();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let spool = spool_input(Cursor::new(&input), &resources, &context).unwrap();
        let mut candidates = BudgetedVec::new(&context.memory).unwrap();
        for item in [
            candidate(0, 4096, 8192, 91),
            candidate(0, 4096, 8192, 7),
            candidate(0, 4096, 8192, 43),
        ] {
            candidates.push(item).unwrap();
        }
        validate_candidates(&spool, &mut candidates, 512).unwrap();
        assert_eq!(candidates.as_slice(), &[candidate(0, 4096, 8192, 7)]);
    }

    #[test]
    fn validation_chunked_periodic_comparison_is_exact_for_large_distances() {
        let block: Vec<u8> = (0..8192).map(|value| (value * 17) as u8).collect();
        let input = [block.as_slice(), block.as_slice()].concat();
        let resources = ResourceConfig::default();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let spool = spool_input(Cursor::new(&input), &resources, &context).unwrap();
        let mut candidates = BudgetedVec::new(&context.memory).unwrap();
        candidates.push(candidate(0, 8192, 8192, 0)).unwrap();
        validate_candidates(&spool, &mut candidates, 512).unwrap();

        let mut corrupted = input.clone();
        corrupted[8192 + 4096] ^= 1;
        let corrupted_spool = spool_input(Cursor::new(&corrupted), &resources, &context).unwrap();
        let mut invalid = BudgetedVec::new(&context.memory).unwrap();
        invalid.push(candidate(0, 8192, 8192, 0)).unwrap();
        assert_eq!(
            validate_candidates(&corrupted_spool, &mut invalid, 512)
                .unwrap_err()
                .kind(),
            crate::error::ErrorKind::InvalidMatch
        );
    }
}
