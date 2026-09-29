use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tempfile::{Builder, TempPath};

use crate::checksum::oneshot;
use crate::config::ResourceConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::resource::{BudgetedVec, MemoryBudget, Reservation, ResourceContext};

use crate::offload::contracts::{
    CodecSourceIndex, CodecSourceIndexFactory, SourceVerifier, VerifiedSourcePages,
};
use crate::offload::session::{CompressionSession, SourceLoan};
use crate::offload::storage::{PagedSourceTable, SourcePageBuilder, SourcePageCache};

pub const MAX_KEY_BYTES: usize = 32;
pub const MAX_METADATA_BYTES: usize = 32;
pub const INDEX_RECORD_LEN: usize = 104;
pub const INDEX_HEADER_LEN: usize = 64;

const MAX_RUN_FAN_IN: usize = 16;
const RECORD_DOMAIN: &[u8] = b"SREP-IDX-REC\0";
const HEADER_DOMAIN: &[u8] = b"SREP-IDX-HDR\0";
const SCRATCH_HEADER_DOMAIN: &[u8] = b"SREP-QRY-HDR\0";

type EntryProducer<'a> = dyn FnMut(&mut dyn FnMut(IndexEntry) -> Result<()>) -> Result<()> + 'a;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub key_kind: u8,
    pub key_bytes: [u8; MAX_KEY_BYTES],
    pub key_len: u8,
    pub position: u64,
    pub insertion_ordinal: u64,
    pub metadata: [u8; MAX_METADATA_BYTES],
    pub metadata_len: u8,
}

impl IndexEntry {
    pub fn new(
        key_kind: u8,
        key_bytes: &[u8],
        position: u64,
        insertion_ordinal: u64,
        metadata: &[u8],
    ) -> Result<Self> {
        if key_bytes.len() > MAX_KEY_BYTES {
            return Err(Error::invalid_config(
                "candidate index key exceeds 32 bytes",
            ));
        }
        if metadata.len() > MAX_METADATA_BYTES {
            return Err(Error::invalid_config(
                "candidate index metadata exceeds 32 bytes",
            ));
        }
        let mut entry = Self {
            key_kind,
            key_bytes: [0; MAX_KEY_BYTES],
            key_len: u8::try_from(key_bytes.len())
                .map_err(|_| Error::invalid_config("candidate index key length overflows"))?,
            position,
            insertion_ordinal,
            metadata: [0; MAX_METADATA_BYTES],
            metadata_len: u8::try_from(metadata.len())
                .map_err(|_| Error::invalid_config("candidate index metadata length overflows"))?,
        };
        entry.key_bytes[..key_bytes.len()].copy_from_slice(key_bytes);
        entry.metadata[..metadata.len()].copy_from_slice(metadata);
        entry.validate_config()?;
        Ok(entry)
    }

    pub fn key_slice(&self) -> &[u8] {
        &self.key_bytes[..self.key_len as usize]
    }

    pub fn metadata_slice(&self) -> &[u8] {
        &self.metadata[..self.metadata_len as usize]
    }

    fn validate(&self) -> Result<()> {
        let key_len = usize::from(self.key_len);
        let metadata_len = usize::from(self.metadata_len);
        if key_len > MAX_KEY_BYTES || metadata_len > MAX_METADATA_BYTES {
            return Err(Error::corrupt_record(
                "candidate index field length is invalid",
            ));
        }
        if self.key_bytes[key_len..].iter().any(|&byte| byte != 0)
            || self.metadata[metadata_len..].iter().any(|&byte| byte != 0)
        {
            return Err(Error::corrupt_record(
                "candidate index fixed fields have nonzero padding",
            ));
        }
        if (key_len, metadata_len) != expected_shape(self.key_kind)? {
            return Err(Error::corrupt_record(
                "candidate index key or metadata shape is invalid",
            ));
        }
        Ok(())
    }

    fn validate_config(&self) -> Result<()> {
        let key_len = usize::from(self.key_len);
        let metadata_len = usize::from(self.metadata_len);
        if key_len > MAX_KEY_BYTES || metadata_len > MAX_METADATA_BYTES {
            return Err(Error::invalid_config(
                "candidate index field length is invalid",
            ));
        }
        if (key_len, metadata_len) != expected_shape(self.key_kind)? {
            return Err(Error::invalid_config(
                "candidate index key or metadata shape is invalid",
            ));
        }
        Ok(())
    }

    pub fn encode_record(&self) -> Result<[u8; INDEX_RECORD_LEN]> {
        self.validate()?;
        let mut bytes = [0u8; INDEX_RECORD_LEN];
        bytes[0..2].copy_from_slice(&1u16.to_le_bytes());
        bytes[2] = self.key_kind;
        bytes[3] = self.key_len;
        bytes[4] = self.metadata_len;
        bytes[8..16].copy_from_slice(&self.position.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.insertion_ordinal.to_le_bytes());
        bytes[24..56].copy_from_slice(&self.key_bytes);
        bytes[56..88].copy_from_slice(&self.metadata);
        let checksum = record_checksum(&bytes[..88]);
        bytes[88..].copy_from_slice(&checksum);
        Ok(bytes)
    }

    pub fn decode_record(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != INDEX_RECORD_LEN {
            return Err(Error::corrupt_record(
                "candidate index record length is invalid",
            ));
        }
        if bytes[0..2] != 1u16.to_le_bytes() || bytes[5] != 0 || bytes[6..8] != [0; 2] {
            return Err(Error::corrupt_record(
                "candidate index record reserved fields are invalid",
            ));
        }
        if bytes[88..] != record_checksum(&bytes[..88]) {
            return Err(Error::corrupt_record(
                "candidate index record checksum mismatch",
            ));
        }
        let mut key_bytes = [0u8; MAX_KEY_BYTES];
        key_bytes.copy_from_slice(&bytes[24..56]);
        let mut metadata = [0u8; MAX_METADATA_BYTES];
        metadata.copy_from_slice(&bytes[56..88]);
        let entry = Self {
            key_kind: bytes[2],
            key_bytes,
            key_len: bytes[3],
            position: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            insertion_ordinal: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            metadata,
            metadata_len: bytes[4],
        };
        entry.validate()?;
        Ok(entry)
    }
}

fn expected_shape(kind: u8) -> Result<(usize, usize)> {
    match kind {
        0 => Ok((8, 0)),
        1..=3 => Ok((24, 0)),
        4 => Ok((16, 0)),
        5 => Ok((16, 4)),
        _ => Err(Error::invalid_config("unknown candidate index key kind")),
    }
}

fn record_checksum(first: &[u8]) -> [u8; 16] {
    let mut input = [0u8; RECORD_DOMAIN.len() + 88];
    input[..RECORD_DOMAIN.len()].copy_from_slice(RECORD_DOMAIN);
    input[RECORD_DOMAIN.len()..].copy_from_slice(first);
    oneshot(crate::config::Checksum::Xxh3, &input)
        .try_into()
        .expect("XXH3 has a fixed 128-bit width")
}

fn fixed_checksum(domain: &[u8], first: &[u8]) -> [u8; 16] {
    let mut input = [0u8; 61];
    let length = domain.len() + first.len();
    input[..domain.len()].copy_from_slice(domain);
    input[domain.len()..length].copy_from_slice(first);
    oneshot(crate::config::Checksum::Xxh3, &input[..length])
        .try_into()
        .expect("XXH3 has a fixed 128-bit width")
}

fn header_checksum(first: &[u8]) -> [u8; 16] {
    fixed_checksum(HEADER_DOMAIN, first)
}

fn scratch_header_checksum(first: &[u8]) -> [u8; 16] {
    fixed_checksum(SCRATCH_HEADER_DOMAIN, first)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunHeader {
    pub count: u64,
    pub generation: u64,
    pub nonce: [u8; 16],
}

impl RunHeader {
    pub fn new(count: u64, generation: u64) -> Result<Self> {
        Ok(Self {
            count,
            generation,
            nonce: random_nonce()?,
        })
    }

    pub fn encode(&self) -> [u8; INDEX_HEADER_LEN] {
        let mut bytes = [0u8; INDEX_HEADER_LEN];
        bytes[0..8].copy_from_slice(b"SREPIDX1");
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        bytes[10..12].copy_from_slice(&(INDEX_RECORD_LEN as u16).to_le_bytes());
        bytes[16..24].copy_from_slice(&self.count.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.generation.to_le_bytes());
        bytes[32..48].copy_from_slice(&self.nonce);
        let checksum = header_checksum(&bytes[..48]);
        bytes[48..64].copy_from_slice(&checksum);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != INDEX_HEADER_LEN {
            return Err(Error::corrupt_index(
                "candidate index header length is invalid",
            ));
        }
        if &bytes[..8] != b"SREPIDX1"
            || bytes[8..10] != 1u16.to_le_bytes()
            || bytes[10..12] != (INDEX_RECORD_LEN as u16).to_le_bytes()
            || bytes[12..16] != [0; 4]
        {
            return Err(Error::corrupt_index(
                "candidate index header fields are invalid",
            ));
        }
        if bytes[48..] != header_checksum(&bytes[..48]) {
            return Err(Error::corrupt_index(
                "candidate index header checksum mismatch",
            ));
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&bytes[32..48]);
        Ok(Self {
            count: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            generation: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            nonce,
        })
    }
}

fn random_nonce() -> Result<[u8; 16]> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| Error::temp_storage_context(error.to_string()))?;
    Ok(nonce)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScratchHeader {
    pub order_id: u8,
    pub count: u64,
    pub generation: u64,
    pub nonce: [u8; 16],
}

impl ScratchHeader {
    pub fn new(order_id: u8, count: u64, generation: u64) -> Result<Self> {
        if !matches!(order_id, 1 | 2) {
            return Err(Error::invalid_config("candidate scratch order is invalid"));
        }
        Ok(Self {
            order_id,
            count,
            generation,
            nonce: random_nonce()?,
        })
    }

    pub fn encode(&self) -> [u8; INDEX_HEADER_LEN] {
        let mut bytes = [0u8; INDEX_HEADER_LEN];
        bytes[0..8].copy_from_slice(b"SREPQRY1");
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        bytes[10..12].copy_from_slice(&(INDEX_RECORD_LEN as u16).to_le_bytes());
        bytes[12] = self.order_id;
        bytes[16..24].copy_from_slice(&self.count.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.generation.to_le_bytes());
        bytes[32..48].copy_from_slice(&self.nonce);
        let checksum = scratch_header_checksum(&bytes[..48]);
        bytes[48..64].copy_from_slice(&checksum);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != INDEX_HEADER_LEN {
            return Err(Error::corrupt_index(
                "candidate scratch header length is invalid",
            ));
        }
        if &bytes[..8] != b"SREPQRY1"
            || bytes[8..10] != 1u16.to_le_bytes()
            || bytes[10..12] != (INDEX_RECORD_LEN as u16).to_le_bytes()
            || !matches!(bytes[12], 1 | 2)
            || bytes[13] != 0
            || bytes[14..16] != [0; 2]
        {
            return Err(Error::corrupt_index(
                "candidate scratch header fields are invalid",
            ));
        }
        if bytes[48..] != scratch_header_checksum(&bytes[..48]) {
            return Err(Error::corrupt_index(
                "candidate scratch header checksum mismatch",
            ));
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&bytes[32..48]);
        Ok(Self {
            order_id: bytes[12],
            count: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            generation: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            nonce,
        })
    }

    fn order(self) -> ScratchOrder {
        if self.order_id == 1 {
            ScratchOrder::Identity
        } else {
            ScratchOrder::PersistedQuery
        }
    }
}

