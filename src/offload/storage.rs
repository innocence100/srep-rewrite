//! Private fixed records and paged source runs. No native-layout serialization.
#[cfg(test)]
use super::contracts::StoreIdentity;
use super::contracts::{
    CandidateRow, Provenance, RandomStore, RecordCursor, ReplayStore, TableBuilder,
    VerifiedSourcePages,
};
use super::session::{
    CleanupEntry, ClosedHandle, CompressionSession, Operation, ResourceLease, SessionId, SessionIo,
    SourceLoan,
};
use crate::candidate_index::{IndexEntry, index_entry_order};
use crate::error::{Error, Result};
use crate::match_ir::Match;
use crate::resource::{BudgetedVec, MemoryBudget, Reservation};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

pub(crate) const HEADER_LEN: u64 = 128;
pub(crate) const PAGE_LEN: usize = 65536;
pub(crate) const SOURCE_PAGE_ENTRIES: usize = 629;
const SOURCE_WIDTH: usize = 104;
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("checked fixed width"))
}
fn put(b: &mut [u8], at: usize, n: u64) {
    b[at..at + 8].copy_from_slice(&n.to_le_bytes());
}
fn hash16(domain: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let mut h = blake3::Hasher::new();
    h.update(domain);
    for p in parts {
        h.update(p);
    }
    h.finalize().as_bytes()[..16].try_into().unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordMeta {
    pub(crate) session: SessionId,
    pub(crate) generation: u64,
    pub(crate) file_id: [u8; 16],
    pub(crate) kind: u8,
    pub(crate) order: u8,
}
fn checksum(domain: &[u8], m: RecordMeta, body: &[u8]) -> [u8; 16] {
    hash16(
        domain,
        &[
            &m.session.wire_tag(),
            &m.file_id,
            &m.generation.to_le_bytes(),
            &[m.kind, m.order],
            body,
        ],
    )
}
fn envelope(b: &mut [u8], m: RecordMeta, seq: u64, domain: &[u8]) {
    let n = b.len();
    put(b, n - 24, seq);
    let sum = checksum(domain, m, &b[..n - 16]);
    b[n - 16..].copy_from_slice(&sum);
}
fn check(b: &[u8], m: RecordMeta, seq: u64, width: usize, domain: &[u8]) -> Result<()> {
    if b.len() != width
        || u64_at(b, width - 24) != seq
        || b[width - 16..] != checksum(domain, m, &b[..width - 16])
    {
        return Err(Error::corrupt_record(
            "offload record width/sequence/checksum",
        ));
    }
    Ok(())
}
pub(crate) trait FixedRecord: Copy {
    const KIND: u8;
    const ORDER: u8;
    const WIDTH: usize;
    fn encode(self, m: RecordMeta, seq: u64, b: &mut [u8]) -> Result<()>;
    fn decode(m: RecordMeta, seq: u64, b: &[u8]) -> Result<Self>;
    fn compare(&self, other: &Self) -> Ordering;
}
const CAND: &[u8] = b"SREP-OFF-CAND-v1\0";
impl FixedRecord for CandidateRow {
    const KIND: u8 = 1;
    const ORDER: u8 = 1;
    const WIDTH: usize = 64;
    fn encode(self, m: RecordMeta, s: u64, b: &mut [u8]) -> Result<()> {
        if b.len() != 64 {
            return Err(Error::corrupt_record("candidate width"));
        }
        b.fill(0);
        let c = self.candidate;
        put(b, 0, c.src);
        put(b, 8, c.dst);
        put(b, 16, c.len);
        put(b, 24, c.insertion_ordinal);
        b[32] = self.provenance.method;
        b[33] = self.provenance.phase;
        envelope(b, m, s, CAND);
        Ok(())
    }
    fn decode(m: RecordMeta, s: u64, b: &[u8]) -> Result<Self> {
        check(b, m, s, 64, CAND)?;
        if b[34..40] != [0; 6] {
            return Err(Error::corrupt_record("candidate padding"));
        }
        Ok(Self {
            candidate: crate::match_ir::MatchCandidate {
                src: u64_at(b, 0),
                dst: u64_at(b, 8),
                len: u64_at(b, 16),
                insertion_ordinal: u64_at(b, 24),
            },
            provenance: Provenance::new(b[32], b[33])
                .map_err(|_| Error::corrupt_record("candidate provenance"))?,
        })
    }
    fn compare(&self, o: &Self) -> Ordering {
        let a = self.candidate;
        let b = o.candidate;
        (
            a.src,
            a.dst,
            a.len,
            a.insertion_ordinal,
            self.provenance.method,
            self.provenance.phase,
        )
            .cmp(&(
                b.src,
                b.dst,
                b.len,
                b.insertion_ordinal,
                o.provenance.method,
                o.provenance.phase,
            ))
    }
}
/// V1 uses interval/end order wrappers without changing candidate encoding.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IntervalRow(pub(crate) CandidateRow);
#[derive(Clone, Copy, Debug)]
pub(crate) struct EndRow(pub(crate) CandidateRow);
macro_rules! candidate_order {
    ($t:ty,$order:expr,$cmp:expr) => {
        impl FixedRecord for $t {
            const KIND: u8 = 1;
            const ORDER: u8 = $order;
            const WIDTH: usize = 64;
            fn encode(self, m: RecordMeta, s: u64, b: &mut [u8]) -> Result<()> {
                self.0.encode(m, s, b)
            }
            fn decode(m: RecordMeta, s: u64, b: &[u8]) -> Result<Self> {
                Ok(Self(CandidateRow::decode(m, s, b)?))
            }
            fn compare(&self, o: &Self) -> Ordering {
                ($cmp)(self.0, o.0)
            }
        }
    };
}
candidate_order!(IntervalRow, 2, |a: CandidateRow, b: CandidateRow| {
    let a = a.candidate;
    let b = b.candidate;
    (a.dst, a.len, a.src, a.insertion_ordinal).cmp(&(b.dst, b.len, b.src, b.insertion_ordinal))
});
candidate_order!(EndRow, 3, |a: CandidateRow, b: CandidateRow| {
    let a = a.candidate;
    let b = b.candidate;
    (
        u128::from(a.dst) + u128::from(a.len),
        a.dst,
        a.src,
        std::cmp::Reverse(a.len),
        a.insertion_ordinal,
    )
        .cmp(&(
            u128::from(b.dst) + u128::from(b.len),
            b.dst,
            b.src,
            std::cmp::Reverse(b.len),
            b.insertion_ordinal,
        ))
});
impl FixedRecord for Match {
    const KIND: u8 = 3;
    const ORDER: u8 = 4;
    const WIDTH: usize = 64;
    fn encode(self, m: RecordMeta, s: u64, b: &mut [u8]) -> Result<()> {
        if b.len() != 64 {
            return Err(Error::corrupt_record("match width"));
        }
        b.fill(0);
        put(b, 0, self.src);
        put(b, 8, self.dst);
        put(b, 16, self.len);
        put(b, 24, self.origin_match_id);
        envelope(b, m, s, b"SREP-OFF-MATCH-v1\0");
        Ok(())
    }
    fn decode(m: RecordMeta, s: u64, b: &[u8]) -> Result<Self> {
        check(b, m, s, 64, b"SREP-OFF-MATCH-v1\0")?;
        if b[32..40] != [0; 8] {
            return Err(Error::corrupt_record("match padding"));
        }
        Ok(Self {
            src: u64_at(b, 0),
            dst: u64_at(b, 8),
            len: u64_at(b, 16),
            origin_match_id: u64_at(b, 24),
        })
    }
    fn compare(&self, o: &Self) -> Ordering {
        (
            self.dst,
            self.src,
            std::cmp::Reverse(self.len),
            self.origin_match_id,
        )
            .cmp(&(o.dst, o.src, std::cmp::Reverse(o.len), o.origin_match_id))
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct SourceMatch(pub(crate) Match);
impl FixedRecord for SourceMatch {
    const KIND: u8 = 3;
    const ORDER: u8 = 5;
    const WIDTH: usize = 64;
    fn encode(self, m: RecordMeta, s: u64, b: &mut [u8]) -> Result<()> {
        self.0.encode(m, s, b)
    }
    fn decode(m: RecordMeta, s: u64, b: &[u8]) -> Result<Self> {
        Ok(Self(Match::decode(m, s, b)?))
    }
    fn compare(&self, o: &Self) -> Ordering {
        (self.0.src, self.0.dst, self.0.origin_match_id).cmp(&(
            o.0.src,
            o.0.dst,
            o.0.origin_match_id,
        ))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DpRow {
    pub(crate) gain: u64,
    pub(crate) covered: u64,
    pub(crate) count: u64,
    pub(crate) best_node: u64,
    pub(crate) include_parent: u64,
    pub(crate) include_depth: u64,
}
impl FixedRecord for DpRow {
    const KIND: u8 = 2;
    const ORDER: u8 = 6;
    const WIDTH: usize = 96;
    fn encode(self, m: RecordMeta, s: u64, b: &mut [u8]) -> Result<()> {
        if b.len() != 96 {
            return Err(Error::corrupt_record("DP width"));
        }
        b.fill(0);
        for (i, n) in [
            self.gain,
            self.covered,
            self.count,
            self.best_node,
            self.include_parent,
            self.include_depth,
        ]
        .into_iter()
        .enumerate()
        {
            put(b, 8 * i, n);
        }
        envelope(b, m, s, b"SREP-OFF-DP-v1\0");
        Ok(())
    }
    fn decode(m: RecordMeta, s: u64, b: &[u8]) -> Result<Self> {
        check(b, m, s, 96, b"SREP-OFF-DP-v1\0")?;
        if b[48..72] != [0; 24] {
            return Err(Error::corrupt_record("DP padding"));
        }
        Ok(Self {
            gain: u64_at(b, 0),
            covered: u64_at(b, 8),
            count: u64_at(b, 16),
            best_node: u64_at(b, 24),
            include_parent: u64_at(b, 32),
            include_depth: u64_at(b, 40),
        })
    }
    fn compare(&self, _: &Self) -> Ordering {
        Ordering::Equal
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReverseIndexRow {
    pub(crate) candidate_index: u64,
}
impl FixedRecord for ReverseIndexRow {
    const KIND: u8 = 4;
    const ORDER: u8 = 7;
    const WIDTH: usize = 32;
    fn encode(self, m: RecordMeta, s: u64, b: &mut [u8]) -> Result<()> {
        if b.len() != 32 {
            return Err(Error::corrupt_record("reverse width"));
        }
        b.fill(0);
        put(b, 0, self.candidate_index);
        envelope(b, m, s, b"SREP-OFF-REV-v1\0");
        Ok(())
    }
    fn decode(m: RecordMeta, s: u64, b: &[u8]) -> Result<Self> {
        check(b, m, s, 32, b"SREP-OFF-REV-v1\0")?;
        Ok(Self {
            candidate_index: u64_at(b, 0),
        })
    }
    fn compare(&self, o: &Self) -> Ordering {
        o.candidate_index.cmp(&self.candidate_index)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    meta: RecordMeta,
    width: u16,
    count: u64,
    payload: u64,
    input_len: u64,
    sealed: bool,
}
impl Header {
    fn encode(self) -> [u8; 128] {
        let mut b = [0; 128];
        b[..8].copy_from_slice(b"SRPOFF01");
        b[8..10].copy_from_slice(&1u16.to_le_bytes());
        b[10..12].copy_from_slice(&128u16.to_le_bytes());
        b[12..14].copy_from_slice(&self.width.to_le_bytes());
        b[14] = self.meta.kind;
        b[15] = self.meta.order;
        put(&mut b, 16, self.meta.generation);
        put(&mut b, 24, self.count);
        put(&mut b, 32, self.payload);
        put(&mut b, 40, u64::from(self.sealed));
        b[48..64].copy_from_slice(&self.meta.session.wire_tag());
        b[64..80].copy_from_slice(&self.meta.file_id);
        put(&mut b, 80, self.input_len);
        let sum = hash16(b"SREP-OFF-HDR-v1\0", &[&b[..112]]);
        b[112..].copy_from_slice(&sum);
        b
    }
    fn check(self, b: &[u8]) -> Result<()> {
        if b != self.encode() {
            return Err(Error::corrupt_index(
                "offload header identity/count/order/checksum changed",
            ));
        }
        Ok(())
    }
}

/// Owns one handle. Failed cleanup retains physical charges through B0 registry.
struct TableFile {
    file: Option<tempfile::NamedTempFile>,
    handle: Option<ResourceLease>,
    slot: Option<ResourceLease>,
    bytes: Option<Reservation>,
    directory: Arc<crate::offload::session::DirectoryOwner>,
    io: SessionIo,
    memory: MemoryBudget,
    length: u64,
    limit: u64,
    poisoned: bool,
    _control: Option<Reservation>,
    #[cfg(test)]
    io_test: Option<(usize, bool, bool)>,
}
impl TableFile {
    fn create(
        session: &CompressionSession,
        kind: u8,
        order: u8,
        width: usize,
        input: u64,
        emergency: bool,
    ) -> Result<(Self, Header)> {
        // Check all byte/control reservations before creation. Caller-owned
        // emergency guard covers the handle when emergency=true.
        let bytes = session.temp().reserve(0)?;
        let slot = session.acquire_file_slot()?;
        let handle = if emergency {
            None
        } else {
            Some(session.acquire_handles(1)?)
        };
        // directory_owner() resolves the absolute parent once; the returned Arc
        // keeps the owner alive for cleanup. Only fixed working control is
        // charged: the per-file cleanup owner slot, its box header, and the
        // allocator envelope. Pathname bytes are excluded as file-pathname
        // processing under the `--memory` contract, so this charge does not
        // depend on path length.
        let directory = session.directory_owner()?;
        let control = session
            .memory()
            .reserve(super::session::file_path_control())?;
        session.checkpoint(Operation::Create)?;
        let generation = session.next_generation()?;
        let mut file_id = [0; 16];
        getrandom::fill(&mut file_id).map_err(|e| Error::temp_storage_context(e.to_string()))?;
        let mut builder = tempfile::Builder::new();
        builder.prefix("table-").disable_cleanup(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o600));
        }
        let file = builder
            .tempfile_in(directory.path())
            .map_err(Error::temp_storage)?;
        directory.verify_contains(file.path())?;
        let mut f = Self {
            file: Some(file),
            handle,
            slot: Some(slot),
            bytes: Some(bytes),
            directory,
            io: session.io_access(),
            memory: session.memory().clone(),
            length: 0,
            limit: session.policy().max_file_bytes.min(session.temp().limit()),
            poisoned: false,
            _control: Some(control),
            #[cfg(test)]
            io_test: None,
        };
        let h = Header {
            meta: RecordMeta {
                session: session.identity(),
                generation,
                file_id,
                kind,
                order,
            },
            width: width as u16,
            count: 0,
            payload: 0,
            input_len: input,
            sealed: false,
        };
        f.write_at(0, &h.encode())?;
        Ok((f, h))
    }
    fn raw(&mut self) -> &mut std::fs::File {
        self.file.as_mut().expect("live file").as_file_mut()
    }
    fn write_syscall(&mut self, b: &[u8]) -> std::io::Result<usize> {
        #[cfg(test)]
        if let Some((max, interrupted, _)) = self.io_test.as_mut() {
            if !*interrupted {
                *interrupted = true;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let n = b.len().min(*max);
            return self.raw().write(&b[..n]);
        }
        self.raw().write(b)
    }
    fn read_syscall(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(test)]
        if let Some((max, _, interrupted)) = self.io_test.as_mut() {
            if !*interrupted {
                *interrupted = true;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let n = b.len().min(*max);
            return self.raw().read(&mut b[..n]);
        }
        self.raw().read(b)
    }
    fn write_at(&mut self, offset: u64, b: &[u8]) -> Result<()> {
        let result = self.write_at_inner(offset, b);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn write_at_inner(&mut self, offset: u64, b: &[u8]) -> Result<()> {
        if self.poisoned {
            return Err(Error::temp_storage_context("poisoned table"));
        }
        let end = offset
            .checked_add(b.len() as u64)
            .ok_or_else(|| Error::temp_limit("table offset overflow"))?;
        if end > self.limit {
            return Err(Error::temp_limit("table single-file limit"));
        }
        let mut permit = self.io.reserve_io(b.len() as u128)?;
        let growth = end.saturating_sub(self.length);
        self.bytes.as_mut().unwrap().grow(growth)?;
        let result = (|| {
            self.io.checkpoint(Operation::Seek)?;
            self.raw()
                .seek(SeekFrom::Start(offset))
                .map_err(Error::temp_storage)?;
            let mut n = 0;
            while n < b.len() {
                self.io.checkpoint(Operation::Write)?;
                let stop = (n + 16384).min(b.len());
                let k = match self.write_syscall(&b[n..stop]) {
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    r => r.map_err(Error::temp_storage)?,
                };
                if k == 0 {
                    return Err(Error::temp_storage_context("table write zero"));
                }
                permit.consume(k as u64)?;
                n += k;
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.poisoned = true;
            let old = self.length;
            if self.io.checkpoint(Operation::Truncate).is_ok() && self.raw().set_len(old).is_ok() {
                self.bytes.as_mut().unwrap().shrink(growth);
            }
            return Err(e);
        }
        self.length = self.length.max(end);
        Ok(())
    }
    fn read_at(&mut self, offset: u64, b: &mut [u8]) -> Result<()> {
        if self.poisoned {
            return Err(Error::corrupt_record("poisoned table read"));
        }
        if offset
            .checked_add(b.len() as u64)
            .is_none_or(|n| n > self.length)
        {
            return Err(Error::corrupt_index("table read bounds"));
        }
        let mut permit = self.io.reserve_io(b.len() as u128)?;
        self.io.checkpoint(Operation::Seek)?;
        self.raw()
            .seek(SeekFrom::Start(offset))
            .map_err(Error::temp_storage)?;
        let mut n = 0;
        while n < b.len() {
            self.io.checkpoint(Operation::Read)?;
            let k = match self.read_syscall(&mut b[n..]) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                r => r.map_err(Error::temp_storage)?,
            };
            if k == 0 {
                return Err(Error::corrupt_index("truncated offload file"));
            }
            permit.consume(k as u64)?;
            n += k;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if self.poisoned {
            return Err(Error::corrupt_record("poisoned table seal"));
        }
        self.io.checkpoint(Operation::Flush)?;
        self.raw().flush().map_err(Error::temp_storage)
    }
    fn verify_header(&mut self, h: Header) -> Result<()> {
        if self.raw().metadata().map_err(Error::temp_storage)?.len() != HEADER_LEN + h.payload {
            return Err(Error::corrupt_index("table physical length changed"));
        }
        let mut b = [0; 128];
        self.read_at(0, &mut b)?;
        h.check(&b)
    }
    fn verify_building(&mut self, h: Header) -> Result<()> {
        if self.poisoned || self.raw().metadata().map_err(Error::temp_storage)?.len() != self.length
        {
            return Err(Error::corrupt_index("unsealed file length changed"));
        }
        let mut b = [0; 128];
        self.read_at(0, &mut b)?;
        h.check(&b)
    }
    fn close_handle(mut self) -> Result<ClosedHandle> {
        let lease = self
            .handle
            .take()
            .ok_or_else(|| Error::corrupt_index("missing ordinary handle"))?;
        let named = self.file.take().unwrap();
        let (file, path) = named.into_parts();
        let token = lease.close_file(file);
        // Transfer the pre-owned cleanup path with no allocation: TempPath is
        // closed (removing the file) and its boxed path is moved into the
        // cleanup entry only when removal fails.
        self.remove_or_defer_temppath(path);
        Ok(token)
    }
    fn remove_or_defer_temppath(&mut self, path: tempfile::TempPath) {
        if self.io.checkpoint(Operation::Remove).is_ok() && std::fs::remove_file(&path).is_ok() {
            return;
        }
        let recovered: std::path::PathBuf = match path.keep() {
            Ok(owned) => owned,
            Err(failed) => failed.path.to_path_buf(),
        };
        self.defer_owned_path(recovered);
    }
    /// The caller transfers ownership of an already-owned path; no
    /// allocation occurs here. The fixed `_control` reservation moves with the
    /// deferred cleanup entry so its working-state charge stays live until the
    /// file is verified removed.
    fn defer_owned_path(&mut self, path: std::path::PathBuf) {
        self.io.defer_cleanup_with_metadata(
            CleanupEntry {
                path,
                bytes: self.bytes.take().unwrap(),
                file: self.slot.take().unwrap(),
                directory: self.directory.clone(),
            },
            self._control.take().expect("live table control"),
        );
    }
}
impl Drop for TableFile {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            // Transfer the owned path with no allocation: the TempPath is
            // closed (removing the file) and its boxed path moves into the
            // cleanup entry only when removal fails.
            let path = file.into_temp_path();
            #[cfg(test)]
            if self.handle.is_none() {
                self.io
                    .note_lifecycle(super::session::LifecycleEvent::EmergencyFileClosed);
            }
            self.handle.take();
            self.remove_or_defer_temppath(path);
        }
    }
}

pub(crate) struct TypedTableBuilder<T: FixedRecord> {
    file: TableFile,
    header: Header,
    count: u64,
    previous: Option<T>,
    payload_hash: blake3::Hasher,
}
impl<T: FixedRecord> TypedTableBuilder<T> {
    fn active(&self) -> Result<()> {
        if self.file.poisoned {
            return Err(Error::temp_storage_context("failed table builder"));
        }
        // This also observes cancellation on cache hits without reading disk.
        drop(self.file.io.reserve_io(0)?);
        Ok(())
    }
    fn terminal<R>(&mut self, operation: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        let result = self.active().and_then(|_| operation(self));
        if result.is_err() {
            self.file.poisoned = true;
        }
        result
    }
    pub(crate) fn committed_len(&self) -> u64 {
        self.count
    }
    /// Bounds errors are terminal, not an end-of-stream indication. W1 checks
    /// committed_len before requesting prior DP rows.
    pub(crate) fn read_committed_cached(
        &mut self,
        index: u64,
        cache: &mut RecordPageCache,
    ) -> Result<T> {
        self.terminal(|this| {
            let payload = this
                .count
                .checked_mul(T::WIDTH as u64)
                .ok_or_else(|| Error::corrupt_index("committed payload overflow"))?;
            cache.read_prefix::<T>(
                &mut this.file,
                this.header,
                this.count,
                payload,
                false,
                index,
            )
        })
    }
    pub(crate) fn new(s: &CompressionSession, input: u64) -> Result<Self> {
        Self::create(s, input, false)
    }
    fn create(s: &CompressionSession, input: u64, emergency: bool) -> Result<Self> {
        if T::WIDTH > 96 || T::WIDTH < 24 {
            return Err(Error::invalid_config("unsupported record width"));
        }
        let (file, header) = TableFile::create(s, T::KIND, T::ORDER, T::WIDTH, input, emergency)?;
        Ok(Self {
            file,
            header,
            count: 0,
            previous: None,
            payload_hash: blake3::Hasher::new(),
        })
    }
    pub(super) fn emergency(s: &CompressionSession, input: u64) -> Result<Self> {
        Self::create(s, input, true)
    }
}
impl<T: FixedRecord> TableBuilder<T> for TypedTableBuilder<T> {
    type Store = RecordTable<T>;
    fn append(&mut self, row: T) -> Result<u64> {
        self.terminal(|this| this.append_inner(row))
    }
    #[cfg(test)]
    fn read_committed(&mut self, i: u64) -> Result<T> {
        self.terminal(|this| this.read_committed_inner(i))
    }
    fn finish(mut self) -> Result<RecordTable<T>> {
        self.active()?;
        self.file.verify_building(self.header)?;
        self.header.count = self.count;
        self.header.payload = self
            .count
            .checked_mul(T::WIDTH as u64)
            .ok_or_else(|| Error::temp_limit("table payload overflow"))?;
        self.header.sealed = true;
        self.file.write_at(0, &self.header.encode())?;
        self.file.flush()?;
        let t = RecordTable {
            file: RefCell::new(self.file),
            header: self.header,
            payload_digest: *self.payload_hash.finalize().as_bytes(),
            cursors: Cell::new(0),
            marker: std::marker::PhantomData,
        };
        t.verify_uncached()?;
        Ok(t)
    }
}
impl<T: FixedRecord> TypedTableBuilder<T> {
    fn append_inner(&mut self, row: T) -> Result<u64> {
        if self.previous.is_some_and(|p| p.compare(&row).is_gt()) {
            self.file.poisoned = true;
            return Err(Error::corrupt_index("unordered table append"));
        }
        let next = self
            .count
            .checked_add(1)
            .ok_or_else(|| Error::temp_limit("row count overflow"))?;
        let offset = record_offset(self.count, T::WIDTH)?;
        let mut b = [0; 96];
        row.encode(self.header.meta, self.count, &mut b[..T::WIDTH])?;
        self.file.write_at(offset, &b[..T::WIDTH])?;
        self.payload_hash.update(&b[..T::WIDTH]);
        self.previous = Some(row);
        self.count = next;
        Ok(next - 1)
    }
    #[cfg(test)]
    fn read_committed_inner(&mut self, i: u64) -> Result<T> {
        if i >= self.count {
            return Err(Error::corrupt_index("uncommitted table row"));
        }
        self.file.verify_building(self.header)?;
        let mut b = [0; 96];
        self.file
            .read_at(record_offset(i, T::WIDTH)?, &mut b[..T::WIDTH])?;
        T::decode(self.header.meta, i, &b[..T::WIDTH])
    }
}
fn record_offset(i: u64, w: usize) -> Result<u64> {
    i.checked_mul(w as u64)
        .and_then(|n| n.checked_add(HEADER_LEN))
        .ok_or_else(|| Error::corrupt_index("row offset overflow"))
}
pub(crate) struct RecordTable<T: FixedRecord> {
    file: RefCell<TableFile>,
    header: Header,
    payload_digest: [u8; 32],
    cursors: Cell<u8>,
    marker: std::marker::PhantomData<T>,
}

/// Explicit shared fixed page cache for W1 random reads. Cache ownership is
/// outside tables: N live runs never allocate N caches. Payload pages begin at
/// offset128; a96-byte DP row may straddle two pages and is assembled before its
/// identity/sequence/checksum is checked. Eviction never changes row semantics.
pub(crate) struct RecordPageCache {
    bytes: BudgetedVec<u8>,
    page_len: usize,
    tags: [Option<(RecordMeta, u64, usize, bool)>; 64],
    _metadata: Reservation,
}
impl RecordPageCache {
    pub(crate) fn new(memory: &MemoryBudget, pages: usize) -> Result<Self> {
        if !(2..=64).contains(&pages) {
            return Err(Error::invalid_config("record cache requires2..64 pages"));
        }
        let metadata = memory.reserve(std::mem::size_of::<Self>() as u64)?;
        let page_len = (memory.limit() / 64).clamp(256, PAGE_LEN as u64) as usize;
        let mut bytes = BudgetedVec::with_capacity(pages * page_len, memory)?;
        bytes.resize(pages * page_len, 0)?;
        Ok(Self {
            bytes,
            page_len,
            tags: [None; 64],
            _metadata: metadata,
        })
    }
    pub(crate) fn read<T: FixedRecord>(&mut self, table: &RecordTable<T>, index: u64) -> Result<T> {
        let mut file = table
            .file
            .try_borrow_mut()
            .map_err(|_| Error::corrupt_index("cache file borrow"))?;
        self.read_prefix::<T>(
            &mut file,
            table.header,
            table.len(),
            table.header.payload,
            true,
            index,
        )
    }
    fn read_prefix<T: FixedRecord>(
        &mut self,
        file: &mut TableFile,
        header: Header,
        count: u64,
        payload: u64,
        sealed: bool,
        index: u64,
    ) -> Result<T> {
        if file.poisoned {
            return Err(Error::temp_storage_context("failed cached table"));
        }
        drop(file.io.reserve_io(0)?);
        if index >= count {
            return Err(Error::corrupt_index("cached row out of bounds"));
        }
        let start = index
            .checked_mul(T::WIDTH as u64)
            .ok_or_else(|| Error::corrupt_index("cached row offset overflow"))?;
        let mut row = [0; 96];
        let mut copied = 0;
        while copied < T::WIDTH {
            let at = start + copied as u64;
            let page = at / self.page_len as u64;
            let offset = (at % self.page_len as u64) as usize;
            let slot = (page % (self.bytes.len() / self.page_len) as u64) as usize;
            let valid = (payload - page * self.page_len as u64).min(self.page_len as u64) as usize;
            // A growing partial page changes valid, forcing a fresh read. Full
            // immutable prefix pages remain hits. Seal state forces boundary
            // revalidation even if count/payload happen to be unchanged.
            let key = (header.meta, page, valid, sealed);
            let buffer =
                &mut self.bytes.as_mut_slice()[slot * self.page_len..(slot + 1) * self.page_len];
            if self.tags[slot] != Some(key) {
                let retained = match self.tags[slot] {
                    Some((meta, old_page, old_valid, old_sealed))
                        if meta == header.meta
                            && old_page == page
                            && old_sealed == sealed
                            && old_valid < valid =>
                    {
                        old_valid
                    }
                    _ => 0,
                };
                self.tags[slot] = None;
                if sealed {
                    file.verify_header(header)?;
                } else {
                    file.verify_building(header)?;
                }
                file.read_at(
                    HEADER_LEN + page * self.page_len as u64 + retained as u64,
                    &mut buffer[retained..valid],
                )?;
                self.tags[slot] = Some(key);
            }
            let take = (T::WIDTH - copied).min(valid - offset);
            row[copied..copied + take].copy_from_slice(&buffer[offset..offset + take]);
            copied += take;
        }
        T::decode(header.meta, index, &row[..T::WIDTH])
    }
}
pub(crate) type CandidateTableBuilder = TypedTableBuilder<CandidateRow>;
pub(crate) type CandidateTableStore = RecordTable<CandidateRow>;
impl<T: FixedRecord> RecordTable<T> {
    #[cfg(test)]
    pub(crate) fn identity(&self) -> StoreIdentity {
        StoreIdentity {
            session: self.header.meta.session,
            generation: self.header.meta.generation,
        }
    }
    pub(crate) fn input_len(&self) -> u64 {
        self.header.input_len
    }
    pub(crate) fn check_session(&self, s: &CompressionSession) -> Result<()> {
        if self.header.meta.session != s.identity() {
            return Err(Error::corrupt_index("cross-session table"));
        }
        Ok(())
    }
    pub(crate) fn verify_uncached(&self) -> Result<()> {
        let mut f = self
            .file
            .try_borrow_mut()
            .map_err(|_| Error::corrupt_index("table already borrowed"))?;
        f.verify_header(self.header)?;
        let mut hash = blake3::Hasher::new();
        let mut previous: Option<T> = None;
        let records = (f.memory.limit() / 4096 / T::WIDTH as u64).clamp(1, 1024) as usize;
        let mut buffer = BudgetedVec::with_capacity(records * T::WIDTH, &f.memory)?;
        buffer.resize(records * T::WIDTH, 0)?;
        let mut i = 0;
        while i < self.header.count {
            let count = (self.header.count - i).min(records as u64) as usize;
            f.read_at(
                record_offset(i, T::WIDTH)?,
                &mut buffer.as_mut_slice()[..count * T::WIDTH],
            )?;
            hash.update(&buffer.as_slice()[..count * T::WIDTH]);
            for encoded in buffer.as_slice()[..count * T::WIDTH].chunks_exact(T::WIDTH) {
                let row = T::decode(self.header.meta, i, encoded)?;
                if previous.is_some_and(|p| p.compare(&row).is_gt()) {
                    return Err(Error::corrupt_index("table order changed"));
                }
                previous = Some(row);
                i += 1;
            }
        }
        if hash.finalize().as_bytes() != &self.payload_digest {
            return Err(Error::corrupt_index("table differs from writer digest"));
        }
        Ok(())
    }
    pub(super) fn retire(self) -> Result<ClosedHandle> {
        self.verify_uncached()?;
        self.file.into_inner().close_handle()
    }
    pub(super) fn adopt(&mut self, lease: ResourceLease) {
        self.file.get_mut().handle = Some(lease);
    }
}
impl<T: FixedRecord> ReplayStore<T> for RecordTable<T> {
    fn len(&self) -> u64 {
        self.header.count
    }
    fn cursor(&self) -> Result<Box<dyn RecordCursor<T> + '_>> {
        if self.cursors.get() >= 4 {
            return Err(Error::memory_limit("table cursor limit"));
        }
        let reservation = self.file.borrow().memory.reserve(128)?;
        let memory = self.file.borrow().memory.clone();
        let records = (memory.limit() / 4096 / T::WIDTH as u64).clamp(1, 1024) as usize;
        let mut buffer = BudgetedVec::with_capacity(records * T::WIDTH, &memory)?;
        buffer.resize(records * T::WIDTH, 0)?;
        self.file.borrow_mut().verify_header(self.header)?;
        self.cursors.set(self.cursors.get() + 1);
        Ok(Box::new(TableCursor {
            table: self,
            position: 0,
            _reservation: reservation,
            buffer,
            start: 0,
            loaded: 0,
        }))
    }
}
impl<T: FixedRecord> RandomStore<T> for RecordTable<T> {
    fn read_at(&self, i: u64) -> Result<T> {
        if i >= self.len() {
            return Err(Error::corrupt_index("table row out of bounds"));
        }
        let mut f = self
            .file
            .try_borrow_mut()
            .map_err(|_| Error::corrupt_index("table already borrowed"))?;
        f.verify_header(self.header)?;
        let mut b = [0; 96];
        f.read_at(record_offset(i, T::WIDTH)?, &mut b[..T::WIDTH])?;
        T::decode(self.header.meta, i, &b[..T::WIDTH])
    }
}
struct TableCursor<'a, T: FixedRecord> {
    table: &'a RecordTable<T>,
    position: u64,
    _reservation: Reservation,
    buffer: BudgetedVec<u8>,
    start: u64,
    loaded: u64,
}
impl<T: FixedRecord> Drop for TableCursor<'_, T> {
    fn drop(&mut self) {
        self.table.cursors.set(self.table.cursors.get() - 1);
    }
}
impl<T: FixedRecord> RecordCursor<T> for TableCursor<'_, T> {
    fn position(&self) -> u64 {
        self.position
    }
    fn seek_row(&mut self, p: u64) -> Result<()> {
        if p > self.table.len() {
            return Err(Error::corrupt_index("cursor seek beyond end"));
        }
        self.position = p;
        Ok(())
    }
    fn next(&mut self) -> Result<Option<T>> {
        if self.position == self.table.len() {
            return Ok(None);
        }
        if self.buffer.len() == T::WIDTH {
            let row = self.table.read_at(self.position)?;
            self.position += 1;
            return Ok(Some(row));
        }
        if self.position < self.start || self.position >= self.start + self.loaded {
            let count =
                (self.table.len() - self.position).min((self.buffer.len() / T::WIDTH) as u64);
            let mut file = self.table.file.borrow_mut();
            file.verify_header(self.table.header)?;
            file.read_at(
                record_offset(self.position, T::WIDTH)?,
                &mut self.buffer.as_mut_slice()[..count as usize * T::WIDTH],
            )?;
            self.start = self.position;
            self.loaded = count;
        }
        let offset = (self.position - self.start) as usize * T::WIDTH;
        let row = T::decode(
            self.table.header.meta,
            self.position,
            &self.buffer.as_slice()[offset..offset + T::WIDTH],
        )?;
        self.position += 1;
        Ok(Some(row))
    }
}

/// One shared source cache for all runs in an I1 index. Key includes full file
/// identity. No per-run 4 MiB allocation; misses reuse the selected page buffer.
pub(crate) struct SourcePageCache<'a> {
    session: SessionId,
    bytes: SourceLoan<'a, u8>,
    tags: [Option<(RecordMeta, u64)>; 64],
    _tags: Reservation,
}
impl<'a> SourcePageCache<'a> {
    pub(crate) fn new(s: &'a CompressionSession) -> Result<Self> {
        let mut bytes = s.source_cache()?;
        let len = (bytes.capacity() / PAGE_LEN).min(64) * PAGE_LEN;
        if len < PAGE_LEN {
            return Err(Error::memory_limit("source page cache minimum"));
        }
        bytes.resize(len, 0)?;
        Ok(Self {
            session: s.identity(),
            bytes,
            tags: [None; 64],
            _tags: s.memory().reserve(std::mem::size_of::<Self>() as u64)?,
        })
    }
    pub(crate) fn read_entry(&mut self, table: &PagedSourceTable, i: u64) -> Result<IndexEntry> {
        if table.header.meta.session != self.session {
            return Err(Error::corrupt_index("cross-session source cache"));
        }
        if i >= table.header.count {
            return Err(Error::corrupt_index("source entry bounds"));
        }
        let page = i / SOURCE_PAGE_ENTRIES as u64;
        let slot = (page % (self.bytes.len() / PAGE_LEN) as u64) as usize;
        let tag = (table.header.meta, page);
        let b = &mut self.bytes.as_mut_slice()[slot * PAGE_LEN..(slot + 1) * PAGE_LEN];
        if self.tags[slot] != Some(tag) {
            self.tags[slot] = None;
            table.read_page(page, b)?;
            self.tags[slot] = Some(tag);
        }
        decode_entry(b, (i % SOURCE_PAGE_ENTRIES as u64) as usize)
    }
}
fn decode_entry(b: &[u8], slot: usize) -> Result<IndexEntry> {
    let start = 32 + slot * SOURCE_WIDTH;
    IndexEntry::decode_record(&b[start..start + SOURCE_WIDTH])
        .map_err(|e| Error::corrupt_index(e.to_string()))
}
fn page_checksum(meta: RecordMeta, b: &[u8]) -> [u8; 16] {
    hash16(
        b"SREP-OFF-SOURCE-PAGE-v1\0",
        &[
            &meta.session.wire_tag(),
            &meta.file_id,
            &meta.generation.to_le_bytes(),
            &b[..PAGE_LEN - 16],
        ],
    )
}
fn validate_page(h: Header, p: u64, b: &[u8]) -> Result<()> {
    let first = p
        .checked_mul(SOURCE_PAGE_ENTRIES as u64)
        .ok_or_else(|| Error::corrupt_index("page offset overflow"))?;
    let valid = h
        .count
        .checked_sub(first)
        .ok_or_else(|| Error::corrupt_index("page beyond count"))?
        .min(SOURCE_PAGE_ENTRIES as u64);
    if b.len() != PAGE_LEN
        || valid == 0
        || u64_at(b, 0) != p
        || u64_at(b, 8) != first
        || u64_at(b, 16) != valid
        || u64_at(b, 24) != 0
        || b[PAGE_LEN - 16..] != page_checksum(h.meta, b)
    {
        return Err(Error::corrupt_index("source page prefix/checksum"));
    }
    if b[32 + valid as usize * SOURCE_WIDTH..PAGE_LEN - 16]
        .iter()
        .any(|n| *n != 0)
    {
        return Err(Error::corrupt_index("source page padding"));
    }
    let mut previous = None;
    for i in 0..valid as usize {
        let e = decode_entry(b, i)?;
        if previous.is_some_and(|p| index_entry_order(&p, &e).is_gt()) {
            return Err(Error::corrupt_index("source page order"));
        }
        previous = Some(e);
    }
    Ok(())
}
pub(crate) struct SourcePageBuilder {
    file: TableFile,
    header: Header,
    page: BudgetedVec<u8>,
    valid: usize,
    pages: u64,
    count: u64,
    previous: Option<IndexEntry>,
    hash: blake3::Hasher,
}
impl SourcePageBuilder {
    fn active(&self) -> Result<()> {
        if self.file.poisoned {
            return Err(Error::temp_storage_context("failed source page builder"));
        }
        drop(self.file.io.reserve_io(0)?);
        Ok(())
    }
    pub(crate) fn new(s: &CompressionSession, input: u64) -> Result<Self> {
        Self::create(s, input, false)
    }
    /// Caller retains B0 emergency guard until output is dropped or adopted.
    pub(crate) fn emergency(s: &CompressionSession, input: u64) -> Result<Self> {
        Self::create(s, input, true)
    }
    fn create(s: &CompressionSession, input: u64, emergency: bool) -> Result<Self> {
        let mut page = BudgetedVec::with_capacity(PAGE_LEN, s.memory())?;
        page.resize(PAGE_LEN, 0)?;
        let (file, header) = TableFile::create(s, 5, 8, 104, input, emergency)?;
        Ok(Self {
            file,
            header,
            page,
            valid: 0,
            pages: 0,
            count: 0,
            previous: None,
            hash: blake3::Hasher::new(),
        })
    }
    pub(crate) fn append(&mut self, e: IndexEntry) -> Result<u64> {
        let count = self.count;
        let previous = self.previous;
        let valid = self.valid;
        let result = self.active().and_then(|_| self.append_inner(e));
        if result.is_err() {
            self.file.poisoned = true;
            // In-memory page bytes may be contaminated; only logical committed
            // state is restored. No path can flush/reuse this builder again.
            self.count = count;
            self.previous = previous;
            self.valid = valid;
        }
        result
    }
    fn append_inner(&mut self, e: IndexEntry) -> Result<u64> {
        if self
            .previous
            .is_some_and(|p| index_entry_order(&p, &e).is_gt())
        {
            self.file.poisoned = true;
            return Err(Error::corrupt_index("unordered source append"));
        }
        let next = self
            .count
            .checked_add(1)
            .ok_or_else(|| Error::temp_limit("source count overflow"))?;
        let b = e.encode_record()?;
        let at = 32 + self.valid * 104;
        self.page.as_mut_slice()[at..at + 104].copy_from_slice(&b);
        self.valid += 1;
        self.count = next;
        self.previous = Some(e);
        if self.valid == SOURCE_PAGE_ENTRIES {
            self.flush_page()?;
        }
        Ok(next - 1)
    }
    fn flush_page(&mut self) -> Result<()> {
        self.active()?;
        if self.valid == 0 {
            return Ok(());
        }
        let next_page = self
            .pages
            .checked_add(1)
            .ok_or_else(|| Error::temp_limit("source page count overflow"))?;
        let b = self.page.as_mut_slice();
        put(b, 0, self.pages);
        put(
            b,
            8,
            self.pages
                .checked_mul(629)
                .ok_or_else(|| Error::temp_limit("source page row overflow"))?,
        );
        put(b, 16, self.valid as u64);
        let sum = page_checksum(self.header.meta, b);
        b[PAGE_LEN - 16..].copy_from_slice(&sum);
        self.file
            .write_at(record_offset(self.pages, PAGE_LEN)?, b)?;
        self.hash.update(b);
        b.fill(0);
        self.valid = 0;
        self.pages = next_page;
        Ok(())
    }
    pub(crate) fn finish(mut self) -> Result<PagedSourceTable> {
        self.active()?;
        self.flush_page()?;
        self.file.verify_building(self.header)?;
        self.header.count = self.count;
        self.header.payload = self
            .pages
            .checked_mul(PAGE_LEN as u64)
            .ok_or_else(|| Error::temp_limit("source payload overflow"))?;
        self.header.sealed = true;
        self.file.write_at(0, &self.header.encode())?;
        self.file.flush()?;
        let table = PagedSourceTable {
            file: RefCell::new(self.file),
            header: self.header,
            digest: *self.hash.finalize().as_bytes(),
            scratch: RefCell::new(self.page),
        };
        table.verify_uncached_all()?;
        Ok(table)
    }
}
pub(crate) struct PagedSourceTable {
    file: RefCell<TableFile>,
    header: Header,
    digest: [u8; 32],
    scratch: RefCell<BudgetedVec<u8>>,
}
impl PagedSourceTable {
    pub(crate) fn retire(self) -> Result<ClosedHandle> {
        #[cfg(test)]
        self.file
            .borrow()
            .io
            .note_lifecycle(super::session::LifecycleEvent::RetireVerify);
        self.file.borrow().io.checkpoint(Operation::RetireVerify)?;
        self.verify_uncached_all()?;
        self.file.into_inner().close_handle()
    }
    pub(crate) fn adopt(&mut self, lease: ResourceLease) {
        self.file.get_mut().handle = Some(lease);
    }
    pub(crate) fn verify_envelope(&self) -> Result<()> {
        self.file.borrow_mut().verify_header(self.header)
    }
    fn read_page(&self, p: u64, b: &mut [u8]) -> Result<()> {
        if p >= self.header.count.div_ceil(629) {
            return Err(Error::corrupt_index("source page bounds"));
        }
        self.file
            .borrow_mut()
            .read_at(record_offset(p, PAGE_LEN)?, b)?;
        validate_page(self.header, p, b)
    }
    pub(crate) fn verify_uncached_all(&self) -> Result<()> {
        self.verify_envelope()?;
        let mut b = self
            .scratch
            .try_borrow_mut()
            .map_err(|_| Error::corrupt_index("source scratch borrowed"))?;
        let mut h = blake3::Hasher::new();
        let mut previous = None;
        for p in 0..self.header.count.div_ceil(629) {
            self.read_page(p, b.as_mut_slice())?;
            h.update(b.as_slice());
            for i in 0..u64_at(b.as_slice(), 16) as usize {
                let e = decode_entry(b.as_slice(), i)?;
                if previous.is_some_and(|old| index_entry_order(&old, &e).is_gt()) {
                    return Err(Error::corrupt_index("source global order"));
                }
                previous = Some(e);
            }
        }
        if h.finalize().as_bytes() != &self.digest {
            return Err(Error::corrupt_index("source writer digest mismatch"));
        }
        Ok(())
    }
}
impl VerifiedSourcePages for PagedSourceTable {
    #[cfg(test)]
    fn identity(&self) -> StoreIdentity {
        StoreIdentity {
            session: self.header.meta.session,
            generation: self.header.meta.generation,
        }
    }
    fn entry_count(&self) -> u64 {
        self.header.count
    }
    #[cfg(test)]
    fn read_entry(&mut self, i: u64) -> Result<IndexEntry> {
        if i >= self.header.count {
            return Err(Error::corrupt_index("source entry bounds"));
        }
        let mut b = self.scratch.borrow_mut();
        self.read_page(i / 629, b.as_mut_slice())?;
        decode_entry(b.as_slice(), (i % 629) as usize)
    }
    fn verify_uncached(&mut self) -> Result<()> {
        self.verify_uncached_all()
    }
}

#[cfg(test)]
mod tests {
    use super::super::session::{FaultAction, OffloadPolicy};
    use super::*;
    use crate::config::ResourceConfig;
    use crate::resource::ResourceContext;
    fn setup() -> (ResourceContext, CompressionSession) {
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        (c, s)
    }
    fn row(n: u64) -> CandidateRow {
        CandidateRow {
            candidate: crate::match_ir::MatchCandidate {
                src: n,
                dst: 100 + n,
                len: 30,
                insertion_ordinal: n,
            },
            provenance: Provenance::EXTERNAL,
        }
    }
    fn source(n: u64) -> IndexEntry {
        IndexEntry::new(0, &0u64.to_le_bytes(), 10_000 - n, n, &[]).unwrap()
    }
    fn raw_mutate(file: &RefCell<TableFile>, offset: u64, bytes: &[u8]) {
        let mut f = file.borrow_mut();
        f.raw().seek(SeekFrom::Start(offset)).unwrap();
        f.raw().write_all(bytes).unwrap();
    }
    #[test]
    fn all_record_encodings_bind_identity_sequence_and_padding() {
        let (_, s) = setup();
        let meta = RecordMeta {
            session: s.identity(),
            generation: 3,
            file_id: [7; 16],
            kind: 1,
            order: 1,
        };
        fn check_row<T: FixedRecord>(row: T, mut m: RecordMeta) {
            m.kind = T::KIND;
            m.order = T::ORDER;
            let mut b = [0; 96];
            row.encode(m, 5, &mut b[..T::WIDTH]).unwrap();
            T::decode(m, 5, &b[..T::WIDTH]).unwrap();
            assert!(T::decode(m, 4, &b[..T::WIDTH]).is_err());
            let mut other = m;
            other.generation += 1;
            assert!(T::decode(other, 5, &b[..T::WIDTH]).is_err());
            other = m;
            other.file_id[0] ^= 1;
            assert!(T::decode(other, 5, &b[..T::WIDTH]).is_err());
            b[0] ^= 1;
            assert!(T::decode(m, 5, &b[..T::WIDTH]).is_err());
        }
        check_row(row(1), meta);
        check_row(IntervalRow(row(1)), meta);
        check_row(EndRow(row(1)), meta);
        let m = Match {
            src: 0,
            dst: 100,
            len: 30,
            origin_match_id: 0,
        };
        check_row(m, meta);
        check_row(SourceMatch(m), meta);
        check_row(
            DpRow {
                gain: 5,
                covered: 30,
                count: 1,
                best_node: 0,
                include_parent: u64::MAX,
                include_depth: 1,
            },
            meta,
        );
        check_row(ReverseIndexRow { candidate_index: 7 }, meta);
    }
    #[test]
    fn header_count_order_identity_and_physical_length_are_checked() {
        let (c, s) = setup();
        for offset in [0, 8, 10, 12, 14, 15, 16, 24, 32, 40, 48, 64, 80, 88, 112] {
            let mut b = CandidateTableBuilder::new(&s, 123).unwrap();
            b.append(row(0)).unwrap();
            let t = b.finish().unwrap();
            raw_mutate(&t.file, offset, &[255]);
            assert!(t.read_at(0).is_err());
        }
        for len in [127, 191, 193] {
            let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
            b.append(row(0)).unwrap();
            let t = b.finish().unwrap();
            t.file.borrow_mut().raw().set_len(len).unwrap();
            assert!(t.verify_uncached().is_err());
        }
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.file_usage().current, 0);
    }
    #[test]
    fn builder_does_not_bless_mutation_and_rejects_unsorted_rows() {
        let (_, s) = setup();
        let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
        b.append(row(0)).unwrap();
        let mut bytes = [0; 64];
        row(1).encode(b.header.meta, 0, &mut bytes).unwrap();
        b.file.raw().seek(SeekFrom::Start(128)).unwrap();
        b.file.raw().write_all(&bytes).unwrap();
        // Valid checksum replacement still differs from the append-side digest.
        assert!(b.finish().is_err());
        let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
        b.append(row(2)).unwrap();
        assert!(b.append(row(1)).is_err());
        assert!(b.finish().is_err());
        let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
        assert!(b.read_committed(0).is_err());
        assert!(b.finish().is_err());
        let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
        b.append(row(0)).unwrap();
        assert_eq!(b.read_committed(0).unwrap(), row(0));
        b.file.raw().seek(SeekFrom::Start(128)).unwrap();
        b.file.raw().write_all(&[44]).unwrap();
        assert!(b.read_committed(0).is_err());
    }
    #[test]
    fn dp_cross_page_cache_replay_and_cursor_limit() {
        let (_, s) = setup();
        let mut b = TypedTableBuilder::<DpRow>::new(&s, 0).unwrap();
        for i in 0..1400 {
            b.append(DpRow {
                gain: i,
                covered: i,
                count: 0,
                best_node: u64::MAX,
                include_parent: u64::MAX,
                include_depth: 0,
            })
            .unwrap();
        }
        let t = b.finish().unwrap();
        let mut cache = RecordPageCache::new(s.memory(), 2).unwrap();
        for i in [682, 683, 1365, 0, 1399] {
            assert_eq!(cache.read(&t, i).unwrap().gain, i);
        }
        let mut a = t.cursor().unwrap();
        let _b = t.cursor().unwrap();
        let _c = t.cursor().unwrap();
        let _d = t.cursor().unwrap();
        assert!(t.cursor().is_err());
        a.seek_row(1400).unwrap();
        assert!(a.next().unwrap().is_none());
        assert!(a.seek_row(1401).is_err());
        assert!(cache.read(&t, 1400).is_err());
    }
    #[test]
    fn source_pages_cached_and_uncached_barrier_detect_untouched_corruption() {
        let (_, s) = setup();
        let mut b = SourcePageBuilder::new(&s, 0).unwrap();
        for i in 0..1300 {
            b.append(source(i)).unwrap();
        }
        let mut t = b.finish().unwrap();
        let mut cache = SourcePageCache::new(&s).unwrap();
        for i in [0, 628, 629, 1258, 1299] {
            assert_eq!(cache.read_entry(&t, i).unwrap(), source(i));
        }
        cache.read_entry(&t, 0).unwrap(); // page2 evicted page0 in the two-page policy
        let before = s.io_used();
        cache.read_entry(&t, 0).unwrap();
        assert_eq!(s.io_used(), before);
        raw_mutate(&t.file, 128 + 65536 + 50, &[255]);
        assert!(cache.read_entry(&t, 629).is_ok());
        assert!(t.verify_uncached().is_err());
        // A fresh read detects corruption even when an older cache snapshot was valid.
        assert!(t.read_entry(629).is_err());
        assert!(t.read_entry(1300).is_err());
    }
    #[test]
    fn source_page_count_padding_checksum_and_preseal_mutation() {
        let (_, s) = setup();
        let mut b = SourcePageBuilder::new(&s, 0).unwrap();
        for i in 0..629 {
            b.append(source(i)).unwrap();
        }
        b.file.raw().seek(SeekFrom::Start(128 + 80)).unwrap();
        b.file.raw().write_all(&[77]).unwrap();
        assert!(b.finish().is_err());
        let mut b = SourcePageBuilder::new(&s, 0).unwrap();
        b.append(source(0)).unwrap();
        let t = b.finish().unwrap();
        let mut bytes = [0; PAGE_LEN];
        t.file.borrow_mut().read_at(128, &mut bytes).unwrap();
        put(&mut bytes, 16, 2);
        let sum = page_checksum(t.header.meta, &bytes);
        bytes[PAGE_LEN - 16..].copy_from_slice(&sum);
        raw_mutate(&t.file, 128, &bytes);
        assert!(t.verify_uncached_all().is_err());
    }
    #[test]
    fn partial_io_failure_retains_growth_until_cleanup_and_poisoned_finish() {
        let (c, s) = setup();
        let mut b = SourcePageBuilder::new(&s, 0).unwrap();
        let before = s.io_used();
        s.inject(Operation::Write, 1, FaultAction::Fail);
        s.inject(Operation::Truncate, 0, FaultAction::Fail);
        s.inject(Operation::Remove, 0, FaultAction::Fail);
        for i in 0..628 {
            b.append(source(i)).unwrap();
        }
        assert!(b.append(source(628)).is_err());
        assert_eq!(s.io_used() - before, 16384);
        assert_eq!(s.io_reserved(), 0);
        assert_eq!(c.temp.current(), 128 + 65536);
        assert!(b.finish().is_err());
        assert_eq!(s.file_usage().current, 1);
        assert_eq!(c.temp.current(), 128 + 65536);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
    }
    #[test]
    fn cross_session_and_resource_denial_cleanly_fail() {
        let (c, s) = setup();
        let t = CandidateTableBuilder::new(&s, 0).unwrap().finish().unwrap();
        let (_, other) = setup();
        assert!(t.check_session(&other).is_err());
        drop(t);
        let handles = s
            .acquire_handles(s.policy().max_handles - s.handle_usage().current)
            .unwrap();
        assert!(CandidateTableBuilder::new(&s, 0).is_err());
        drop(handles);
        assert_eq!(c.temp.current(), 0);
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            temp_limit: 127,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        assert!(CandidateTableBuilder::new(&s, 0).is_err());
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.file_usage().current, 0);
    }

    #[test]
    fn source_cache_keys_include_file_identity_and_barrier_reads_all_pages() {
        let (_, s) = setup();
        let mut a = SourcePageBuilder::new(&s, 0).unwrap();
        let mut b = SourcePageBuilder::new(&s, 0).unwrap();
        for i in 0..1300 {
            a.append(source(i)).unwrap();
            b.append(source(i + 100)).unwrap();
        }
        let a = a.finish().unwrap();
        let b = b.finish().unwrap();
        let mut cache = SourcePageCache::new(&s).unwrap();
        assert_eq!(cache.read_entry(&a, 0).unwrap(), source(0));
        assert_eq!(cache.read_entry(&b, 0).unwrap(), source(100));
        raw_mutate(&a.file, 128 + 2 * 65536 + 40, &[77]); // never queried third page
        assert!(a.verify_uncached_all().is_err());
        b.verify_uncached_all().unwrap();
        let mut encoded = [0; PAGE_LEN];
        b.file.borrow_mut().read_at(128, &mut encoded).unwrap();
        raw_mutate(&a.file, 128, &encoded);
        assert!(a.read_page(0, &mut encoded).is_err());
    }

    #[test]
    fn limits_io_failure_and_cancellation_release_owned_resources() {
        for mode in 0..4 {
            let r = ResourceConfig {
                memory: 8 * 1024 * 1024,
                ..ResourceConfig::default()
            };
            let c = ResourceContext::with_resources(&r).unwrap();
            let s = CompressionSession::new(
                &c,
                &r,
                super::super::session::OffloadPolicy {
                    max_temp_files: 3,
                    max_file_bytes: if mode == 0 { 128 } else { u64::MAX },
                    ..OffloadPolicy::default()
                },
            )
            .unwrap();
            if mode == 0 {
                let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
                assert!(b.append(row(0)).is_err());
                drop(b);
            }
            if mode == 1 {
                let a = CandidateTableBuilder::new(&s, 0).unwrap();
                let b = CandidateTableBuilder::new(&s, 0).unwrap();
                let d = CandidateTableBuilder::new(&s, 0).unwrap();
                assert!(CandidateTableBuilder::new(&s, 0).is_err());
                drop((a, b, d));
            }
            if mode == 2 {
                let held = c
                    .memory
                    .reserve(c.memory.limit() - c.memory.current())
                    .unwrap();
                assert!(CandidateTableBuilder::new(&s, 0).is_err());
                drop(held);
            }
            if mode == 3 {
                let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
                s.inject(Operation::Write, 0, FaultAction::Cancel);
                assert!(b.append(row(0)).is_err());
                assert!(b.finish().is_err());
            }
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), 0);
            assert_eq!(s.file_usage().current, 0);
            assert_eq!(s.handle_usage().current, 7);
        }
        let (_, s) = setup();
        let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
        b.append(row(0)).unwrap();
        let t = b.finish().unwrap();
        let before = s.io_used();
        s.inject(Operation::Read, 1, FaultAction::Fail);
        assert!(t.read_at(0).is_err());
        assert_eq!(s.io_used() - before, 128);
        assert_eq!(s.io_reserved(), 0);
    }

    #[test]
    fn short_interrupted_syscalls_are_counted_exactly_and_headers_not_blessed() {
        let (_, s) = setup();
        let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
        let before = s.io_used();
        b.file.io_test = Some((7, false, false));
        b.append(row(0)).unwrap();
        assert_eq!(s.io_used() - before, 64);
        let before = s.io_used();
        assert_eq!(b.read_committed(0).unwrap(), row(0));
        assert_eq!(s.io_used() - before, 128 + 64);
        let t = b.finish().unwrap();
        assert_eq!(t.read_at(0).unwrap(), row(0));
        let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
        b.append(row(0)).unwrap();
        b.file.raw().seek(SeekFrom::Start(24)).unwrap();
        b.file.raw().write_all(&[1]).unwrap();
        assert!(b.finish().is_err());
    }

    #[test]
    fn finite_io_and_small_source_override_do_not_bypass_storage_accounting() {
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let mut c = ResourceContext::with_resources(&r).unwrap();
        c.candidate_index_memtable_bytes = Some(0);
        let s = CompressionSession::new(
            &c,
            &r,
            OffloadPolicy {
                io_limit: super::super::session::IoLimit::Finite(127),
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        assert!(CandidateTableBuilder::new(&s, 0).is_err());
        assert_eq!(s.io_used(), 0);
        assert_eq!(s.io_reserved(), 0);
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);
        for cap in [0, 1] {
            let mut c = ResourceContext::with_resources(&r).unwrap();
            c.candidate_index_memtable_bytes = Some(cap);
            let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
            assert_eq!(s.candidate_index_memtable_bytes(), cap);
            let mut b = SourcePageBuilder::new(&s, 0).unwrap();
            b.append(source(0)).unwrap();
            let mut table = b.finish().unwrap();
            assert_eq!(table.read_entry(0).unwrap(), source(0));
            table.verify_uncached().unwrap();
            drop(table);
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), 0);
        }
    }

    #[test]
    fn all_early_append_failures_are_terminal_without_count_changes() {
        use super::super::session::IoLimit;
        for mode in 0..5 {
            let r = ResourceConfig {
                memory: 8 * 1024 * 1024,
                temp_limit: if mode == 2 { 128 } else { 1024 * 1024 },
                ..ResourceConfig::default()
            };
            let c = ResourceContext::with_resources(&r).unwrap();
            let s = CompressionSession::new(
                &c,
                &r,
                OffloadPolicy {
                    max_file_bytes: if mode == 0 { 128 } else { u64::MAX },
                    io_limit: if mode == 1 {
                        IoLimit::Finite(128)
                    } else {
                        IoLimit::Unlimited
                    },
                    ..OffloadPolicy::default()
                },
            )
            .unwrap();
            let mut b = CandidateTableBuilder::new(&s, 0).unwrap();
            if mode == 3 {
                s.cancel();
            }
            if mode == 4 {
                b.count = u64::MAX;
            }
            let old = b.count;
            let digest = *b.payload_hash.finalize().as_bytes();
            let error = b.append(row(0)).unwrap_err();
            assert_eq!(
                error.kind(),
                if mode == 0 || mode == 2 || mode == 4 {
                    crate::ErrorKind::TempBudgetExceeded
                } else {
                    crate::ErrorKind::TemporaryStorageFailure
                }
            );
            assert_eq!(b.count, old);
            assert_eq!(*b.payload_hash.finalize().as_bytes(), digest);
            for _ in 0..5 {
                assert!(b.append(row(0)).is_err());
                assert!(b.read_committed(0).is_err());
                assert_eq!(b.count, old);
            }
            assert!(b.finish().is_err());
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), 0);
            assert_eq!(s.file_usage().current, 0);
            assert_eq!(s.io_reserved(), 0);
        }
    }

    #[test]
    fn failed_full_source_page_never_accepts_more_rows_or_seals_prefix() {
        for mode in 0..4 {
            let r = ResourceConfig {
                memory: 8 * 1024 * 1024,
                ..ResourceConfig::default()
            };
            let c = ResourceContext::with_resources(&r).unwrap();
            let s = CompressionSession::new(
                &c,
                &r,
                OffloadPolicy {
                    max_file_bytes: if mode == 0 { 128 } else { u64::MAX },
                    ..OffloadPolicy::default()
                },
            )
            .unwrap();
            let mut b = SourcePageBuilder::new(&s, 0).unwrap();
            for i in 0..628 {
                b.append(source(i)).unwrap();
            }
            if mode == 1 {
                s.inject(Operation::Write, 1, FaultAction::Fail);
                s.inject(Operation::Truncate, 0, FaultAction::Fail);
            }
            if mode == 2 {
                s.cancel();
            }
            if mode == 3 {
                b.pages = u64::MAX;
            }
            let pages = b.pages;
            let hash = *b.hash.finalize().as_bytes();
            assert!(b.append(source(628)).is_err());
            assert_eq!(b.count, 628);
            assert_eq!(b.valid, 628);
            assert_eq!(b.pages, pages);
            assert_eq!(*b.hash.finalize().as_bytes(), hash);
            for i in 629..2000 {
                assert!(b.append(source(i)).is_err());
                assert_eq!(b.count, 628);
                assert_eq!(b.valid, 628);
            }
            if mode == 1 {
                assert_eq!(c.temp.current(), 128 + 65536);
            }
            assert!(b.finish().is_err());
            s.retry_cleanup().unwrap();
            assert_eq!(c.temp.current(), 0);
            assert_eq!(s.file_usage().current, 0);
        }
    }

    fn dp(n: u64) -> DpRow {
        DpRow {
            gain: n,
            covered: n,
            count: 0,
            best_node: u64::MAX,
            include_parent: u64::MAX,
            include_depth: 0,
        }
    }
    #[test]
    fn live_dp_cache_interleaves_appends_partial_growth_and_cross_page_rows() {
        let (_, s) = setup();
        let mut b = TypedTableBuilder::<DpRow>::new(&s, 0).unwrap();
        let mut cache = RecordPageCache::new(s.memory(), 2).unwrap();
        for i in 0..690 {
            b.append(dp(i)).unwrap();
            assert_eq!(b.committed_len(), i + 1);
            assert_eq!(b.read_committed_cached(i, &mut cache).unwrap(), dp(i));
            let before = s.io_used();
            assert_eq!(b.read_committed_cached(i, &mut cache).unwrap(), dp(i));
            assert_eq!(
                s.io_used(),
                before,
                "repeat hit must not reread header/page"
            );
            if i > 0 {
                assert_eq!(
                    b.read_committed_cached(i - 1, &mut cache).unwrap(),
                    dp(i - 1)
                );
            }
        }
        // Row682 straddles page0/page1; both pages are still addressable before seal.
        assert_eq!(b.read_committed_cached(682, &mut cache).unwrap(), dp(682));
        let before = s.io_used();
        assert_eq!(b.read_committed_cached(682, &mut cache).unwrap(), dp(682));
        assert_eq!(s.io_used(), before);
        let t = b.finish().unwrap();
        let before = s.io_used();
        assert_eq!(cache.read(&t, 682).unwrap(), dp(682));
        assert!(
            s.io_used() > before,
            "sealing is a cache identity-state boundary"
        );
        let mut other = TypedTableBuilder::<DpRow>::new(&s, 0).unwrap();
        other.append(dp(999)).unwrap();
        assert_eq!(other.read_committed_cached(0, &mut cache).unwrap(), dp(999));
    }

    #[test]
    fn live_cache_bounds_io_corruption_and_cancel_failures_poison_builder() {
        for mode in 0..5 {
            let (_, s) = setup();
            let mut b = TypedTableBuilder::<DpRow>::new(&s, 0).unwrap();
            b.append(dp(0)).unwrap();
            let mut cache = RecordPageCache::new(s.memory(), 2).unwrap();
            if mode == 0 {
                assert!(b.read_committed_cached(1, &mut cache).is_err());
            }
            if mode == 1 {
                s.inject(Operation::Read, 1, FaultAction::Fail);
                assert!(b.read_committed_cached(0, &mut cache).is_err());
            }
            if mode == 2 {
                b.file.raw().seek(SeekFrom::Start(128)).unwrap();
                b.file.raw().write_all(&[99]).unwrap();
                assert!(b.read_committed_cached(0, &mut cache).is_err());
            }
            if mode == 3 {
                b.read_committed_cached(0, &mut cache).unwrap();
                s.cancel();
                assert!(b.read_committed_cached(0, &mut cache).is_err());
            }
            if mode == 4 {
                assert!(b.read_committed(1).is_err());
            }
            assert_eq!(b.committed_len(), 1);
            assert!(b.append(dp(1)).is_err());
            assert!(b.read_committed(0).is_err());
            assert!(b.read_committed_cached(0, &mut cache).is_err());
            assert!(b.finish().is_err());
            assert_eq!(s.io_reserved(), 0);
        }
        let (_, s) = setup();
        let mut b = TypedTableBuilder::<DpRow>::new(&s, 0).unwrap();
        b.append(dp(0)).unwrap();
        let mut cache = RecordPageCache::new(s.memory(), 2).unwrap();
        b.read_committed_cached(0, &mut cache).unwrap();
        b.file.raw().seek(SeekFrom::Start(128)).unwrap();
        b.file.raw().write_all(&[99]).unwrap();
        assert_eq!(b.read_committed_cached(0, &mut cache).unwrap(), dp(0));
        assert!(
            b.finish().is_err(),
            "uncached seal must not trust cache hits"
        );
    }

    #[test]
    fn live_cache_read_admission_and_short_file_are_terminal() {
        use super::super::session::IoLimit;
        let r = ResourceConfig {
            memory: 8 * 1024 * 1024,
            ..ResourceConfig::default()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(
            &c,
            &r,
            OffloadPolicy {
                io_limit: IoLimit::Finite(128 + 96),
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        let mut b = TypedTableBuilder::<DpRow>::new(&s, 0).unwrap();
        b.append(dp(0)).unwrap();
        let mut cache = RecordPageCache::new(s.memory(), 2).unwrap();
        assert_eq!(
            b.read_committed_cached(0, &mut cache).unwrap_err().kind(),
            crate::ErrorKind::TemporaryStorageFailure
        );
        assert_eq!(s.io_used(), 224);
        assert_eq!(s.io_reserved(), 0);
        assert_eq!(b.committed_len(), 1);
        assert!(b.append(dp(1)).is_err());
        assert!(b.finish().is_err());
        s.retry_cleanup().unwrap();
        assert_eq!(c.temp.current(), 0);

        let (_, s) = setup();
        let mut b = TypedTableBuilder::<DpRow>::new(&s, 0).unwrap();
        b.append(dp(0)).unwrap();
        let mut cache = RecordPageCache::new(s.memory(), 2).unwrap();
        b.read_committed_cached(0, &mut cache).unwrap();
        b.append(dp(1)).unwrap();
        b.file.raw().set_len(128 + 96).unwrap();
        assert_eq!(
            b.read_committed_cached(1, &mut cache).unwrap_err().kind(),
            crate::ErrorKind::CorruptIndex
        );
        assert!(b.read_committed_cached(0, &mut cache).is_err());
        assert!(b.finish().is_err());
    }
}
