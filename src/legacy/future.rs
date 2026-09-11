use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::{
    config::ResourceConfig,
    error::{Error, Result},
    resource::{Reservation, ResourceContext},
};

use super::{
    Decoded, LegacyBoundary, checksum,
    header::{Header, Layout},
    legacy_block_boundary,
    storage::OutputStore,
};

const BLOCK_RECORD_SIZE: u64 = 56;
const FRAGMENT_RECORD_SIZE: u64 = 40;
const SIZE_RECORD_SIZE: u64 = 8;
const RUN_RECORDS: u64 = 256;
const MAX_MERGE_LEVELS: usize = 57;

const fn initial_runs_for_fragments(fragments: u64) -> u64 {
    fragments.div_ceil(RUN_RECORDS)
}

const fn merge_levels_for_initial_runs(runs: u64) -> usize {
    let mut remaining = runs;
    let mut levels = 0;
    while remaining != 0 {
        levels += 1;
        remaining >>= 1;
    }
    levels
}

const _: () = assert!(
    merge_levels_for_initial_runs(initial_runs_for_fragments(u64::MAX)) == MAX_MERGE_LEVELS
);

#[derive(Clone, Copy)]
struct Block {
    start: u64,
    len: u64,
    digest_pos: u64,
    stats_pos: u64,
    stats_len: u64,
    literal_pos: u64,
    literal_len: u64,
}

#[derive(Clone, Copy)]
struct Fragment {
    src: u64,
    dst: u64,
    len: u64,
    source_block: u64,
    ordinal: u64,
}

struct DiskFile {
    file: File,
    _temp: tempfile::NamedTempFile,
    reservation: Reservation,
    len: u64,
}

struct MergeRun {
    path: tempfile::TempPath,
    _reservation: Reservation,
    count: u64,
}

impl MergeRun {
    fn open(&self) -> Result<File> {
        File::open(&self.path).map_err(Error::temp_storage)
    }
}

impl DiskFile {
    fn new(prefix: &str, resources: &ResourceConfig, context: &ResourceContext) -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix(prefix)
            .tempfile_in(&resources.temp_dir)
            .map_err(Error::temp_storage)?;
        let file = temp.reopen().map_err(Error::temp_storage)?;
        Ok(Self {
            file,
            _temp: temp,
            reservation: context.temp.reserve(0)?,
            len: 0,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> Result<u64> {
        let offset = self.len;
        let amount = u64::try_from(bytes.len())
            .map_err(|_| Error::temp_limit("temporary record length overflows"))?;
        self.reservation.grow(amount)?;
        if let Err(error) = self
            .file
            .seek(SeekFrom::Start(offset))
            .map_err(Error::temp_storage)
            .and_then(|_| self.file.write_all(bytes).map_err(Error::temp_storage))
        {
            self.reservation.shrink(amount);
            return Err(error);
        }
        self.len = self
            .len
            .checked_add(amount)
            .ok_or_else(|| Error::temp_limit("temporary file length overflows"))?;
        Ok(offset)
    }

    fn into_merge_run(self, count: u64) -> MergeRun {
        let DiskFile {
            file,
            _temp,
            reservation,
            ..
        } = self;
        drop(file);
        MergeRun {
            path: _temp.into_temp_path(),
            _reservation: reservation,
            count,
        }
    }
}

struct Blocks {
    disk: DiskFile,
    count: u64,
}
impl Blocks {
    fn new(r: &ResourceConfig, c: &ResourceContext) -> Result<Self> {
        Ok(Self {
            disk: DiskFile::new("srep-legacy-blocks-", r, c)?,
            count: 0,
        })
    }
    fn push(&mut self, block: Block) -> Result<()> {
        let fields = [
            block.start,
            block.len,
            block.digest_pos,
            block.stats_pos,
            block.stats_len,
            block.literal_pos,
            block.literal_len,
        ];
        let mut bytes = [0u8; 56];
        for (index, value) in fields.into_iter().enumerate() {
            let start = index * 8;
            bytes[start..start + 8].copy_from_slice(&value.to_le_bytes());
        }
        self.disk.append(&bytes)?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| Error::temp_limit("block count overflows"))?;
        Ok(())
    }
    fn get(&mut self, index: u64) -> Result<Block> {
        let offset = index
            .checked_mul(BLOCK_RECORD_SIZE)
            .ok_or_else(|| Error::temp_limit("block offset overflows"))?;
        self.disk
            .file
            .seek(SeekFrom::Start(offset))
            .map_err(Error::temp_storage)?;
        let mut b = [0u8; 56];
        self.disk
            .file
            .read_exact(&mut b)
            .map_err(Error::temp_storage)?;
        let field =
            |start: usize| -> u64 { u64::from_le_bytes(b[start..start + 8].try_into().unwrap()) };
        Ok(Block {
            start: field(0),
            len: field(8),
            digest_pos: field(16),
            stats_pos: field(24),
            stats_len: field(32),
            literal_pos: field(40),
            literal_len: field(48),
        })
    }
}

struct Sizes {
    disk: DiskFile,
    count: u64,
}
impl Sizes {
    fn new(r: &ResourceConfig, c: &ResourceContext) -> Result<Self> {
        Ok(Self {
            disk: DiskFile::new("srep-legacy-sizes-", r, c)?,
            count: 0,
        })
    }
    fn push(&mut self, value: u64) -> Result<()> {
        self.disk.append(&value.to_le_bytes())?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| Error::corrupt_index("legacy v4 block count overflows"))?;
        Ok(())
    }
    fn get(&mut self, index: u64) -> Result<u64> {
        let offset = index
            .checked_mul(SIZE_RECORD_SIZE)
            .ok_or_else(|| Error::corrupt_index("legacy v4 size offset overflows"))?;
        self.disk
            .file
            .seek(SeekFrom::Start(offset))
            .map_err(Error::temp_storage)?;
        let mut b = [0u8; 8];
        self.disk
            .file
            .read_exact(&mut b)
            .map_err(Error::temp_storage)?;
        Ok(u64::from_le_bytes(b))
    }
}