pub trait CandidateIndex {
    fn insert(&mut self, entry: IndexEntry) -> Result<()>;

    fn for_each_candidate(
        &self,
        key_kind: u8,
        key_bytes: &[u8],
        before: u64,
        max_distance: u64,
        callback: &mut dyn FnMut(IndexEntry) -> Result<()>,
    ) -> Result<()>;

    fn memory_budget(&self) -> &MemoryBudget;

    fn candidates(
        &self,
        key_kind: u8,
        key_bytes: &[u8],
        before: u64,
        max_distance: u64,
    ) -> Result<BudgetedVec<IndexEntry>> {
        validate_query_shape(key_kind, key_bytes)?;
        let mut count = 0usize;
        self.for_each_candidate(key_kind, key_bytes, before, max_distance, &mut |_| {
            count = count
                .checked_add(1)
                .ok_or_else(|| Error::memory_limit("candidate result count overflows"))?;
            Ok(())
        })?;
        let mut result = BudgetedVec::with_capacity(count, self.memory_budget())?;
        self.for_each_candidate(key_kind, key_bytes, before, max_distance, &mut |entry| {
            result.push(entry)
        })?;
        Ok(result)
    }

    fn finish_epoch(&mut self) -> Result<()>;
}

#[derive(Debug)]
pub struct RamCandidateIndex {
    entries: BudgetedVec<IndexEntry>,
}

impl RamCandidateIndex {
    pub fn with_capacity(capacity: usize, budget: &MemoryBudget) -> Result<Self> {
        Ok(Self {
            entries: BudgetedVec::with_capacity(capacity, budget)?,
        })
    }

    pub fn new(budget: &MemoryBudget) -> Result<Self> {
        Ok(Self {
            entries: BudgetedVec::new(budget)?,
        })
    }

    pub fn entries(&self) -> &[IndexEntry] {
        self.entries.as_slice()
    }

    pub fn reserved_bytes(&self) -> u64 {
        self.entries.reserved_bytes()
    }
}

impl CandidateIndex for RamCandidateIndex {
    fn insert(&mut self, entry: IndexEntry) -> Result<()> {
        entry.validate()?;
        if let Some(existing_index) = self
            .entries
            .iter()
            .position(|existing| same_identity(existing, &entry))
        {
            if entry.insertion_ordinal < self.entries[existing_index].insertion_ordinal {
                let mut existing = self.entries.remove(existing_index);
                existing.insertion_ordinal = entry.insertion_ordinal;
                let index = self
                    .entries
                    .as_slice()
                    .binary_search_by(|current| index_entry_order(current, &existing))
                    .unwrap_or_else(|index| index);
                self.entries.insert(index, existing)?;
            }
            return Ok(());
        }
        let index = self
            .entries
            .as_slice()
            .binary_search_by(|current| index_entry_order(current, &entry))
            .unwrap_or_else(|index| index);
        self.entries.insert(index, entry)
    }

    fn for_each_candidate(
        &self,
        key_kind: u8,
        key_bytes: &[u8],
        before: u64,
        max_distance: u64,
        callback: &mut dyn FnMut(IndexEntry) -> Result<()>,
    ) -> Result<()> {
        validate_query_shape(key_kind, key_bytes)?;
        for &entry in self
            .entries
            .iter()
            .filter(|entry| matches_query(entry, key_kind, key_bytes, before, max_distance))
        {
            callback(entry)?;
        }
        Ok(())
    }

    fn memory_budget(&self) -> &MemoryBudget {
        self.entries.budget()
    }

