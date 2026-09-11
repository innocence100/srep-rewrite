use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::{
    config::ResourceConfig,
    error::{Error, Result},
    resource::ResourceContext,
};

use super::{
    Decoded, LegacyBoundary, checksum,
    header::{Header, Layout},
    legacy_block_boundary,
    storage::OutputStore,
};

struct Block {
    start: u64,
    len: u64,
    digest: [u8; 64],
    digest_len: usize,
    stats_pos: u64,
    stats_len: u64,
    literal_pos: u64,
    literal_len: u64,
}

fn u32le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}

fn read_block(file: &mut File, h: &Header, pos: &mut u64) -> Result<Option<Block>> {
    let file_len = file.seek(SeekFrom::End(0)).map_err(Error::temp_storage)?;
    match legacy_block_boundary(file, *pos, file_len)? {
        LegacyBoundary::End => return Ok(None),
        LegacyBoundary::Block => {}
    }
    file.seek(SeekFrom::Start(*pos))
        .map_err(Error::temp_storage)?;
    let mut frame = [0u8; 12];
    file.read_exact(&mut frame)
        .map_err(|e| Error::map_eof(e, "truncated legacy block header"))?;
    let literal_len = u64::from(u32le(&frame[..4]));
    let logical_len = u64::from(u32le(&frame[4..8]));
    let stats_len = u64::from(u32le(&frame[8..]));
    if logical_len == 0 {
        return Err(Error::corrupt_record(
            "legacy physical block length must be positive",
        ));
    }
    let payload = u64::try_from(h.checksum.width())
        .unwrap()
        .checked_add(stats_len)
        .and_then(|x| x.checked_add(literal_len))
        .ok_or_else(|| Error::corrupt_record("legacy block payload overflows"))?;
    let end = (*pos)
        .checked_add(12)
        .and_then(|x| x.checked_add(payload))
        .ok_or_else(|| Error::corrupt_record("legacy block range overflows"))?;
    if end > file_len {
        return Err(Error::truncated("truncated legacy block payload"));
    }
    let digest_pos = *pos + 12;
    let stats_pos = digest_pos + u64::try_from(h.checksum.width()).unwrap();
    let literal_pos = stats_pos + stats_len;
    let mut digest = [0u8; 64];
    file.seek(SeekFrom::Start(digest_pos))
        .map_err(Error::temp_storage)?;
    file.read_exact(&mut digest[..h.checksum.width()])
        .map_err(|e| Error::map_eof(e, "truncated legacy block digest"))?;
    *pos = end;
    Ok(Some(Block {
        start: 0,
        len: logical_len,
        digest,
        digest_len: h.checksum.width(),
        stats_pos,
        stats_len,
        literal_pos,
        literal_len,
    }))
}