struct Fragments {
    disk: DiskFile,
    count: u64,
}

struct Periods {
    disk: DiskFile,
}

impl Periods {
    fn new(r: &ResourceConfig, c: &ResourceContext) -> Result<Self> {
        Ok(Self {
            disk: DiskFile::new("srep-legacy-periods-", r, c)?,
        })
    }
    fn append_from_output(&mut self, output: &mut OutputStore, src: u64, len: u64) -> Result<u64> {
        let start = self.disk.len;
        for offset in 0..len {
            let mut byte = [0u8; 1];
            output.read_at(
                src.checked_add(offset)
                    .ok_or_else(|| Error::invalid_match("future period source overflows"))?,
                &mut byte,
            )?;
            self.disk.append(&byte)?;
        }
        Ok(start)
    }
    fn read_byte(&mut self, position: u64) -> Result<u8> {
        self.disk
            .file
            .seek(SeekFrom::Start(position))
            .map_err(Error::temp_storage)?;
        let mut byte = [0u8; 1];
        self.disk
            .file
            .read_exact(&mut byte)
            .map_err(Error::temp_storage)?;
        Ok(byte[0])
    }
}
impl Fragments {
    fn new(r: &ResourceConfig, c: &ResourceContext) -> Result<Self> {
        Ok(Self {
            disk: DiskFile::new("srep-legacy-fragments-", r, c)?,
            count: 0,
        })
    }
    fn push(&mut self, fragment: Fragment) -> Result<()> {
        let mut b = [0u8; FRAGMENT_RECORD_SIZE as usize];
        for (i, v) in [
            fragment.dst,
            fragment.src,
            fragment.len,
            fragment.source_block,
            fragment.ordinal,
        ]
        .into_iter()
        .enumerate()
        {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes())
        }
        self.disk.append(&b)?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| Error::temp_limit("fragment count overflows"))?;
        Ok(())
    }
    fn get(&mut self, index: u64) -> Result<Fragment> {
        read_fragment(&mut self.disk.file, index)
    }

    fn sort_by_destination(
        &mut self,
        resources: &ResourceConfig,
        context: &ResourceContext,
    ) -> Result<()> {
        const RUN_SIZE: usize = RUN_RECORDS as usize;
        // ceil(u64::MAX / 2^8) is 2^56, so binary carry needs levels 0..=56.
        // This fixed stack state is mathematically complete for the u64 format;
        // only the individual run files consume the temporary budget.
        let mut levels: [Option<MergeRun>; MAX_MERGE_LEVELS] = std::array::from_fn(|_| None);
        let mut first = 0u64;
        while first < self.count {
            let amount = (self.count - first).min(RUN_RECORDS) as usize;
            let mut records = [Fragment {
                src: 0,
                dst: 0,
                len: 0,
                source_block: 0,
                ordinal: 0,
            }; RUN_SIZE];
            for (offset, record) in records[..amount].iter_mut().enumerate() {
                *record = self.get(first + offset as u64)?;
            }
            records[..amount].sort_unstable_by_key(|record| (record.dst, record.ordinal));
            let mut run = DiskFile::new("srep-legacy-fragment-run-", resources, context)?;
            for record in &records[..amount] {
                append_fragment(&mut run, *record)?;
            }
            let carry = run.into_merge_run(amount as u64);
            insert_merge_run(&mut levels, carry, |existing, incoming| {
                merge_runs(existing, incoming, resources, context)
            })?;
            first += amount as u64;
        }

        self.disk.file.set_len(0).map_err(Error::temp_storage)?;
        self.disk.reservation.shrink(self.disk.len);
        self.disk.len = 0;
        let mut sorted: Option<MergeRun> = None;
        for run in levels.into_iter().flatten() {
            sorted = Some(match sorted {
                None => run,
                Some(existing) => merge_runs(existing, run, resources, context)?,
            });
        }
        if let Some(sorted_run) = sorted {
            let mut sorted_disk = sorted_run.open()?;
            let count = sorted_run.count;
            let mut cursor = 0;
            while cursor < count {
                append_fragment(&mut self.disk, read_fragment(&mut sorted_disk, cursor)?)?;
                cursor += 1;
            }
        }
        Ok(())
    }

    fn next(&mut self, cursor: &mut u64) -> Result<Option<Fragment>> {
        if *cursor >= self.count {
            return Ok(None);
        }
        let fragment = self.get(*cursor)?;
        *cursor += 1;
        Ok(Some(fragment))
    }

    fn covers(
        &mut self,
        position: u64,
        cursor: &mut Option<Fragment>,
        index: &mut u64,
    ) -> Result<bool> {
        while cursor.is_none_or(|f| f.dst.checked_add(f.len).is_none_or(|end| end <= position)) {
            *cursor = self.next(index)?;
            if cursor.is_none() {
                return Ok(false);
            }
        }
        Ok(cursor.is_some_and(|f| {
            f.dst
                .checked_add(f.len)
                .is_some_and(|end| position >= f.dst && position < end)
        }))
    }
}