    fn finish_epoch(&mut self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct Run {
    path: TempPath,
    _reservation: Reservation,
    header: RunHeader,
}

#[derive(Debug)]
struct ScratchRun {
    path: TempPath,
    _reservation: Reservation,
    header: ScratchHeader,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScratchOrder {
    Identity,
    PersistedQuery,
}

impl ScratchOrder {
    fn id(self) -> u8 {
        match self {
            Self::Identity => 1,
            Self::PersistedQuery => 2,
        }
    }
}

fn physical_len(count: usize) -> Result<u64> {
    let count =
        u64::try_from(count).map_err(|_| Error::temp_limit("candidate index count overflows"))?;
    (INDEX_HEADER_LEN as u64)
        .checked_add(
            count
                .checked_mul(INDEX_RECORD_LEN as u64)
                .ok_or_else(|| Error::temp_limit("candidate index length overflows"))?,
        )
        .ok_or_else(|| Error::temp_limit("candidate index length overflows"))
}

fn write_temp_file(
    prefix: &str,
    count: usize,
    temp_dir: &Path,
    temp: &crate::resource::TempBudget,
    producer: &mut EntryProducer<'_>,
    header: &[u8; INDEX_HEADER_LEN],
) -> Result<(TempPath, Reservation)> {
    let length = physical_len(count)?;
    let reservation = temp.reserve(length)?;
    fs::create_dir_all(temp_dir).map_err(Error::temp_storage)?;
    let mut file = Builder::new()
        .prefix(prefix)
        .tempfile_in(temp_dir)
        .map_err(Error::temp_storage)?;
    #[cfg(unix)]
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(Error::temp_storage)?;
    file.write_all(header).map_err(Error::temp_storage)?;
    let mut written = 0usize;
    let mut emit = |entry: IndexEntry| {
        file.write_all(&entry.encode_record()?)
            .map_err(Error::temp_storage)?;
        written = written
            .checked_add(1)
            .ok_or_else(|| Error::temp_limit("candidate index count overflows"))?;
        Ok(())
    };
    if let Err(error) = producer(&mut emit) {
        drop(reservation);
        return Err(error);
    }
    if written != count {
        drop(reservation);
        return Err(Error::corrupt_index(
            "candidate index header count mismatch",
        ));
    }
    file.flush().map_err(Error::temp_storage)?;
    file.as_file().sync_all().map_err(Error::temp_storage)?;
    if file
        .as_file()
        .metadata()
        .map_err(Error::temp_storage)?
        .len()
        != length
    {
        drop(reservation);
        return Err(Error::temp_storage_context(
            "candidate index physical length changed",
        ));
    }
    Ok((file.into_temp_path(), reservation))
}

impl Run {
    fn from_scratch(
        source: &ScratchRun,
        generation: u64,
        temp_dir: &Path,
        temp: &crate::resource::TempBudget,
    ) -> Result<Self> {
        source.validate()?;
        let header = RunHeader::new(source.header.count, generation)?;
        let mut producer =
            |emit: &mut dyn FnMut(IndexEntry) -> Result<()>| source.for_each_entry(emit);
        let count = usize::try_from(source.header.count)
            .map_err(|_| Error::temp_limit("candidate index count exceeds platform limits"))?;
        let (path, reservation) = write_temp_file(
            "srep-index-",
            count,
            temp_dir,
            temp,
            &mut producer,
            &header.encode(),
        )?;
        let run = Self {
            path,
            _reservation: reservation,
            header,
        };
        run.validate()?;
        Ok(run)
    }

    fn validate(&self) -> Result<()> {
        self.for_each_entry(&mut |_| Ok(()))
    }

    fn for_each_entry(&self, callback: &mut dyn FnMut(IndexEntry) -> Result<()>) -> Result<()> {
        let mut reader = VisibleReader::open(self)?;
        while let Some(entry) = reader.next()? {
            callback(entry)?;
        }
        Ok(())
    }

    fn collect_key_matches(
        &self,
        key_kind: u8,
        key_bytes: &[u8],
        before: u64,
        max_distance: u64,
        output: &mut BudgetedVec<IndexEntry>,
    ) -> Result<()> {
        // The binary search below is only an optimization.  Validate the complete
        // immutable run first so that neither the search nor its callback-visible
        // suffix can hide corruption in an unvisited record.
        self.validate()?;
        let mut file = File::open(&self.path).map_err(Error::temp_storage)?;
        let mut bytes = [0u8; INDEX_HEADER_LEN];
        file.read_exact(&mut bytes)
            .map_err(|_| Error::corrupt_index("candidate index header is truncated"))?;
        let header = RunHeader::decode(&bytes)?;
        if header != self.header {
            return Err(Error::corrupt_index("candidate index generation changed"));
        }
        validate_file_length(&file, header.count)?;
        let mut low = 0u64;
        let mut high = header.count;
        while low < high {
            let middle = low + (high - low) / 2;
            let entry = read_entry_at(&mut file, middle)?;
            if memtable_key_order(&entry, key_kind, key_bytes).is_lt() {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let mut previous = None;
        for index in low..header.count {
            let entry = read_entry_at(&mut file, index)?;
            if previous.is_some_and(|previous| index_entry_order(&previous, &entry).is_gt()) {
                return Err(Error::corrupt_index("candidate index records are unsorted"));
            }
            previous = Some(entry);
            if entry.key_kind != key_kind || entry.key_slice() != key_bytes {
                break;
            }
            if matches_query(&entry, key_kind, key_bytes, before, max_distance) {
                output.push(entry)?;
            }
        }
        Ok(())
    }
}

impl ScratchRun {
    fn create_entries(
        entries: &[IndexEntry],
        generation: u64,
        order: ScratchOrder,
        prefix: &str,
        temp_dir: &Path,
        temp: &crate::resource::TempBudget,
    ) -> Result<Self> {
        let header = ScratchHeader::new(order.id(), entries.len() as u64, generation)?;
        let mut producer = |emit: &mut dyn FnMut(IndexEntry) -> Result<()>| {
            for &entry in entries {
                emit(entry)?;
            }
            Ok(())
        };
        let (path, reservation) = write_temp_file(
            prefix,
            entries.len(),
            temp_dir,
            temp,
            &mut producer,
            &header.encode(),
        )?;
        let run = Self {
            path,
            _reservation: reservation,
            header,
        };
        run.validate()?;
        Ok(run)
    }

    fn validate(&self) -> Result<()> {
        self.for_each_entry(&mut |_| Ok(()))
    }

    fn for_each_entry(&self, callback: &mut dyn FnMut(IndexEntry) -> Result<()>) -> Result<()> {
        let mut reader = ScratchReader::open(self)?;
        while let Some(entry) = reader.next()? {
            callback(entry)?;
        }
        Ok(())
    }
}

struct VisibleReader {
    file: File,
    remaining: u64,
    previous: Option<IndexEntry>,
}

impl VisibleReader {
    fn open(run: &Run) -> Result<Self> {
        let mut file = File::open(&run.path).map_err(Error::temp_storage)?;
        let mut bytes = [0u8; INDEX_HEADER_LEN];
        file.read_exact(&mut bytes)
            .map_err(|_| Error::corrupt_index("candidate index header is truncated"))?;
        let header = RunHeader::decode(&bytes)?;
        if header != run.header {
            return Err(Error::corrupt_index("candidate index generation changed"));
        }
        validate_file_length(&file, header.count)?;
        Ok(Self {
            file,
            remaining: header.count,
            previous: None,
        })
    }

    fn next(&mut self) -> Result<Option<IndexEntry>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let entry = read_entry(&mut self.file, "candidate index record")?;
        if self
            .previous
            .is_some_and(|previous| index_entry_order(&previous, &entry).is_gt())
        {
            return Err(Error::corrupt_index("candidate index records are unsorted"));
        }
        self.previous = Some(entry);
        self.remaining -= 1;
        Ok(Some(entry))
    }
}

struct ScratchReader {
    file: File,
    remaining: u64,
    order: ScratchOrder,
    previous: Option<IndexEntry>,
}

impl ScratchReader {
    fn open(run: &ScratchRun) -> Result<Self> {
        let mut file = File::open(&run.path).map_err(Error::temp_storage)?;
        let mut bytes = [0u8; INDEX_HEADER_LEN];
        file.read_exact(&mut bytes)
            .map_err(|_| Error::corrupt_index("candidate scratch header is truncated"))?;
        let header = ScratchHeader::decode(&bytes)?;
        if header != run.header {
            return Err(Error::corrupt_index("candidate scratch generation changed"));
        }
        validate_file_length(&file, header.count)?;
        Ok(Self {
            file,
            remaining: header.count,
            order: header.order(),
            previous: None,
        })
    }

    fn next(&mut self) -> Result<Option<IndexEntry>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let entry = read_entry(&mut self.file, "candidate scratch record")?;
        if self
            .previous
            .is_some_and(|previous| scratch_order(&previous, &entry, self.order).is_gt())
        {
            return Err(Error::corrupt_index(
                "candidate scratch records are unsorted",
            ));
        }
        self.previous = Some(entry);
        self.remaining -= 1;
        Ok(Some(entry))
    }
}

fn read_entry(file: &mut File, what: &str) -> Result<IndexEntry> {
    let mut bytes = [0u8; INDEX_RECORD_LEN];
    file.read_exact(&mut bytes)
        .map_err(|_| Error::corrupt_index(format!("{what} is truncated")))?;
    IndexEntry::decode_record(&bytes).map_err(|error| Error::corrupt_index(error.to_string()))
}

fn record_offset(index: u64) -> Result<u64> {
    (INDEX_HEADER_LEN as u64)
        .checked_add(
            index
                .checked_mul(INDEX_RECORD_LEN as u64)
                .ok_or_else(|| Error::corrupt_index("candidate index offset overflows"))?,
        )
        .ok_or_else(|| Error::corrupt_index("candidate index offset overflows"))
}

fn read_entry_at(file: &mut File, index: u64) -> Result<IndexEntry> {
    file.seek(SeekFrom::Start(record_offset(index)?))
        .map_err(Error::temp_storage)?;
    read_entry(file, "candidate index record")
}

fn validate_file_length(file: &File, count: u64) -> Result<()> {
    let expected = (INDEX_HEADER_LEN as u64)
        .checked_add(
            count
                .checked_mul(INDEX_RECORD_LEN as u64)
                .ok_or_else(|| Error::corrupt_index("candidate index length overflows"))?,
        )
        .ok_or_else(|| Error::corrupt_index("candidate index length overflows"))?;
    if file.metadata().map_err(Error::temp_storage)?.len() != expected {
        return Err(Error::corrupt_index(
            "candidate index physical length is invalid",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct ScratchSet {
    slots: [Option<ScratchRun>; MAX_RUN_FAN_IN],
    len: usize,
}

impl ScratchSet {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, run: ScratchRun) -> Result<()> {
        if self.len == MAX_RUN_FAN_IN {
            return Err(Error::invalid_config("candidate scratch set is full"));
        }
        self.slots[self.len] = Some(run);
        self.len += 1;
        Ok(())
    }

    fn remove_first(&mut self) -> ScratchRun {
        let first = self.slots[0].take().expect("scratch set is nonempty");
        for index in 1..self.len {
            self.slots[index - 1] = self.slots[index].take();
        }
        self.len -= 1;
        first
    }

    fn insert_first(&mut self, run: ScratchRun) -> Result<()> {
        if self.len == MAX_RUN_FAN_IN {
            return Err(Error::invalid_config("candidate scratch set is full"));
        }
        for index in (0..self.len).rev() {
            self.slots[index + 1] = self.slots[index].take();
        }
        self.slots[0] = Some(run);
        self.len += 1;
        Ok(())
    }

    fn take_last(&mut self) -> Option<ScratchRun> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        self.slots[self.len].take()
    }

    fn iter(&self) -> ScratchSetIter<'_> {
        ScratchSetIter {
            slots: &self.slots,
            len: self.len,
            index: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct ScratchSetIter<'a> {
    slots: &'a [Option<ScratchRun>; MAX_RUN_FAN_IN],
    len: usize,
    index: usize,
}

impl<'a> Iterator for ScratchSetIter<'a> {
    type Item = &'a ScratchRun;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.len {
            let index = self.index;
            self.index += 1;
            if let Some(run) = self.slots[index].as_ref() {
                return Some(run);
            }
        }
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn append_scratch(
    set: &mut ScratchSet,
    run: ScratchRun,
    order: ScratchOrder,
    generation: &mut u64,
    temp_dir: &Path,
    temp: &crate::resource::TempBudget,
) -> Result<()> {
    if set.len() < MAX_RUN_FAN_IN {
        return set.push(run);
    }
    let mut sources: [Option<&ScratchRun>; MAX_RUN_FAN_IN] = [None; MAX_RUN_FAN_IN];
    for (index, old) in set.iter().take(MAX_RUN_FAN_IN - 1).enumerate() {
        sources[index] = Some(old);
    }
    sources[MAX_RUN_FAN_IN - 1] = Some(&run);
    let merged = merge_scratch(
        sources.iter().filter_map(Option::as_ref).copied(),
        order,
        generation,
        temp_dir,
        temp,
    )?;
    for _ in 0..(MAX_RUN_FAN_IN - 1) {
        set.remove_first();
    }
    set.insert_first(merged)
}

#[allow(clippy::too_many_arguments)]
fn push_chunk(
    chunk: &mut BudgetedVec<IndexEntry>,
    set: &mut ScratchSet,
    entry: IndexEntry,
    order: ScratchOrder,
    chunk_limit: usize,
    generation: &mut u64,
    temp_dir: &Path,
    temp: &crate::resource::TempBudget,
) -> Result<()> {
    if chunk.len() >= chunk_limit && !chunk.is_empty() {
        flush_chunk(chunk, set, order, chunk_limit, generation, temp_dir, temp)?;
    }
    match chunk.push(entry) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::MemoryBudgetExceeded => {
            if !chunk.is_empty() {
                flush_chunk(chunk, set, order, chunk_limit, generation, temp_dir, temp)?;
                if chunk.push(entry).is_ok() {
                    return Ok(());
                }
            }
            let run = ScratchRun::create_entries(
                std::slice::from_ref(&entry),
                *generation,
                order,
                "srep-qry-",
                temp_dir,
                temp,
            )?;
            *generation = generation
                .checked_add(1)
                .ok_or_else(|| Error::temp_limit("candidate scratch generation overflows"))?;
            append_scratch(set, run, order, generation, temp_dir, temp)
        }
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_chunk(
    chunk: &mut BudgetedVec<IndexEntry>,
    set: &mut ScratchSet,
    order: ScratchOrder,
    _chunk_limit: usize,
    generation: &mut u64,
    temp_dir: &Path,
    temp: &crate::resource::TempBudget,
) -> Result<()> {
    if chunk.is_empty() {
        return Ok(());
    }
    if order == ScratchOrder::Identity {
        canonicalize_identity(chunk);
    } else {
        chunk.sort_unstable_by(index_entry_order);
    }
    let run = ScratchRun::create_entries(
        chunk.as_slice(),
        *generation,
        order,
        "srep-qry-",
        temp_dir,
        temp,
    )?;
    *generation = generation
        .checked_add(1)
        .ok_or_else(|| Error::temp_limit("candidate scratch generation overflows"))?;
    append_scratch(set, run, order, generation, temp_dir, temp)?;
    *chunk = BudgetedVec::new(chunk.budget())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect_identity<'a, I>(
    source_runs: I,
    extra_slices: &[&[IndexEntry]],
    chunk_limit: usize,
    generation: &mut u64,
    temp_dir: &Path,
    temp: &crate::resource::TempBudget,
    memory: &MemoryBudget,
) -> Result<ScratchSet>
where
    I: IntoIterator<Item = &'a Run>,
{
    let mut chunk = BudgetedVec::new(memory)?;
    let mut set = ScratchSet::new();
    for run in source_runs {
        run.for_each_entry(&mut |entry| {
            push_chunk(
                &mut chunk,
                &mut set,
                entry,
                ScratchOrder::Identity,
                chunk_limit,
                generation,
                temp_dir,
                temp,
            )
        })?;
    }
    for entries in extra_slices {
        for &entry in *entries {
            push_chunk(
                &mut chunk,
                &mut set,
                entry,
                ScratchOrder::Identity,
                chunk_limit,
                generation,
                temp_dir,
                temp,
            )?;
        }
    }
    flush_chunk(
        &mut chunk,
        &mut set,
        ScratchOrder::Identity,
        chunk_limit,
        generation,
        temp_dir,
        temp,
    )?;
    Ok(set)
}

#[allow(clippy::too_many_arguments)]
fn collect_ordered_from_scratch(
    source: &ScratchRun,
    order: ScratchOrder,
    chunk_limit: usize,
    generation: &mut u64,
    temp_dir: &Path,
    temp: &crate::resource::TempBudget,
    memory: &MemoryBudget,
) -> Result<ScratchSet> {
    let mut chunk = BudgetedVec::new(memory)?;
    let mut set = ScratchSet::new();
    source.for_each_entry(&mut |entry| {
        push_chunk(
            &mut chunk,
            &mut set,
            entry,
            order,
            chunk_limit,
            generation,
            temp_dir,
            temp,
        )
    })?;
    flush_chunk(
        &mut chunk,
        &mut set,
        order,
        chunk_limit,
        generation,
        temp_dir,
        temp,
    )?;
    Ok(set)
}

fn collapse_scratch(
    set: &mut ScratchSet,
    order: ScratchOrder,
    generation: &mut u64,
    temp_dir: &Path,
    temp: &crate::resource::TempBudget,
) -> Result<ScratchRun> {
    while set.len() > 1 {
        let fan_in = set.len().min(MAX_RUN_FAN_IN);
        let merged = merge_scratch(set.iter().take(fan_in), order, generation, temp_dir, temp)?;
        for _ in 0..fan_in {
            set.remove_first();
        }
        set.insert_first(merged)?;
    }
    set.take_last()
        .ok_or_else(|| Error::corrupt_index("candidate scratch set is empty"))
}

fn merge_scratch<'a, I>(
    sources: I,
    order: ScratchOrder,
    generation: &mut u64,
    temp_dir: &Path,
    temp: &crate::resource::TempBudget,
) -> Result<ScratchRun>
where
    I: IntoIterator<Item = &'a ScratchRun> + Clone,
{
    let mut count = 0usize;
    let deduplicate = order == ScratchOrder::Identity;
    merge_entries(sources.clone(), order, deduplicate, &mut |entry| {
        count = count
            .checked_add(1)
            .ok_or_else(|| Error::temp_limit("candidate scratch count overflows"))?;
        let _ = entry;
        Ok(())
    })?;
    let header = ScratchHeader::new(order.id(), count as u64, *generation)?;
    let mut producer = |emit: &mut dyn FnMut(IndexEntry) -> Result<()>| {
        merge_entries(sources.clone(), order, deduplicate, emit)
    };
    let (path, reservation) = write_temp_file(
        "srep-qry-",
        count,
        temp_dir,
        temp,
        &mut producer,
        &header.encode(),
    )?;
    *generation = generation
        .checked_add(1)
        .ok_or_else(|| Error::temp_limit("candidate scratch generation overflows"))?;
    let run = ScratchRun {
        path,
        _reservation: reservation,
        header,
    };
    run.validate()?;
    Ok(run)
}

fn merge_entries<'a, I>(
    sources: I,
    order: ScratchOrder,
    deduplicate: bool,
    callback: &mut dyn FnMut(IndexEntry) -> Result<()>,
) -> Result<()>
where
    I: IntoIterator<Item = &'a ScratchRun>,
{
    let mut readers: [Option<ScratchReader>; MAX_RUN_FAN_IN] = std::array::from_fn(|_| None);
    let mut count = 0usize;
    for source in sources {
        if source.header.order() != order {
            return Err(Error::corrupt_index("candidate scratch order mismatch"));
        }
        if count == MAX_RUN_FAN_IN {
            return Err(Error::invalid_config(
                "candidate scratch merge fan-in is invalid",
            ));
        }
        readers[count] = Some(ScratchReader::open(source)?);
        count += 1;
    }
    let mut current: [Option<IndexEntry>; MAX_RUN_FAN_IN] = std::array::from_fn(|_| None);
    for index in 0..count {
        current[index] = readers[index].as_mut().unwrap().next()?;
    }
    let mut pending = None;
    loop {
        let mut selected = None;
        for index in 0..count {
            let Some(candidate) = current[index].as_ref() else {
                continue;
            };
            if selected.is_none_or(|selected: usize| {
                scratch_order(candidate, current[selected].as_ref().unwrap(), order).is_lt()
            }) {
                selected = Some(index);
            }
        }
        let Some(selected) = selected else {
            break;
        };
        let entry = current[selected].take().unwrap();
        current[selected] = readers[selected].as_mut().unwrap().next()?;
        if deduplicate {
            if let Some(mut winner) = pending {
                if same_identity(&winner, &entry) {
                    winner.insertion_ordinal =
                        winner.insertion_ordinal.min(entry.insertion_ordinal);
                    pending = Some(winner);
                } else {
                    callback(winner)?;
                    pending = Some(entry);
                }
            } else {
                pending = Some(entry);
            }
        } else {
            callback(entry)?;
        }
    }
    if let Some(winner) = pending {
        callback(winner)?;
    }
    Ok(())
}

fn canonicalize_identity(entries: &mut BudgetedVec<IndexEntry>) {
    entries.sort_unstable_by(|a, b| {
        identity(a)
            .cmp(&identity(b))
            .then_with(|| a.insertion_ordinal.cmp(&b.insertion_ordinal))
    });
    entries.dedup_by(|current, duplicate| {
        if same_identity(current, duplicate) {
            current.insertion_ordinal = current.insertion_ordinal.min(duplicate.insertion_ordinal);
            true
        } else {
            false
        }
    });
}

fn canonical_visible_from_sources<'a, I>(
    source_runs: I,
    extra_slices: &[&[IndexEntry]],
    chunk_limit: usize,
    generation: &mut u64,
    temp_dir: &Path,
    temp: &crate::resource::TempBudget,
    memory: &MemoryBudget,
) -> Result<Run>
where
    I: IntoIterator<Item = &'a Run>,
{
    let mut identity_runs = collect_identity(
        source_runs,
        extra_slices,
        chunk_limit,
        generation,
        temp_dir,
        temp,
        memory,
    )?;
    let identity = collapse_scratch(
        &mut identity_runs,
        ScratchOrder::Identity,
        generation,
        temp_dir,
        temp,
    )?;
    let mut persisted_runs = collect_ordered_from_scratch(
        &identity,
        ScratchOrder::PersistedQuery,
        chunk_limit,
        generation,
        temp_dir,
        temp,
        memory,
    )?;
    let persisted = collapse_scratch(
        &mut persisted_runs,
        ScratchOrder::PersistedQuery,
        generation,
        temp_dir,
        temp,
    )?;
    Run::from_scratch(&persisted, *generation, temp_dir, temp)
}

#[derive(Debug)]
struct RunSet {
    slots: [Option<Run>; MAX_RUN_FAN_IN],
    len: usize,
}

enum RunReplacement {
    Append(Run),
    Compact(Run),
}

impl RunSet {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, run: Run) {
        debug_assert!(self.len < MAX_RUN_FAN_IN);
        self.slots[self.len] = Some(run);
        self.len += 1;
    }

    fn iter(&self) -> impl Iterator<Item = &Run> {
        self.slots[..self.len].iter().map(|slot| {
            slot.as_ref()
                .expect("run set slots before len are always occupied")
        })
    }
}

impl IntoIterator for RunSet {
    type Item = Run;
    type IntoIter = std::iter::FilterMap<
        std::array::IntoIter<Option<Run>, MAX_RUN_FAN_IN>,
        fn(Option<Run>) -> Option<Run>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        fn occupied(slot: Option<Run>) -> Option<Run> {
            slot
        }
        self.slots.into_iter().filter_map(occupied)
    }
}

#[derive(Debug)]
pub struct HybridCandidateIndex {
    memtable: BudgetedVec<IndexEntry>,
    runs: RunSet,
    context: ResourceContext,
    temp_dir: PathBuf,
    memtable_limit: u64,
    next_generation: u64,
}

pub(crate) fn new_candidate_index(
    context: &ResourceContext,
    resources: &ResourceConfig,
) -> Result<HybridCandidateIndex> {
    HybridCandidateIndex::new(context, resources)
}

impl HybridCandidateIndex {
    pub fn new(context: &ResourceContext, _resources: &ResourceConfig) -> Result<Self> {
        let limit = context.candidate_index_memtable_bytes.unwrap_or_else(|| {
            (context.memory.limit() / 4).clamp(INDEX_RECORD_LEN as u64, 64 * 1024 * 1024)
        });
        Self::with_memtable_bytes_in(context, &context.temp_dir, limit)
    }

    pub fn with_memtable_bytes(context: &ResourceContext, limit: u64) -> Result<Self> {
        Self::with_memtable_bytes_in(context, &context.temp_dir, limit)
    }

    pub fn with_memtable_bytes_in(
        context: &ResourceContext,
        temp_dir: &Path,
        limit: u64,
    ) -> Result<Self> {
        Ok(Self {
            memtable: BudgetedVec::new(&context.memory)?,
            runs: RunSet::new(),
            context: context.clone(),
            temp_dir: temp_dir.to_path_buf(),
            memtable_limit: limit,
            next_generation: 0,
        })
    }

    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    pub fn run_paths(&self) -> Vec<PathBuf> {
        self.runs.iter().map(|run| run.path.to_path_buf()).collect()
    }

    pub fn memtable_entries(&self) -> &[IndexEntry] {
        self.memtable.as_slice()
    }

    pub fn memtable_capacity(&self) -> usize {
        self.memtable.capacity()
    }

    pub fn memtable_reserved_bytes(&self) -> u64 {
        self.memtable.reserved_bytes()
    }

    pub fn next_generation(&self) -> u64 {
        self.next_generation
    }

    fn chunk_limit(&self) -> usize {
        usize::try_from(self.memtable_limit)
            .ok()
            .map(|bytes| bytes / std::mem::size_of::<IndexEntry>())
            .unwrap_or(1)
            .max(1)
    }

    fn for_each_memtable_candidate(
        &self,
        key_kind: u8,
        key_bytes: &[u8],
        before: u64,
        max_distance: u64,
        callback: &mut dyn FnMut(IndexEntry) -> Result<()>,
    ) -> Result<()> {
        let mut low = 0usize;
        let mut high = self.memtable.len();
        while low < high {
            let middle = low + (high - low) / 2;
            if memtable_key_order(&self.memtable[middle], key_kind, key_bytes).is_lt() {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        for &entry in self.memtable.as_slice()[low..].iter() {
            if entry.key_kind != key_kind || entry.key_slice() != key_bytes {
                break;
            }
            if matches_query(&entry, key_kind, key_bytes, before, max_distance) {
                callback(entry)?;
            }
        }
        Ok(())
    }

    fn collect_direct_query_hits(
        &self,
        key_kind: u8,
        key_bytes: &[u8],
        before: u64,
        max_distance: u64,
    ) -> Result<BudgetedVec<IndexEntry>> {
        let mut hits = BudgetedVec::new(&self.context.memory)?;
        for run in self.runs.iter() {
            run.collect_key_matches(key_kind, key_bytes, before, max_distance, &mut hits)?;
        }
        collect_memtable_matches(
            self.memtable.as_slice(),
            key_kind,
            key_bytes,
            before,
            max_distance,
            &mut hits,
        )?;
        canonicalize_query_hits(&mut hits);
        Ok(hits)
    }

    fn for_each_candidate_from_scratch(
        &self,
        key_kind: u8,
        key_bytes: &[u8],
        before: u64,
        max_distance: u64,
        callback: &mut dyn FnMut(IndexEntry) -> Result<()>,
    ) -> Result<()> {
        for run in self.runs.iter() {
            run.validate()?;
        }
        let mut generation = self.next_generation;
        let mut identity_runs = collect_identity(
            self.runs.iter(),
            &[self.memtable.as_slice()],
            self.chunk_limit(),
            &mut generation,
            &self.temp_dir,
            &self.context.temp,
            &self.context.memory,
        )?;
        if identity_runs.is_empty() {
            return Ok(());
        }
        let identity = collapse_scratch(
            &mut identity_runs,
            ScratchOrder::Identity,
            &mut generation,
            &self.temp_dir,
            &self.context.temp,
        )?;
        let mut ordered_runs = collect_ordered_from_scratch(
            &identity,
            ScratchOrder::PersistedQuery,
            self.chunk_limit(),
            &mut generation,
            &self.temp_dir,
            &self.context.temp,
            &self.context.memory,
        )?;
        let ordered = collapse_scratch(
            &mut ordered_runs,
            ScratchOrder::PersistedQuery,
            &mut generation,
            &self.temp_dir,
            &self.context.temp,
        )?;
        ordered.validate()?;
        ordered.for_each_entry(&mut |entry| {
            if matches_query(&entry, key_kind, key_bytes, before, max_distance) {
                callback(entry)?;
            }
            Ok(())
        })
    }

    fn checkpoint(&mut self, incoming: Option<&IndexEntry>) -> Result<()> {
        let empty = BudgetedVec::new(&self.context.memory)?;
        let mut extra_slices = [self.memtable.as_slice(), &[]];
        let extra_count = if let Some(incoming) = incoming {
            extra_slices[1] = std::slice::from_ref(incoming);
            2
        } else {
            1
        };
        let mut generation = self.next_generation;
        let replacement = if self.runs.len() < MAX_RUN_FAN_IN {
            RunReplacement::Append(canonical_visible_from_sources(
                std::iter::empty::<&Run>(),
                &extra_slices[..extra_count],
                self.chunk_limit(),
                &mut generation,
                &self.temp_dir,
                &self.context.temp,
                &self.context.memory,
            )?)
        } else {
            let compacted = canonical_visible_from_sources(
                self.runs.iter().take(MAX_RUN_FAN_IN - 1),
                &extra_slices[..extra_count],
                self.chunk_limit(),
                &mut generation,
                &self.temp_dir,
                &self.context.temp,
                &self.context.memory,
            )?;
            RunReplacement::Compact(compacted)
        };

        match replacement {
            RunReplacement::Append(run) => {
                let old_runs = std::mem::replace(&mut self.runs, RunSet::new());
                let mut committed = RunSet::new();
                for old in old_runs {
                    committed.push(old);
                }
                committed.push(run);
                self.runs = committed;
            }
            RunReplacement::Compact(compacted) => {
                let old_runs = std::mem::replace(&mut self.runs, RunSet::new());
                let mut committed = RunSet::new();
                for (index, old) in old_runs.into_iter().enumerate() {
                    if index >= MAX_RUN_FAN_IN - 1 {
                        committed.push(old);
                    }
                }
                committed.push(compacted);
                self.runs = committed;
            }
        }
        self.memtable = empty;
        self.next_generation = generation;
        self.context.note_candidate_index_spill();
        Ok(())
    }
}

impl CandidateIndex for HybridCandidateIndex {
    fn insert(&mut self, entry: IndexEntry) -> Result<()> {
        entry.validate()?;
        if let Some(existing_index) = self
            .memtable
            .iter()
            .position(|existing| same_identity(existing, &entry))
        {
            if entry.insertion_ordinal >= self.memtable[existing_index].insertion_ordinal {
                return Ok(());
            }
            let existing = self.memtable.remove(existing_index);
            let insertion_index = self
                .memtable
                .as_slice()
                .binary_search_by(|current| index_entry_order(current, &entry))
                .unwrap_or_else(|index| index);
            if let Err(error) = self.memtable.insert_without_growth(insertion_index, entry) {
                let restore = self
                    .memtable
                    .insert_without_growth(existing_index, existing);
                debug_assert!(restore.is_ok());
                return Err(error);
            }
            return Ok(());
        }
        let next_bytes = self
            .memtable
            .len()
            .checked_add(1)
            .and_then(|count| count.checked_mul(std::mem::size_of::<IndexEntry>()));
        if next_bytes.is_none_or(|bytes| (bytes as u64) > self.memtable_limit) {
            return self.checkpoint(Some(&entry));
        }
        match self.memtable.insert_sorted(entry) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::MemoryBudgetExceeded => {
                self.checkpoint(Some(&entry))
            }
            Err(error) => Err(error),
        }
    }

    fn for_each_candidate(
        &self,
        key_kind: u8,
        key_bytes: &[u8],
        before: u64,
        max_distance: u64,
        callback: &mut dyn FnMut(IndexEntry) -> Result<()>,
    ) -> Result<()> {
        validate_query_shape(key_kind, key_bytes)?;
        if self.runs.is_empty() {
            return self.for_each_memtable_candidate(
                key_kind,
                key_bytes,
                before,
                max_distance,
                callback,
            );
        }
        match self.collect_direct_query_hits(key_kind, key_bytes, before, max_distance) {
            Ok(hits) => {
                for &entry in hits.as_slice() {
                    callback(entry)?;
                }
                Ok(())
            }
            Err(error) if error.kind() == ErrorKind::MemoryBudgetExceeded => self
                .for_each_candidate_from_scratch(
                    key_kind,
                    key_bytes,
                    before,
                    max_distance,
                    callback,
                ),
            Err(error) => Err(error),
        }
    }

    fn memory_budget(&self) -> &MemoryBudget {
        &self.context.memory
    }

    fn finish_epoch(&mut self) -> Result<()> {
        if self.memtable.is_empty() {
            return Ok(());
        }
        self.checkpoint(None)
    }
}

// The public HybridCandidateIndex above deliberately retains its historical
// (stronger) per-query validation contract.  The codec source index is a
// separate, private implementation.  In particular, it never calls Run's
// validate method from a query and it never materializes a query's hits.
const CODEC_MAX_RUNS: usize = MAX_RUN_FAN_IN;
const CODEC_MAX_SOURCES: usize = CODEC_MAX_RUNS + 2; // runs, memtable, incoming

pub(crate) struct CodecPagedIndex<'a> {
    session: &'a CompressionSession,
    memtable: SourceLoan<'a, IndexEntry>,
    runs: [Option<PagedSourceTable>; CODEC_MAX_RUNS],
    run_count: usize,
    cache: RefCell<SourcePageCache<'a>>,
    bloom: SourceLoan<'a, u8>,
    last_position: Option<u64>,
    last_ordinal: Option<u64>,
    poisoned: Cell<bool>,
    finished: bool,
    metrics: Cell<CodecIndexMetrics>,
    _metadata: Reservation,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CodecIndexMetrics {
    pub(crate) queries: u64,
    pub(crate) negative_queries: u64,
    pub(crate) comparisons: u64,
    pub(crate) yielded: u64,
    pub(crate) checkpoints: u64,
    pub(crate) compactions: u64,
    pub(crate) barrier_rows: u64,
}

impl<'a> CodecPagedIndex<'a> {
    fn new(session: &'a CompressionSession) -> Result<Self> {
        let metadata = session
            .memory()
            .reserve(std::mem::size_of::<Self>() as u64 + 8192)?;
        let memtable = session.source_memtable()?;
        let cache = SourcePageCache::new(session)?;
        let mut bloom = session.source_filter()?;
        let bloom_len = bloom.capacity();
        bloom.resize(bloom_len, 0)?;
        Ok(Self {
            session,
            memtable,
            runs: std::array::from_fn(|_| None),
            run_count: 0,
            cache: RefCell::new(cache),
            bloom,
            last_position: None,
            last_ordinal: None,
            poisoned: Cell::new(false),
            finished: false,
            metrics: Cell::new(CodecIndexMetrics::default()),
            _metadata: metadata,
        })
    }

    fn active(&self) -> Result<()> {
        if self.poisoned.get() {
            return Err(Error::temp_storage_context(
                "codec source index is poisoned",
            ));
        }
        if self.finished {
            return Err(Error::temp_storage_context(
                "codec source index is finished",
            ));
        }
        self.session.check_cancelled()
    }

    pub(crate) fn metrics(&self) -> CodecIndexMetrics {
        self.metrics.get()
    }
    fn note(&self, update: impl FnOnce(&mut CodecIndexMetrics) -> Result<()>) -> Result<()> {
        let mut m = self.metrics.get();
        update(&mut m)?;
        self.metrics.set(m);
        Ok(())
    }
    fn terminal<R>(&self, result: Result<R>) -> Result<R> {
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }

    fn key_bits(&self, kind: u8, key: &[u8]) -> Option<[usize; 6]> {
        let bits = self.bloom.len().checked_mul(8)?;
        if bits == 0 {
            return None;
        }
        let mut h = blake3::Hasher::new();
        h.update(b"SREP-OFF-SOURCE-KEY-v1\0");
        h.update(&[kind, key.len() as u8]);
        h.update(key);
        let digest = h.finalize();
        let bytes = digest.as_bytes();
        Some(std::array::from_fn(|i| {
            let at = i * 4;
            (u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize) % bits
        }))
    }

    fn bloom_contains(&self, kind: u8, key: &[u8]) -> bool {
        let Some(bits) = self.key_bits(kind, key) else {
            return true;
        };
        bits.into_iter()
            .all(|bit| self.bloom.as_slice()[bit / 8] & (1 << (bit % 8)) != 0)
    }

    fn bloom_insert(&mut self, kind: u8, key: &[u8]) {
        let Some(bits) = self.key_bits(kind, key) else {
            return;
        };
        for bit in bits {
            self.bloom.as_mut_slice()[bit / 8] |= 1 << (bit % 8);
        }
    }

    fn verify_epoch(&self) -> Result<()> {
        for slot in &self.runs[..self.run_count] {
            slot.as_ref()
                .expect("run slot before run_count is occupied")
                .verify_envelope()?;
        }
        Ok(())
    }

    fn read_cached(&self, table: &PagedSourceTable, index: u64) -> Result<IndexEntry> {
        self.session.check_cancelled()?;
        self.cache
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("source page cache already borrowed"))?
            .read_entry(table, index)
    }

    fn memtable_lower_bound(&self, kind: u8, key: &[u8]) -> usize {
        let mut low = 0;
        let mut high = self.memtable.len();
        while low < high {
            let middle = low + (high - low) / 2;
            if memtable_key_order(&self.memtable[middle], kind, key).is_lt() {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn run_lower_bound(&self, table: &PagedSourceTable, kind: u8, key: &[u8]) -> Result<u64> {
        let mut low = 0;
        let mut high = table.entry_count();
        while low < high {
            let middle = low + (high - low) / 2;
            let entry = self.read_cached(table, middle)?;
            self.note(|m| {
                m.comparisons = m
                    .comparisons
                    .checked_add(1)
                    .ok_or_else(|| Error::temp_limit("source metric overflow"))?;
                Ok(())
            })?;
            if memtable_key_order(&entry, kind, key).is_lt() {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low)
    }

    fn query_run_start(
        &self,
        table: &PagedSourceTable,
        kind: u8,
        key: &[u8],
    ) -> Result<Option<(u64, IndexEntry)>> {
        let at = self.run_lower_bound(table, kind, key)?;
        if at == table.entry_count() {
            return Ok(None);
        }
        let entry = self.read_cached(table, at)?;
        if entry.key_kind == kind && entry.key_slice() == key {
            Ok(Some((at, entry)))
        } else {
            Ok(None)
        }
    }

    fn merge_and_write(
        &self,
        incoming: Option<IndexEntry>,
        merge_count: usize,
        mut builder: SourcePageBuilder,
    ) -> Result<PagedSourceTable> {
        let mut heads: [Option<IndexEntry>; CODEC_MAX_SOURCES] = std::array::from_fn(|_| None);
        let mut next: [u64; CODEC_MAX_SOURCES] = [0; CODEC_MAX_SOURCES];
        let mut source_kind = [0u8; CODEC_MAX_SOURCES];
        let mut source_count = 0usize;

        // Normal checkpoints create one immutable run from the current
        // memtable.  Only a full run set performs a bounded compaction; this
        // bounds resident run metadata, not total rewrite work. A fixed run cap
        // can still rewrite old rows repeatedly; cumulative I/O is measured.
        for run_index in 0..merge_count {
            let table = self.runs[run_index]
                .as_ref()
                .expect("run slot before run_count is occupied");
            source_kind[source_count] = run_index as u8;
            heads[source_count] = Some(self.read_cached(table, 0)?);
            next[source_count] = 0;
            source_count += 1;
        }
        if !self.memtable.is_empty() {
            source_kind[source_count] = CODEC_MAX_RUNS as u8;
            heads[source_count] = Some(self.memtable[0]);
            next[source_count] = 0;
            source_count += 1;
        }
        if let Some(entry) = incoming {
            source_kind[source_count] = (CODEC_MAX_RUNS + 1) as u8;
            heads[source_count] = Some(entry);
            next[source_count] = 0;
            source_count += 1;
        }

        while let Some(selected) = (0..source_count)
            .filter(|&i| heads[i].is_some())
            .min_by(|&a, &b| index_entry_order(&heads[a].unwrap(), &heads[b].unwrap()))
        {
            let entry = heads[selected].take().unwrap();
            builder.append(entry)?;
            let source = source_kind[selected] as usize;
            let position = next[selected]
                .checked_add(1)
                .ok_or_else(|| Error::temp_limit("codec source cursor overflow"))?;
            next[selected] = position;
            if source < merge_count {
                let table = self.runs[source]
                    .as_ref()
                    .expect("run slot before run_count is occupied");
                if position < table.entry_count() {
                    heads[selected] = Some(self.read_cached(table, position)?);
                }
            } else if source == CODEC_MAX_RUNS {
                if position < self.memtable.len() as u64 {
                    heads[selected] = Some(self.memtable[position as usize]);
                }
            }
        }
        builder.finish()
    }

    fn checkpoint(&mut self, incoming: Option<IndexEntry>) -> Result<()> {
        let result = (|| {
            self.active()?;
            // Choose ONCE, before allocation. Compact two smallest runs at
            // pressure/full-set boundaries; no repeated giant+tiny full merge.
            let pressured = self.run_count == CODEC_MAX_RUNS
                || self.session.handle_usage().current >= self.session.policy().max_handles
                || self
                    .session
                    .file_usage()
                    .current
                    .checked_add(2)
                    .is_none_or(|n| n > self.session.policy().max_temp_files);
            if pressured && self.run_count < 2 {
                return Err(Error::temp_storage_context(
                    "source checkpoint lacks replacement headroom",
                ));
            }
            let merge_count = if pressured { 2 } else { 0 };
            if merge_count == 2 {
                // All slots are occupied before run_count; swapping descriptors
                // has no effect on query order (heads are merged by entry key).
                for target in 0..2 {
                    let smallest = (target..self.run_count)
                        .min_by_key(|&i| self.runs[i].as_ref().unwrap().entry_count())
                        .unwrap();
                    self.runs.swap(target, smallest);
                }
            }
            let guard = if merge_count == 2 {
                Some(self.session.emergency_merge()?)
            } else {
                None
            };
            let builder = if guard.is_some() {
                SourcePageBuilder::emergency(self.session, 0)?
            } else {
                SourcePageBuilder::new(self.session, 0)?
            };
            let mut replacement = self.merge_and_write(incoming, merge_count, builder)?;
            // Verify inputs freshly BEFORE taking ownership out of live slots.
            // Persistent corruption leaves both original owners charged on error.
            for table in self.runs[..merge_count].iter().flatten() {
                table.verify_uncached_all()?;
            }
            if let Some(guard) = guard {
                let first = match self.runs[0].take().unwrap().retire() {
                    Ok(token) => token,
                    Err(error) => {
                        drop(replacement);
                        drop(guard);
                        return Err(error);
                    }
                };
                let second = match self.runs[1].take().unwrap().retire() {
                    Ok(token) => token,
                    Err(error) => {
                        drop(replacement);
                        drop(guard);
                        drop(first);
                        return Err(error);
                    }
                };
                match guard.adopt_output(first, second) {
                    Ok(lease) => replacement.adopt(lease),
                    Err((error, guard, a, b)) => {
                        drop(replacement);
                        drop(guard);
                        drop(a);
                        drop(b);
                        return Err(error);
                    }
                }
                self.note(|m| {
                    m.compactions = m
                        .compactions
                        .checked_add(1)
                        .ok_or_else(|| Error::temp_limit("source metric overflow"))?;
                    Ok(())
                })?;
            }
            self.session.note_candidate_index_spill()?;
            let old = std::mem::replace(&mut self.runs, std::array::from_fn(|_| None));
            let mut retained = 0;
            for (old_slot, table) in old.into_iter().enumerate() {
                if old_slot >= merge_count && table.is_some() {
                    self.runs[retained] = table;
                    retained += 1;
                }
            }
            self.runs[retained] = Some(replacement);
            self.run_count = retained + 1;
            self.memtable.clear_with_capacity();
            self.note(|m| {
                m.checkpoints = m
                    .checkpoints
                    .checked_add(1)
                    .ok_or_else(|| Error::temp_limit("source metric overflow"))?;
                Ok(())
            })?;
            Ok(())
        })();
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }

    fn query_inner(
        &self,
        kind: u8,
        key: &[u8],
        before: u64,
        max_distance: u64,
        callback: &mut dyn FnMut(IndexEntry) -> Result<()>,
    ) -> Result<()> {
        let mut heads: [Option<IndexEntry>; CODEC_MAX_SOURCES] = std::array::from_fn(|_| None);
        let mut positions: [u64; CODEC_MAX_SOURCES] = [0; CODEC_MAX_SOURCES];
        let mut sources = 0usize;
        for run_index in 0..self.run_count {
            let table = self.runs[run_index]
                .as_ref()
                .expect("run slot before run_count is occupied");
            if let Some((at, entry)) = self.query_run_start(table, kind, key)? {
                heads[sources] = Some(entry);
                positions[sources] = at;
            }
            sources += 1;
        }
        let mem = self.memtable_lower_bound(kind, key);
        if mem < self.memtable.len()
            && self.memtable[mem].key_kind == kind
            && self.memtable[mem].key_slice() == key
        {
            heads[sources] = Some(self.memtable[mem]);
            positions[sources] = mem as u64;
        }
        sources += 1;

        loop {
            self.session.check_cancelled()?;
            let Some(selected) = (0..sources)
                .filter(|&i| heads[i].is_some())
                .min_by(|&a, &b| index_entry_order(&heads[a].unwrap(), &heads[b].unwrap()))
            else {
                break;
            };
            let entry = heads[selected].take().unwrap();
            if entry.key_kind != kind || entry.key_slice() != key {
                // Every source is ordered by key and cannot return to this
                // key after leaving it.
                continue;
            }
            if entry.position < before {
                if max_distance == 0 || before - entry.position <= max_distance {
                    callback(entry)?;
                    self.note(|m| {
                        m.yielded = m
                            .yielded
                            .checked_add(1)
                            .ok_or_else(|| Error::temp_limit("source metric overflow"))?;
                        Ok(())
                    })?;
                } else {
                    // Positions descend inside one key group, so this source
                    // can be retired once it is too old.
                    heads[selected] = None;
                    continue;
                }
            }
            let source = selected;
            if source < self.run_count {
                let table = self.runs[source]
                    .as_ref()
                    .expect("run slot before run_count is occupied");
                let next = positions[source]
                    .checked_add(1)
                    .ok_or_else(|| Error::corrupt_index("codec source cursor overflow"))?;
                positions[source] = next;
                if next < table.entry_count() {
                    let next_entry = self.read_cached(table, next)?;
                    if next_entry.key_kind == kind && next_entry.key_slice() == key {
                        heads[source] = Some(next_entry);
                    }
                }
            } else if positions[source] + 1 < self.memtable.len() as u64 {
                positions[source] += 1;
                let next_entry = self.memtable[positions[source] as usize];
                if next_entry.key_kind == kind && next_entry.key_slice() == key {
                    heads[source] = Some(next_entry);
                }
            }
        }
        Ok(())
    }
}

impl Drop for CodecPagedIndex<'_> {
    fn drop(&mut self) {
        // Disk owners disappear before RAII source loans return to the session.
        // An unverified/failed index aborts source reuse for this session: a later
        // overlay cannot replace the missing base completion certificate.
        if !self.finished {
            self.session.source_failed();
        }
        for slot in &mut self.runs {
            drop(slot.take());
        }
        self.run_count = 0;
        self.memtable.clear_with_capacity();
        self.bloom.as_mut_slice().fill(0);
    }
}

impl CandidateIndex for CodecPagedIndex<'_> {
    fn insert(&mut self, entry: IndexEntry) -> Result<()> {
        let result = self.insert_inner(entry);
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }

    fn for_each_candidate(
        &self,
        key_kind: u8,
        key_bytes: &[u8],
        before: u64,
        max_distance: u64,
        callback: &mut dyn FnMut(IndexEntry) -> Result<()>,
    ) -> Result<()> {
        let result = (|| {
            self.active()?;
            validate_query_shape(key_kind, key_bytes)?;
            self.note(|m| {
                m.queries = m
                    .queries
                    .checked_add(1)
                    .ok_or_else(|| Error::temp_limit("source metric overflow"))?;
                Ok(())
            })?;
            // Negative membership costs no file I/O; final/retirement barriers
            // still verify every persisted page, including never-queried ones.
            if !self.bloom_contains(key_kind, key_bytes) {
                self.note(|m| {
                    m.negative_queries = m
                        .negative_queries
                        .checked_add(1)
                        .ok_or_else(|| Error::temp_limit("source metric overflow"))?;
                    Ok(())
                })?;
                return Ok(());
            }
            self.query_inner(key_kind, key_bytes, before, max_distance, callback)
        })();
        self.terminal(result)
    }
    fn memory_budget(&self) -> &MemoryBudget {
        self.memtable.budget()
    }
    fn finish_epoch(&mut self) -> Result<()> {
        let result = (|| {
            self.active()?;
            if self.memtable.is_empty() {
                self.verify_epoch()
            } else {
                self.checkpoint(None)
            }
        })();
        self.terminal(result)
    }
}
impl CodecPagedIndex<'_> {
    fn insert_inner(&mut self, entry: IndexEntry) -> Result<()> {
        self.active()?;
        entry.validate()?;
        if self
            .last_position
            .is_some_and(|previous| entry.position <= previous)
        {
            return Err(Error::corrupt_index(
                "codec source positions must increase strictly",
            ));
        }
        if self
            .last_ordinal
            .is_some_and(|previous| entry.insertion_ordinal <= previous)
        {
            return Err(Error::corrupt_index(
                "codec source ordinals must increase strictly",
            ));
        }
        self.bloom_insert(entry.key_kind, entry.key_slice());
        let next_bytes = self
            .memtable
            .len()
            .checked_add(1)
            .and_then(|n| n.checked_mul(std::mem::size_of::<IndexEntry>()));
        if next_bytes.is_none_or(|n| n as u64 > self.session.candidate_index_memtable_bytes())
            || self.memtable.capacity() == self.memtable.len()
        {
            self.checkpoint(Some(entry))?;
        } else {
            if let Err(error) = self.memtable.insert_sorted(entry) {
                self.poisoned.set(true);
                return Err(error);
            }
        }
        self.last_position = Some(entry.position);
        self.last_ordinal = Some(entry.insertion_ordinal);
        Ok(())
    }
}

impl SourceVerifier for CodecPagedIndex<'_> {
    fn finish_verified(&mut self, session: &CompressionSession) -> Result<()> {
        let result = (|| {
            self.active()?;
            if self.session.identity() != session.identity() {
                return Err(Error::corrupt_index("cross-session source verification"));
            }
            // Resident source entries need no disk flush merely to finish.
            for slot in &mut self.runs[..self.run_count] {
                slot.as_mut()
                    .expect("run slot before run_count is occupied")
                    .verify_uncached()?;
            }
            session.check_cancelled()?;
            let rows = self.runs.iter().flatten().try_fold(0u64, |n, t| {
                n.checked_add(t.entry_count())
                    .ok_or_else(|| Error::temp_limit("barrier rows overflow"))
            })?;
            self.note(|m| {
                m.barrier_rows = rows;
                Ok(())
            })?;
            let old = std::mem::replace(&mut self.runs, std::array::from_fn(|_| None));
            self.run_count = 0;
            drop(old);
            self.memtable.clear_with_capacity();
            self.finished = true;
            Ok(())
        })();
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }
}

impl CodecSourceIndex for CodecPagedIndex<'_> {}

pub(crate) struct CodecPagedIndexFactory<'a> {
    session: &'a CompressionSession,
}

impl<'a> CodecPagedIndexFactory<'a> {
    pub(crate) fn new(session: &'a CompressionSession) -> Self {
        Self { session }
    }
}

impl<'a> CodecSourceIndexFactory for CodecPagedIndexFactory<'a> {
    type Index = CodecPagedIndex<'a>;