pub fn decode(
    file: &mut File,
    h: &Header,
    r: &ResourceConfig,
    c: &ResourceContext,
) -> Result<Decoded> {
    let mut output = OutputStore::new(r, c)?;
    let mut pos = h.header_end;
    let mut total = 0u64;
    let mut blocks = 0u64;
    let mut matches = 0u64;
    let mut covered = 0u64;
    let width = if h.layout == Layout::IoRounded {
        12
    } else {
        16
    };
    while let Some(mut block) = read_block(file, h, &mut pos)? {
        block.start = total;
        total = total
            .checked_add(block.len)
            .ok_or_else(|| Error::output_limit("legacy output length overflows"))?;
        if total > r.output_limit {
            return Err(Error::output_limit("legacy output exceeds output limit"));
        }
        if block.stats_len % width as u64 != 0 {
            return Err(Error::corrupt_record(
                "legacy statistics are not record-aligned",
            ));
        }
        file.seek(SeekFrom::Start(block.stats_pos))
            .map_err(Error::temp_storage)?;
        let mut stats_left = block.stats_len;
        let mut cursor = block.start;
        let mut literal_cursor = block.literal_pos;
        while stats_left > 0 {
            let mut rec = [0u8; 16];
            file.read_exact(&mut rec[..width])
                .map_err(|e| Error::map_eof(e, "truncated legacy statistics"))?;
            let literal_len = u64::from(u32le(&rec[..4]));
            let actual = cursor
                .checked_add(literal_len)
                .ok_or_else(|| Error::invalid_match("legacy literal cursor overflows"))?;
            let distance = if width == 12 {
                u64::from(u32le(&rec[4..8]))
                    .checked_mul(u64::from(h.base_len))
                    .ok_or_else(|| Error::invalid_match("legacy rounded distance overflows"))?
            } else {
                u64::from(u32le(&rec[4..8])) | (u64::from(u32le(&rec[8..12])) << 32)
            };
            let rounded = if width == 12 {
                (actual / u64::from(h.base_len))
                    .checked_mul(u64::from(h.base_len))
                    .ok_or_else(|| Error::invalid_match("legacy rounded destination overflows"))?
            } else {
                actual
            };
            let length = if width == 12 {
                u64::from(u32le(&rec[8..12]))
                    .checked_add(1)
                    .and_then(|x| x.checked_mul(u64::from(h.base_len)))
                    .ok_or_else(|| Error::invalid_match("legacy rounded length overflows"))?
            } else {
                u64::from(u32le(&rec[12..]))
                    .checked_add(u64::from(h.base_len))
                    .ok_or_else(|| Error::invalid_match("legacy match length overflows"))?
            };
            if literal_len
                > block
                    .literal_len
                    .saturating_sub(literal_cursor - block.literal_pos)
            {
                return Err(Error::corrupt_record("legacy literal stream is too short"));
            }
            file.seek(SeekFrom::Start(literal_cursor))
                .map_err(Error::temp_storage)?;
            for i in 0..literal_len {
                let mut b = [0u8; 1];
                file.read_exact(&mut b)
                    .map_err(|e| Error::map_eof(e, "truncated legacy literals"))?;
                output.write_at(
                    actual
                        .checked_sub(literal_len)
                        .and_then(|start| start.checked_add(i))
                        .ok_or_else(|| {
                            Error::invalid_match("legacy literal destination underflows")
                        })?,
                    &b,
                )?;
            }
            literal_cursor = literal_cursor
                .checked_add(literal_len)
                .ok_or_else(|| Error::corrupt_record("legacy literal offset overflows"))?;
            let src = rounded
                .checked_sub(distance)
                .ok_or_else(|| Error::invalid_match("legacy match source underflows"))?;
            let end = actual
                .checked_add(length)
                .ok_or_else(|| Error::invalid_match("legacy match endpoint overflows"))?;
            if distance == 0 || src >= actual || end > block.start + block.len {
                return Err(Error::invalid_match("legacy match bounds are invalid"));
            }
            for i in 0..length {
                let p = src
                    .checked_add(i)
                    .ok_or_else(|| Error::invalid_match("legacy source overflows"))?;
                let mut z = [0];
                output.read_at(p, &mut z)?;
                let b = z[0];
                output.write_at(actual + i, &[b])?;
            }
            cursor = end;
            matches += 1;
            covered = covered
                .checked_add(length)
                .ok_or_else(|| Error::corrupt_record("legacy coverage overflows"))?;
            stats_left -= u64::try_from(width).unwrap();
        }
        let remaining_literals = block
            .literal_len
            .checked_sub(literal_cursor - block.literal_pos)
            .ok_or_else(|| Error::corrupt_record("legacy literal cursor overflows"))?;
        file.seek(SeekFrom::Start(literal_cursor))
            .map_err(Error::temp_storage)?;
        for i in 0..remaining_literals {
            let mut b = [0u8; 1];
            file.read_exact(&mut b)
                .map_err(|e| Error::map_eof(e, "truncated legacy literals"))?;
            output.write_at(cursor + i, &b)?;
        }
        if cursor.checked_add(remaining_literals) != Some(block.start + block.len) {
            return Err(Error::corrupt_record(
                "legacy reconstructed block length mismatch",
            ));
        }
        checksum::verify_reader(
            h.checksum,
            &h.seed,
            &mut output,
            block.start,
            block.len,
            &block.digest[..block.digest_len],
        )?;
        blocks += 1;
    }
    Ok(Decoded {
        output,
        original_size: total,
        block_count: blocks,
        match_count: matches,
        covered_bytes: covered,
        literal_bytes: total
            .checked_sub(covered)
            .ok_or_else(|| Error::corrupt_record("legacy literal count underflows"))?,
    })
}