fn insert_merge_run<T, F>(
    levels: &mut [Option<T>; MAX_MERGE_LEVELS],
    incoming: T,
    mut merge: F,
) -> Result<()>
where
    F: FnMut(T, T) -> Result<T>,
{
    let mut carry = Some(incoming);
    let mut level = 0;
    while let Some(value) = carry.take() {
        if level == levels.len() {
            return Err(Error::corrupt_record(
                "fragment merge level arithmetic overflow",
            ));
        }
        if let Some(existing) = levels[level].take() {
            carry = Some(merge(existing, value)?);
            level += 1;
        } else {
            levels[level] = Some(value);
        }
    }
    Ok(())
}

fn append_fragment(disk: &mut DiskFile, fragment: Fragment) -> Result<()> {
    let mut b = [0u8; FRAGMENT_RECORD_SIZE as usize];
    for (i, value) in [
        fragment.dst,
        fragment.src,
        fragment.len,
        fragment.source_block,
        fragment.ordinal,
    ]
    .into_iter()
    .enumerate()
    {
        b[i * 8..i * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    disk.append(&b).map(|_| ())
}

fn read_fragment(disk: &mut File, index: u64) -> Result<Fragment> {
    let offset = index
        .checked_mul(FRAGMENT_RECORD_SIZE)
        .ok_or_else(|| Error::temp_limit("fragment offset overflows"))?;
    disk.seek(SeekFrom::Start(offset))
        .map_err(Error::temp_storage)?;
    let mut b = [0u8; FRAGMENT_RECORD_SIZE as usize];
    disk.read_exact(&mut b).map_err(Error::temp_storage)?;
    Ok(Fragment {
        dst: u64::from_le_bytes(b[0..8].try_into().unwrap()),
        src: u64::from_le_bytes(b[8..16].try_into().unwrap()),
        len: u64::from_le_bytes(b[16..24].try_into().unwrap()),
        source_block: u64::from_le_bytes(b[24..32].try_into().unwrap()),
        ordinal: u64::from_le_bytes(b[32..40].try_into().unwrap()),
    })
}

fn merge_runs(
    left: MergeRun,
    right: MergeRun,
    resources: &ResourceConfig,
    context: &ResourceContext,
) -> Result<MergeRun> {
    let left_count = left.count;
    let right_count = right.count;
    let mut left_file = left.open()?;
    let mut right_file = right.open()?;
    let mut output = DiskFile::new("srep-legacy-fragment-merge-", resources, context)?;
    let mut left_cursor = 0;
    let mut right_cursor = 0;
    let mut left_record = if left_count == 0 {
        None
    } else {
        Some(read_fragment(&mut left_file, 0)?)
    };
    let mut right_record = if right_count == 0 {
        None
    } else {
        Some(read_fragment(&mut right_file, 0)?)
    };
    while left_record.is_some() || right_record.is_some() {
        let take_left = match (left_record, right_record) {
            (Some(left_value), Some(right_value)) => {
                (left_value.dst, left_value.ordinal) <= (right_value.dst, right_value.ordinal)
            }
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => false,
        };
        if take_left {
            let record = left_record.take().unwrap();
            append_fragment(&mut output, record)?;
            left_cursor += 1;
            left_record = if left_cursor < left_count {
                Some(read_fragment(&mut left_file, left_cursor)?)
            } else {
                None
            };
        } else {
            let record = right_record.take().unwrap();
            append_fragment(&mut output, record)?;
            right_cursor += 1;
            right_record = if right_cursor < right_count {
                Some(read_fragment(&mut right_file, right_cursor)?)
            } else {
                None
            };
        }
    }
    drop(left_file);
    drop(right_file);
    Ok(output.into_merge_run(left_count + right_count))
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b.try_into().unwrap())
}

fn read_frame(
    file: &mut File,
    h: &Header,
    pos: u64,
    limit: u64,
    index: bool,
) -> Result<(Block, u64)> {
    let minimum = 12u64
        .checked_add(
            u64::try_from(h.checksum.width())
                .map_err(|_| Error::corrupt_index("legacy checksum width overflows"))?,
        )
        .ok_or_else(|| Error::corrupt_index("legacy frame minimum overflows"))?;
    if pos > limit || limit - pos < minimum {
        return Err(if index {
            Error::truncated("truncated legacy v4 body")
        } else {
            Error::truncated("truncated legacy block header")
        });
    }
    file.seek(SeekFrom::Start(pos))
        .map_err(Error::temp_storage)?;
    let mut b = [0u8; 12];
    file.read_exact(&mut b)
        .map_err(|e| Error::map_eof(e, "truncated legacy block header"))?;
    let lit = u64::from(u32le(&b[..4]));
    let len = u64::from(u32le(&b[4..8]));
    let stat = u64::from(u32le(&b[8..]));
    if len == 0 {
        return Err(if index {
            Error::corrupt_index("legacy v4 block length is zero")
        } else {
            Error::corrupt_record("legacy block length is zero")
        });
    }
    if index && stat != 0 {
        return Err(Error::corrupt_index(
            "legacy v4 body contains inline statistics",
        ));
    }
    if !index && stat % 16 != 0 {
        return Err(Error::corrupt_record(
            "legacy v3 inline statistics are not record-aligned",
        ));
    }
    let payload = u64::try_from(h.checksum.width())
        .unwrap()
        .checked_add(stat)
        .and_then(|x| x.checked_add(lit))
        .ok_or_else(|| Error::corrupt_record("legacy block payload overflows"))?;
    let end = pos
        .checked_add(12)
        .and_then(|x| x.checked_add(payload))
        .ok_or_else(|| Error::corrupt_record("legacy block range overflows"))?;
    if end > limit {
        return Err(if index {
            Error::corrupt_index("legacy v4 body is truncated")
        } else {
            Error::truncated("truncated legacy block payload")
        });
    }
    let digest_pos = pos
        .checked_add(12)
        .ok_or_else(|| Error::corrupt_record("legacy digest offset overflows"))?;
    let stats_pos = digest_pos
        .checked_add(
            u64::try_from(h.checksum.width())
                .map_err(|_| Error::corrupt_record("legacy checksum width overflows"))?,
        )
        .ok_or_else(|| Error::corrupt_record("legacy statistics offset overflows"))?;
    let literal_pos = stats_pos
        .checked_add(stat)
        .ok_or_else(|| Error::corrupt_record("legacy literal offset overflows"))?;
    Ok((
        Block {
            start: 0,
            len,
            digest_pos,
            stats_pos,
            stats_len: stat,
            literal_pos,
            literal_len: lit,
        },
        end,
    ))
}

fn read_footer(
    file: &mut File,
    h: &Header,
    r: &ResourceConfig,
    c: &ResourceContext,
) -> Result<(u64, Sizes)> {
    const FOOTER_LEN: u64 = 24;
    let len = file.seek(SeekFrom::End(0)).map_err(Error::temp_storage)?;
    if len < h.header_end + FOOTER_LEN {
        return Err(Error::truncated("truncated legacy v4 footer"));
    }
    let start = len - FOOTER_LEN;
    file.seek(SeekFrom::Start(start))
        .map_err(Error::temp_storage)?;
    let mut f = [0u8; 24];
    file.read_exact(&mut f)
        .map_err(|e| Error::map_eof(e, "truncated legacy v4 footer"))?;
    let fixed =
        u32le(&f[12..16]) == 1 && u32le(&f[16..20]) == 0xafbaadac && u32le(&f[20..]) == 0xd9cae7e8;
    if !fixed {
        return Err(Error::corrupt_index("legacy v4 footer markers are invalid"));
    }
    let total = u64::from(u32le(&f[..4])) | (u64::from(u32le(&f[4..8])) << 32);
    let fs = u64::from(u32le(&f[8..12]));
    let Some(size_bytes) = fs.checked_sub(FOOTER_LEN).filter(|n| *n % 4 == 0) else {
        return Err(Error::corrupt_index("legacy v4 footer size is invalid"));
    };
    let Some(ss) = len.checked_sub(fs) else {
        return Err(Error::corrupt_index("legacy v4 footer range is invalid"));
    };
    let Some(ix) = ss.checked_sub(total) else {
        return Err(Error::corrupt_index("legacy v4 ranges overlap"));
    };
    if ix < h.header_end {
        return Err(Error::corrupt_index("legacy v4 ranges are invalid"));
    }
    let n = size_bytes / 4;
    let sizes = read_sizes_checked(file, ss, n, ix, total, r, c)?;
    let body_end = scan_v4_body(file, h, ix, n)?;
    if body_end != ix {
        return Err(Error::corrupt_index("legacy v4 body ranges are invalid"));
    }
    Ok(sizes)
}
fn scan_v4_body(file: &mut File, h: &Header, end: u64, n: u64) -> Result<u64> {
    let mut p = h.header_end;
    for _ in 0..n {
        let (_, q) = read_frame(file, h, p, end, true)?;
        p = q;
    }
    Ok(p)
}
fn read_sizes_checked(
    file: &mut File,
    start: u64,
    n: u64,
    ix: u64,
    expected: u64,
    r: &ResourceConfig,
    c: &ResourceContext,
) -> Result<(u64, Sizes)> {
    let mut z = Sizes::new(r, c)?;
    file.seek(SeekFrom::Start(start))
        .map_err(Error::temp_storage)?;
    let mut sum = 0u64;
    for _ in 0..n {
        let mut b = [0u8; 4];
        file.read_exact(&mut b)
            .map_err(|e| Error::map_eof(e, "truncated legacy v4 size array"))?;
        let v = u64::from(u32le(&b));
        if v % 16 != 0 {
            return Err(Error::corrupt_index("legacy v4 stat size is not aligned"));
        }
        sum = sum
            .checked_add(v)
            .ok_or_else(|| Error::corrupt_index("legacy v4 stat total overflows"))?;
        z.push(v)?
    }
    if sum != expected {
        return Err(Error::corrupt_index("legacy v4 stat total mismatch"));
    }
    Ok((ix, z))
}

pub fn decode(
    file: &mut File,
    h: &Header,
    r: &ResourceConfig,
    c: &ResourceContext,
) -> Result<Decoded> {
    let index = h.layout == Layout::Index;
    let (body_end, mut sizes) = if index {
        let (x, s) = read_footer(file, h, r, c)?;
        (x, Some(s))
    } else {
        (
            file.seek(SeekFrom::End(0)).map_err(Error::temp_storage)?,
            None,
        )
    };
    let mut blocks = Blocks::new(r, c)?;
    let mut p = h.header_end;
    let mut logical = 0;
    while p < body_end {
        if !index {
            match legacy_block_boundary(file, p, body_end)? {
                LegacyBoundary::End => break,
                LegacyBoundary::Block => {}
            }
        }
        let (mut b, next) = read_frame(file, h, p, body_end, index)?;
        b.start = logical;
        logical = logical
            .checked_add(b.len)
            .ok_or_else(|| Error::output_limit("legacy output length overflows"))?;
        if logical > r.output_limit {
            return Err(Error::output_limit("legacy output exceeds output limit"));
        }
        blocks.push(b)?;
        p = next
    }
    if index {
        let size_count = sizes
            .as_ref()
            .map(|value| value.count)
            .ok_or_else(|| Error::corrupt_index("legacy v4 size table is missing"))?;
        if blocks.count != size_count {
            return Err(Error::corrupt_index("legacy v4 block count mismatch"));
        }
    };
    let mut fragments = Fragments::new(r, c)?;
    let mut covered: u64 = 0;
    let mut stats_cursor = body_end;
    for i in 0..blocks.count {
        let b = blocks.get(i)?;
        let (group_pos, group_len) = if let Some(ref mut s) = sizes {
            let n = s.get(i)?;
            let position = stats_cursor;
            stats_cursor = stats_cursor
                .checked_add(n)
                .ok_or_else(|| Error::corrupt_index("legacy v4 statistics range overflows"))?;
            (position, n)
        } else {
            (b.stats_pos, b.stats_len)
        };
        file.seek(SeekFrom::Start(group_pos))
            .map_err(Error::temp_storage)?;
        let mut left = group_len;
        let mut cur = b.start;
        while left > 0 {
            let mut x = [0u8; 16];
            file.read_exact(&mut x)
                .map_err(|e| Error::map_eof(e, "truncated legacy Future statistics"))?;
            let src = cur
                .checked_add(u64::from(u32le(&x[..4])))
                .ok_or_else(|| Error::invalid_match("future source overflows"))?;
            let d = u64::from(u32le(&x[4..8])) | (u64::from(u32le(&x[8..12])) << 32);
            let len = u64::from(u32le(&x[12..]))
                .checked_add(u64::from(h.base_len))
                .ok_or_else(|| Error::invalid_match("future length overflows"))?;
            let dst = src
                .checked_add(d)
                .ok_or_else(|| Error::invalid_match("future destination overflows"))?;
            let end = b
                .start
                .checked_add(b.len)
                .ok_or_else(|| Error::invalid_match("future source block overflows"))?;
            if src < b.start
                || src >= end
                || d == 0
                || len == 0
                || len > end - src
                || dst.checked_add(len).filter(|x| *x <= logical).is_none()
            {
                return Err(Error::invalid_match("future physical fragment is invalid"));
            }
            fragments.push(Fragment {
                src,
                dst,
                len,
                source_block: i,
                ordinal: fragments.count,
            })?;
            covered = covered
                .checked_add(len)
                .ok_or_else(|| Error::corrupt_record("legacy coverage overflows"))?;
            cur = src;
            left -= 16
        }
    }
    if let Some(ref s) = sizes {
        let file_len = file.seek(SeekFrom::End(0)).map_err(Error::temp_storage)?;
        let size_array_bytes = s
            .count
            .checked_mul(4)
            .ok_or_else(|| Error::corrupt_index("legacy v4 size array overflows"))?;
        let expected_stats_end = file_len
            .checked_sub(24)
            .and_then(|value| value.checked_sub(size_array_bytes))
            .ok_or_else(|| Error::corrupt_index("legacy v4 statistics range underflows"))?;
        if stats_cursor != expected_stats_end {
            return Err(Error::corrupt_index(
                "legacy v4 statistics range is incomplete",
            ));
        }
    }
    fragments.sort_by_destination(r, c)?;
    let mut prev_end = 0;
    let mut cursor = 0;
    while let Some(f) = fragments.next(&mut cursor)? {
        if f.dst < prev_end {
            return Err(Error::invalid_match("overlapping Future-LZ destinations"));
        }
        prev_end = f
            .dst
            .checked_add(f.len)
            .ok_or_else(|| Error::invalid_match("future destination overflows"))?
    }
    let mut out = OutputStore::new(r, c)?;
    let mut periods = Periods::new(r, c)?;
    let mut literal_total: u64 = 0;
    let mut coverage_cursor = None;
    let mut coverage_index = 0;
    for i in 0..blocks.count {
        let b = blocks.get(i)?;
        file.seek(SeekFrom::Start(b.literal_pos))
            .map_err(Error::temp_storage)?;
        let mut cursor = b.start;
        let end = b
            .start
            .checked_add(b.len)
            .ok_or_else(|| Error::corrupt_record("legacy block end overflows"))?;
        let mut lit = 0;
        while cursor < end {
            if !fragments.covers(cursor, &mut coverage_cursor, &mut coverage_index)? {
                let mut x = [0u8; 1];
                file.read_exact(&mut x)
                    .map_err(|e| Error::map_eof(e, "truncated legacy literals"))?;
                out.write_at(cursor, &x)?;
                lit += 1
            }
            cursor += 1
        }
        if lit != b.literal_len {
            return Err(if index {
                Error::corrupt_index("legacy v4 literal coverage mismatch")
            } else {
                Error::corrupt_record("legacy literal coverage mismatch")
            });
        }
        literal_total = literal_total
            .checked_add(lit)
            .ok_or_else(|| Error::corrupt_record("legacy literal total overflows"))?;
        let _ = i;
    }
    let mut delivery_cursor = 0;
    while let Some(f) = fragments.next(&mut delivery_cursor)? {
        let period = f.len.min(f.dst - f.src);
        let period_len =
            usize::try_from(period).map_err(|_| Error::invalid_match("future period overflows"))?;
        if period_len == 0 {
            return Err(Error::invalid_match("future period is zero"));
        }
        let period_pos = periods.append_from_output(&mut out, f.src, period)?;
        for i in 0..f.len {
            let byte = periods.read_byte(
                period_pos
                    .checked_add(i % period)
                    .ok_or_else(|| Error::invalid_match("future period position overflows"))?,
            )?;
            out.write_at(
                f.dst
                    .checked_add(i)
                    .ok_or_else(|| Error::invalid_match("future destination overflows"))?,
                &[byte],
            )?
        }
    }
    let mut digest = [0u8; 64];
    for i in 0..blocks.count {
        let b = blocks.get(i)?;
        file.seek(SeekFrom::Start(b.digest_pos))
            .map_err(Error::temp_storage)?;
        file.read_exact(&mut digest[..h.checksum.width()])
            .map_err(|e| Error::map_eof(e, "truncated legacy block digest"))?;
        checksum::verify_reader(
            h.checksum,
            &h.seed,
            &mut out,
            b.start,
            b.len,
            &digest[..h.checksum.width()],
        )?;
        let _ = i;
    }
    Ok(Decoded {
        output: out,
        original_size: logical,
        block_count: blocks.count,
        match_count: fragments.count,
        covered_bytes: covered,
        literal_bytes: literal_total,
    })
}

#[cfg(test)]
mod merge_tests {
    use super::insert_merge_run;

    #[test]
    fn merge_levels_grow_past_the_former_sixteen_level_boundary() {
        let mut levels: [Option<u64>; super::MAX_MERGE_LEVELS] = std::array::from_fn(|_| None);
        for value in 0..(1u64 << 17) {
            insert_merge_run(&mut levels, value, |left, right| Ok(left + right)).unwrap();
        }
        assert!(levels.iter().skip(16).any(Option::is_some));
        assert_eq!(levels.iter().filter(|value| value.is_some()).count(), 1);
    }

    #[test]
    fn merge_level_bound_is_complete_for_u64_fragment_counts() {
        assert_eq!(super::initial_runs_for_fragments(1 << 16), 1 << 8);
        assert_eq!(super::merge_levels_for_initial_runs(1 << 55), 56);
        assert_eq!(super::merge_levels_for_initial_runs(1 << 56), 57);
        assert_eq!(
            super::merge_levels_for_initial_runs(super::initial_runs_for_fragments(u64::MAX)),
            super::MAX_MERGE_LEVELS
        );
    }
}