    fn create(&self, session: &CompressionSession) -> Result<Self::Index> {
        if self.session.identity() != session.identity() {
            return Err(Error::corrupt_index("cross-session codec source factory"));
        }
        CodecPagedIndex::new(self.session)
    }
}

pub(crate) fn new_codec_candidate_index(
    session: &CompressionSession,
) -> Result<CodecPagedIndex<'_>> {
    CodecPagedIndex::new(session)
}

#[cfg(test)]
mod codec_index_tests {
    use super::*;
    use crate::config::ResourceConfig;
    use crate::offload::contracts::{CodecSourceIndexFactory, SourceVerifier};
    use crate::offload::session::{CompressionSession, OffloadPolicy};

    fn setup() -> (ResourceContext, CompressionSession) {
        let resources = ResourceConfig {
            memory: 16 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let mut context = ResourceContext::with_resources(&resources).unwrap();
        context.candidate_index_memtable_bytes = Some(1);
        let session =
            CompressionSession::new(&context, &resources, OffloadPolicy::default()).unwrap();
        (context, session)
    }

    fn source(position: u64, ordinal: u64) -> IndexEntry {
        IndexEntry::new(0, &7u64.to_le_bytes(), position, ordinal, &[]).unwrap()
    }

    #[test]
    fn private_codec_index_streams_paged_hits_and_finishes_uncached() {
        let (context, session) = setup();
        // One-byte override intentionally forces every source row through the
        // bounded run path and exercises run-head merging without allocating
        // a hit vector.
        let factory = CodecPagedIndexFactory::new(&session);
        let mut index = factory.create(&session).unwrap();
        for position in 0..20 {
            index.insert(source(position, position)).unwrap();
        }
        let mut rows = Vec::new();
        index
            .for_each_candidate(0, &7u64.to_le_bytes(), 20, 3, &mut |entry| {
                rows.push((entry.position, entry.insertion_ordinal));
                Ok(())
            })
            .unwrap();
        assert_eq!(rows, vec![(19, 19), (18, 18), (17, 17)]);
        index.finish_verified(&session).unwrap();
        drop(index);
        drop(session);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }

    #[test]
    fn private_codec_index_rejects_nonmonotone_source_positions_and_ordinals() {
        let (_context, session) = setup();
        let mut index = new_codec_candidate_index(&session).unwrap();
        index.insert(source(4, 4)).unwrap();
        assert_eq!(
            index.insert(source(4, 5)).unwrap_err().kind(),
            ErrorKind::CorruptIndex
        );
        assert!(index.finish_verified(&session).is_err());
        drop(index);
        let (_context, session) = setup();
        let mut index = new_codec_candidate_index(&session).unwrap();
        index.insert(source(4, 4)).unwrap();
        assert_eq!(
            index.insert(source(5, 4)).unwrap_err().kind(),
            ErrorKind::CorruptIndex
        );
        assert!(index.finish_verified(&session).is_err());
    }

    #[test]
    fn private_codec_index_zero_memtable_uses_direct_spill() {
        let resources = ResourceConfig {
            memory: 16 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let mut context = ResourceContext::with_resources(&resources).unwrap();
        context.candidate_index_memtable_bytes = Some(0);
        let session =
            CompressionSession::new(&context, &resources, OffloadPolicy::default()).unwrap();
        let mut index = new_codec_candidate_index(&session).unwrap();
        index.insert(source(1, 1)).unwrap();
        let mut got = Vec::new();
        index
            .for_each_candidate(0, &7u64.to_le_bytes(), 2, 0, &mut |entry| {
                got.push(entry.position);
                Ok(())
            })
            .unwrap();
        assert_eq!(got, vec![1]);
        index.finish_verified(&session).unwrap();
        drop(index);
        drop(session);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }

    fn configured(cap: u64, handles: u64, files: u64) -> (ResourceContext, CompressionSession) {
        let r = ResourceConfig {
            memory: 16 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let mut c = ResourceContext::with_resources(&r).unwrap();
        c.candidate_index_memtable_bytes = Some(cap);
        let s = CompressionSession::new(
            &c,
            &r,
            OffloadPolicy {
                max_handles: handles,
                max_temp_files: files,
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        (c, s)
    }
    fn keyed(n: u64) -> IndexEntry {
        IndexEntry::new(0, &(n % 7).to_le_bytes(), n * 3, n, &[]).unwrap()
    }
    fn private_rows(
        index: &CodecPagedIndex<'_>,
        key: u64,
        before: u64,
        distance: u64,
    ) -> Vec<IndexEntry> {
        let mut rows = Vec::new();
        index
            .for_each_candidate(0, &key.to_le_bytes(), before, distance, &mut |r| {
                rows.push(r);
                Ok(())
            })
            .unwrap();
        rows
    }
    #[test]
    fn stream_matches_public_hybrid_through_multiple_run_epochs_and_distances() {
        let (c, s) = configured(3 * std::mem::size_of::<IndexEntry>() as u64, 128, 192);
        let mut private = new_codec_candidate_index(&s).unwrap();
        let r = ResourceConfig::default();
        let public_context = ResourceContext::with_resources(&r).unwrap();
        let mut public = HybridCandidateIndex::with_memtable_bytes(&public_context, 512).unwrap();
        for n in 0..90 {
            private.insert(keyed(n)).unwrap();
            public.insert(keyed(n)).unwrap();
            if n % 11 == 0 {
                private.finish_epoch().unwrap();
                public.finish_epoch().unwrap();
            }
            if n % 9 == 0 {
                for key in 0u64..8 {
                    for dist in [0, 1, 15, 200] {
                        let mut expected = Vec::new();
                        public
                            .for_each_candidate(0, &key.to_le_bytes(), n * 3 + 1, dist, &mut |e| {
                                expected.push(e);
                                Ok(())
                            })
                            .unwrap();
                        assert_eq!(private_rows(&private, key, n * 3 + 1, dist), expected);
                    }
                }
            }
        }
        assert!(private.metrics().compactions > 0);
        assert!(private.metrics().checkpoints > 16);
        private.finish_verified(&s).unwrap();
        assert!(private.finish_verified(&s).is_err());
        drop(private);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.handle_usage().current, 7);
    }
    #[test]
    fn smallest_fd_policy_repeatedly_adopts_live_replacements() {
        let (c, s) = configured(0, 10, 4);
        let mut i = new_codec_candidate_index(&s).unwrap();
        for n in 0..65 {
            i.insert(source(n, n)).unwrap();
        }
        assert!(i.metrics().compactions >= 2);
        assert!(i.run_count <= 3);
        assert_eq!(
            private_rows(&i, 7, 65, 3)
                .iter()
                .map(|e| e.position)
                .collect::<Vec<_>>(),
            vec![64, 63, 62]
        );
        assert!(s.emergency_merge().is_ok());
        assert!(s.handle_usage().peak <= 10);
        assert!(s.file_usage().peak <= 4);
        assert_eq!(c.candidate_index_spill_count(), i.metrics().checkpoints);
        i.finish_verified(&s).unwrap();
        drop(i);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.file_usage().current, 0);
        assert_eq!(s.handle_usage().current, 7);
    }
    fn run_path(i: &CodecPagedIndex<'_>, run: usize) -> std::path::PathBuf {
        let generation = i.runs[run].as_ref().unwrap().identity().generation;
        let dir = i.session.temp_directory().unwrap();
        for e in fs::read_dir(&*dir).unwrap() {
            let path = e.unwrap().path();
            let mut f = File::open(&path).unwrap();
            let mut header = [0; 128];
            f.read_exact(&mut header).unwrap();
            if &header[..8] == b"SRPOFF01"
                && header[14] == 5
                && u64::from_le_bytes(header[16..24].try_into().unwrap()) == generation
            {
                return path;
            }
        }
        panic!("owned test source run not found")
    }
    fn corrupt(path: &Path, offset: u64) {
        let mut f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        f.seek(SeekFrom::Start(offset)).unwrap();
        let mut b = [0];
        f.read_exact(&mut b).unwrap();
        b[0] ^= 0x80;
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.write_all(&b).unwrap();
        f.flush().unwrap();
    }
    #[test]
    fn positive_queries_bound_reads_and_bloom_negatives_read_no_run_bytes() {
        let (c, s) = configured(6000 * std::mem::size_of::<IndexEntry>() as u64, 128, 192);
        let mut i = new_codec_candidate_index(&s).unwrap();
        for n in 0u64..5000 {
            i.insert(IndexEntry::new(0, &n.to_le_bytes(), n, n, &[]).unwrap())
                .unwrap();
        }
        i.finish_epoch().unwrap();
        assert_eq!(i.run_count, 1);
        let before = s.io_used();
        for _ in 0..1000 {
            assert!(private_rows(&i, 999_999, 6000, 0).is_empty());
        }
        assert_eq!(s.io_used(), before);
        assert_eq!(i.metrics().negative_queries, 1000);
        // Force a true Bloom false-positive path without a probabilistic test.
        i.bloom.as_mut_slice().fill(255);
        let before = s.io_used();
        for _ in 0..1000 {
            assert!(private_rows(&i, 999_999, 6000, 0).is_empty());
        }
        let reads = s.io_used() - before;
        // 5000 entries occupy8 pages. At most binary-search page probes per
        // lookup; no full-run scan. Warm same-key cache needs only a few pages.
        assert!(
            reads <= 1000 * 3 * 65536,
            "unexpected whole-run query I/O: {reads}"
        );
        let before = s.io_used();
        for _ in 0..1000 {
            assert_eq!(private_rows(&i, 2345, 6000, 0).len(), 1);
        }
        let hit_reads = s.io_used() - before;
        assert!(hit_reads <= 1000 * 3 * 65536);
        assert!(i.metrics().comparisons < 3000 * 15);
        assert_eq!(i.metrics().yielded, 1000);
        eprintln!(
            "I1 read evidence: negative1000=0, false-positive1000={reads}, positive1000={hit_reads}, rows5000/pages8"
        );
        let before = s.io_used();
        i.finish_verified(&s).unwrap();
        assert!(s.io_used() - before >= 8 * 65536);
        assert_eq!(i.metrics().barrier_rows, 5000);
        drop(i);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
    }
    #[test]
    fn cached_and_never_queried_corruption_fail_final_barrier() {
        for offset in [0, 16, 48, 64, 128 + 65536 + 90] {
            let (c, s) = configured(2000 * std::mem::size_of::<IndexEntry>() as u64, 128, 192);
            let mut i = new_codec_candidate_index(&s).unwrap();
            for n in 0..1500 {
                i.insert(source(n, n)).unwrap();
            }
            i.finish_epoch().unwrap();
            assert_eq!(private_rows(&i, 7, 1500, 1).len(), 1);
            let path = run_path(&i, 0);
            corrupt(&path, offset);
            // A negative query cannot conceal corruption at finish.
            assert!(private_rows(&i, 999, 1600, 0).is_empty());
            assert_eq!(
                i.finish_verified(&s).unwrap_err().kind(),
                ErrorKind::CorruptIndex
            );
            assert!(i.finish_verified(&s).is_err());
            assert!(path.exists());
            assert!(c.temp.current() > 0);
            drop(i);
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), 0);
        }
    }
    #[test]
    fn corrupt_retiring_input_is_not_deleted_or_unaccounted_on_merge_failure() {
        let (c, s) = configured(0, 10, 4);
        let mut i = new_codec_candidate_index(&s).unwrap();
        for n in 0..3 {
            i.insert(source(n, n)).unwrap();
        }
        let path = run_path(&i, 0);
        corrupt(&path, 128 + 80);
        let before = c.temp.current();
        assert!(i.insert(source(3, 3)).is_err());
        assert!(i.finish_verified(&s).is_err());
        assert!(path.exists());
        assert!(c.temp.current() >= before);
        drop(i);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.handle_usage().current, 7);
    }
    #[test]
    fn every_operation_error_prevents_successful_source_completion() {
        use crate::offload::session::{FaultAction, Operation};
        for mode in 0..5 {
            let (c, s) = configured(0, 128, 192);
            let mut i = new_codec_candidate_index(&s).unwrap();
            i.insert(source(1, 1)).unwrap();
            match mode {
                0 => {
                    s.cancel();
                    assert!(i.finish_verified(&s).is_err());
                }
                1 => {
                    s.inject(Operation::Read, 0, FaultAction::Fail);
                    assert!(
                        i.for_each_candidate(0, &7u64.to_le_bytes(), 2, 0, &mut |_| Ok(()))
                            .is_err()
                    );
                }
                2 => {
                    assert!(
                        i.for_each_candidate(0, &7u64.to_le_bytes(), 2, 0, &mut |_| Err(
                            Error::temp_limit("sink failure")
                        ))
                        .is_err()
                    );
                }
                3 => {
                    s.inject(Operation::Create, 0, FaultAction::Fail);
                    assert!(i.insert(source(2, 2)).is_err());
                }
                _ => {
                    assert!(i.insert(source(1, 2)).is_err());
                }
            }
            assert!(i.finish_verified(&s).is_err());
            drop(i);
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), 0);
            assert_eq!(s.file_usage().current, 0);
            assert_eq!(s.handle_usage().current, 7);
        }
        let (_, s) = configured(0, 128, 192);
        let (_, other) = configured(0, 128, 192);
        let mut i = new_codec_candidate_index(&s).unwrap();
        assert!(i.finish_verified(&other).is_err());
        assert!(i.finish_verified(&s).is_err());
        let (_, s) = configured(0, 128, 192);
        let mut i = new_codec_candidate_index(&s).unwrap();
        i.finish_verified(&s).unwrap();
    }

    #[test]
    fn key_kinds_nested_candidate_spills_and_source_limit_failures() {
        use crate::offload::candidate_sink::ExternalCandidateSink;
        use crate::offload::contracts::{
            CandidateRow, CandidateRunSink, CandidateSink, Provenance,
        };
        let (c, s) = configured(1, 128, 192);
        let mut index = new_codec_candidate_index(&s).unwrap();
        let mut expected = Vec::new();
        for n in 0..40 {
            let kind = (n % 6) as u8;
            let (key_len, meta_len) = expected_shape(kind).unwrap();
            let key = [7; 32];
            let metadata = [0; 32];
            let entry =
                IndexEntry::new(kind, &key[..key_len], n, n, &metadata[..meta_len]).unwrap();
            expected.push(entry);
            index.insert(entry).unwrap();
        }
        let mut sink = ExternalCandidateSink::with_chunk_limit(&s, 1).unwrap();
        for kind in 0..6 {
            let (key_len, _) = expected_shape(kind).unwrap();
            let mut actual = Vec::new();
            index
                .for_each_candidate(kind, &[7; 32][..key_len], 40, 0, &mut |e| {
                    actual.push(e);
                    sink.emit(CandidateRow {
                        candidate: crate::match_ir::MatchCandidate {
                            src: e.position,
                            dst: 100 + e.position,
                            len: 30,
                            insertion_ordinal: e.insertion_ordinal,
                        },
                        provenance: Provenance::EXTERNAL,
                    })
                })
                .unwrap();
            let mut wanted = expected
                .iter()
                .copied()
                .filter(|e| e.key_kind == kind)
                .collect::<Vec<_>>();
            wanted.sort_unstable_by(index_entry_order);
            assert_eq!(actual, wanted);
        }
        let store = sink.finish().unwrap();
        index.finish_verified(&s).unwrap();
        drop(store);
        drop(index);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert!(s.handle_usage().peak <= 128);

        for small_temp in [true, false] {
            let r = ResourceConfig {
                memory: 16 * 1024 * 1024,
                temp_limit: if small_temp { 128 } else { 1024 * 1024 },
                ..ResourceConfig::default()
            };
            let mut c = ResourceContext::with_resources(&r).unwrap();
            c.candidate_index_memtable_bytes = Some(0);
            let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
            let mut i = new_codec_candidate_index(&s).unwrap();
            let hold = if small_temp {
                None
            } else {
                Some(
                    s.acquire_handles(s.policy().max_handles - s.handle_usage().current)
                        .unwrap(),
                )
            };
            assert!(i.insert(source(1, 1)).is_err());
            assert!(i.finish_verified(&s).is_err());
            drop(hold);
            drop(i);
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), 0);
            assert_eq!(s.file_usage().current, 0);
        }
    }

    #[test]
    fn retire_reverification_failure_closes_replacement_before_emergency_return() {
        use crate::offload::session::{FaultAction, LifecycleEvent, Operation};
        for failed_retire in [0, 1] {
            let (c, s) = configured(0, 10, 4);
            let mut index = new_codec_candidate_index(&s).unwrap();
            for n in 0..3 {
                index.insert(source(n, n)).unwrap();
            }
            s.clear_lifecycle();
            s.inject(Operation::RetireVerify, failed_retire, FaultAction::Fail);
            assert!(index.insert(source(3, 3)).is_err());
            assert!(index.finish_verified(&s).is_err());
            let events = s.lifecycle_events();
            assert_eq!(
                events
                    .iter()
                    .filter(|&&e| e == LifecycleEvent::RetireVerify)
                    .count(),
                failed_retire as usize + 1
            );
            let closed = events
                .iter()
                .position(|e| *e == LifecycleEvent::EmergencyFileClosed)
                .unwrap();
            let returned = events
                .iter()
                .position(|e| *e == LifecycleEvent::EmergencyReturned)
                .unwrap();
            assert!(
                closed < returned,
                "actual replacement close must precede emergency return: {events:?}"
            );
            assert!(s.emergency_merge().is_ok());
            drop(index);
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), 0);
            assert_eq!(s.handle_usage().current, 7);
            assert_eq!(s.file_usage().current, 0);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Identity {
    kind: u8,
    key_len: u8,
    key: [u8; MAX_KEY_BYTES],
    position: u64,
    metadata_len: u8,
    metadata: [u8; MAX_METADATA_BYTES],
}

fn identity(entry: &IndexEntry) -> Identity {
    Identity {
        kind: entry.key_kind,
        key_len: entry.key_len,
        key: entry.key_bytes,
        position: entry.position,
        metadata_len: entry.metadata_len,
        metadata: entry.metadata,
    }
}

fn same_identity(a: &IndexEntry, b: &IndexEntry) -> bool {
    identity(a) == identity(b)
}

fn scratch_order(a: &IndexEntry, b: &IndexEntry, order: ScratchOrder) -> Ordering {
    match order {
        ScratchOrder::Identity => identity(a)
            .cmp(&identity(b))
            .then_with(|| a.insertion_ordinal.cmp(&b.insertion_ordinal)),
        ScratchOrder::PersistedQuery => index_entry_order(a, b),
    }
}

pub(crate) fn index_entry_order(a: &IndexEntry, b: &IndexEntry) -> Ordering {
    a.key_kind
        .cmp(&b.key_kind)
        .then_with(|| a.key_slice().cmp(b.key_slice()))
        .then_with(|| b.position.cmp(&a.position))
        .then_with(|| a.insertion_ordinal.cmp(&b.insertion_ordinal))
        .then_with(|| a.metadata_slice().cmp(b.metadata_slice()))
}

fn matches_query(
    entry: &IndexEntry,
    key_kind: u8,
    key_bytes: &[u8],
    before: u64,
    max_distance: u64,
) -> bool {
    entry.key_kind == key_kind
        && entry.key_slice() == key_bytes
        && entry.position < before
        && (max_distance == 0 || before - entry.position <= max_distance)
}

fn validate_query_shape(key_kind: u8, key_bytes: &[u8]) -> Result<()> {
    let expected = expected_shape(key_kind)?;
    if key_bytes.len() != expected.0 {
        return Err(Error::invalid_config(
            "candidate index query key shape is invalid",
        ));
    }
    Ok(())
}

fn memtable_key_order(entry: &IndexEntry, key_kind: u8, key_bytes: &[u8]) -> Ordering {
    entry
        .key_kind
        .cmp(&key_kind)
        .then_with(|| entry.key_slice().cmp(key_bytes))
}

fn collect_memtable_matches(
    entries: &[IndexEntry],
    key_kind: u8,
    key_bytes: &[u8],
    before: u64,
    max_distance: u64,
    output: &mut BudgetedVec<IndexEntry>,
) -> Result<()> {
    let mut low = 0usize;
    let mut high = entries.len();
    while low < high {
        let middle = low + (high - low) / 2;
        if memtable_key_order(&entries[middle], key_kind, key_bytes).is_lt() {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    for &entry in &entries[low..] {
        if entry.key_kind != key_kind || entry.key_slice() != key_bytes {
            break;
        }
        if matches_query(&entry, key_kind, key_bytes, before, max_distance) {
            output.push(entry)?;
        }
    }
    Ok(())
}

fn canonicalize_query_hits(hits: &mut BudgetedVec<IndexEntry>) {
    canonicalize_identity(hits);
    hits.sort_unstable_by(index_entry_order);
}

trait SortedInsert {
    fn insert_sorted(&mut self, value: IndexEntry) -> Result<()>;
}

impl SortedInsert for BudgetedVec<IndexEntry> {
    fn insert_sorted(&mut self, value: IndexEntry) -> Result<()> {
        let index = self
            .as_slice()
            .binary_search_by(|current| index_entry_order(current, &value))
            .unwrap_or_else(|index| index);
        self.insert(index, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(position: u64, ordinal: u64) -> IndexEntry {
        IndexEntry::new(0, &[7; 8], position, ordinal, &[]).unwrap()
    }

    #[test]
    fn canonical_order_is_independent_of_insertion_order() {
        let budget = MemoryBudget::new(4096);
        let mut index = RamCandidateIndex::new(&budget).unwrap();
        index
            .insert(IndexEntry::new(0, &[0; 8], 2, 1, &[]).unwrap())
            .unwrap();
        index
            .insert(IndexEntry::new(0, &[0; 8], 1, 2, &[]).unwrap())
            .unwrap();
        assert_eq!(index.entries()[0].position, 2);
    }

    #[test]
    fn checkpoint_canonicalizes_duplicate_across_run_memtable_and_incoming() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
        context.temp_dir = temp_dir.path().to_path_buf();
        let mut hybrid = HybridCandidateIndex::with_memtable_bytes_in(
            &context,
            temp_dir.path(),
            2 * std::mem::size_of::<IndexEntry>() as u64,
        )
        .unwrap();
        let oracle_budget = MemoryBudget::new(64 * 1024);
        let mut oracle = RamCandidateIndex::new(&oracle_budget).unwrap();

        for position in 0..16u64 {
            let value = test_entry(position * 2, position);
            hybrid.memtable.insert_sorted(value).unwrap();
            hybrid.checkpoint(None).unwrap();
            oracle.insert(value).unwrap();
        }
        assert_eq!(hybrid.run_count(), 16);
        let before_generation = hybrid.next_generation;
        let before_spills = context.candidate_index_spill_count();

        let run_memtable_duplicate = test_entry(10, 40);
        let memtable_distinct = test_entry(200, 41);
        hybrid
            .memtable
            .insert_sorted(run_memtable_duplicate)
            .unwrap();
        hybrid.memtable.insert_sorted(memtable_distinct).unwrap();
        oracle.insert(run_memtable_duplicate).unwrap();
        oracle.insert(memtable_distinct).unwrap();
        let incoming = test_entry(10, 2);
        oracle.insert(incoming).unwrap();

        hybrid.checkpoint(Some(&incoming)).unwrap();

        assert!(hybrid.memtable.is_empty());
        assert_eq!(hybrid.run_count(), 2);
        assert_eq!(hybrid.next_generation, before_generation + 19);
        assert_eq!(context.candidate_index_spill_count(), before_spills + 1);
        let mut expected = Vec::new();
        oracle
            .for_each_candidate(0, &[7; 8], 301, 0, &mut |value| {
                expected.push((value.position, value.insertion_ordinal));
                Ok(())
            })
            .unwrap();
        let mut actual = Vec::new();
        hybrid
            .for_each_candidate(0, &[7; 8], 301, 0, &mut |value| {
                actual.push((value.position, value.insertion_ordinal));
                Ok(())
            })
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            actual
                .iter()
                .filter(|&&(position, _)| position == 10)
                .count(),
            1
        );
        assert_eq!(
            actual.iter().find(|&&(position, _)| position == 10),
            Some(&(10, 2))
        );

        let visible_paths = hybrid.run_paths();
        let files = fs::read_dir(temp_dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), visible_paths.len());
        assert!(files.iter().all(|path| visible_paths.contains(path)));
        drop(expected);
        drop(actual);
        drop(hybrid);
        drop(oracle);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
        assert!(temp_dir.path().read_dir().unwrap().next().is_none());
    }
}
