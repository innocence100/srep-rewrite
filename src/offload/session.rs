//! One operation, shared parent byte budgets, independently leased other resources.
use crate::config::ResourceConfig;
use crate::error::{Error, Result};
use crate::resource::{BudgetedVec, MemoryBudget, Reservation, ResourceContext, TempBudget};
use std::cell::{Ref, RefCell, RefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpillMode {
    Adaptive,
    ForceRam,
    ForceSpill,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoLimit {
    Unlimited,
    Finite(u128),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OffloadPolicy {
    pub(crate) max_handles: u64,
    pub(crate) external_handles: u64,
    pub(crate) max_temp_files: u64,
    pub(crate) max_file_bytes: u64,
    pub(crate) max_fan_in: u8,
    pub(crate) spill_mode: SpillMode,
    pub(crate) io_limit: IoLimit,
}
impl Default for OffloadPolicy {
    fn default() -> Self {
        Self {
            max_handles: 128,
            external_handles: 4,
            max_temp_files: 192,
            max_file_bytes: i64::MAX as u64,
            max_fan_in: 16,
            spill_mode: SpillMode::Adaptive,
            io_limit: IoLimit::Unlimited,
        }
    }
}
impl OffloadPolicy {
    fn validate(self) -> Result<()> {
        if self.max_handles
            < self
                .external_handles
                .checked_add(6)
                .ok_or_else(|| Error::invalid_config("handle policy overflow"))?
            || !(3..=192).contains(&self.max_temp_files)
            || self.max_file_bytes == 0
            || !(2..=16).contains(&self.max_fan_in)
        {
            return Err(Error::invalid_config(
                "offload policy lacks mandatory resource headroom",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionId(u64, [u8; 16]);
impl SessionId {
    pub(crate) fn wire_tag(self) -> [u8; 16] {
        self.1
    }
}
fn next_checked(counter: &AtomicU64) -> Result<u64> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
        .map_err(|_| Error::temp_storage_context("offload identity/generation exhausted"))
}
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CountSnapshot {
    pub(crate) current: u64,
    pub(crate) peak: u64,
}
#[derive(Debug)]
struct CountLedger {
    state: Mutex<CountSnapshot>,
    limit: u64,
}
impl CountLedger {
    fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CountSnapshot::default()),
            limit,
        })
    }
    fn acquire(self: &Arc<Self>, amount: u64) -> Result<ResourceLease> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::temp_storage_context("resource ledger poisoned"))?;
        let next = state
            .current
            .checked_add(amount)
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| Error::temp_storage_context("offload handle/file limit exceeded"))?;
        state.current = next;
        state.peak = state.peak.max(next);
        Ok(ResourceLease {
            ledger: self.clone(),
            amount,
        })
    }
    fn snapshot(&self) -> CountSnapshot {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}
/// Acquire before OS create/open/reopen; put handle fields BEFORE lease fields
/// so handles are closed before the charge is released.
#[derive(Debug)]
pub(crate) struct ResourceLease {
    ledger: Arc<CountLedger>,
    amount: u64,
}
impl ResourceLease {
    pub(crate) fn split(&mut self, amount: u64) -> Result<Self> {
        if amount > self.amount {
            return Err(Error::temp_storage_context("lease split exceeds ownership"));
        }
        self.amount -= amount;
        Ok(Self {
            ledger: self.ledger.clone(),
            amount,
        })
    }
    /// A closed input's ordinary slot may replenish emergency capacity. This
    /// consumes the OS handle first; a live input cannot supply a retirement token.
    pub(crate) fn close_file(self, file: std::fs::File) -> ClosedHandle {
        drop(file);
        ClosedHandle(self)
    }
}
pub(crate) struct ClosedHandle(ResourceLease);
impl Drop for ResourceLease {
    fn drop(&mut self) {
        self.ledger
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current -= self.amount;
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IoSnapshot {
    pub(crate) used_actual: u128,
    pub(crate) reserved: u128,
}
#[derive(Debug)]
struct IoLedger {
    state: Mutex<IoSnapshot>,
    limit: IoLimit,
}
#[derive(Debug)]
pub(crate) struct IoReservation {
    ledger: Arc<IoLedger>,
    remaining: u128,
}
impl IoReservation {
    pub(crate) fn remaining(&self) -> u128 {
        self.remaining
    }
    /// Call immediately after each successful syscall's returned byte count.
    pub(crate) fn consume(&mut self, amount: u64) -> Result<()> {
        let amount = u128::from(amount);
        if amount > self.remaining {
            return Err(Error::temp_storage_context("I/O exceeded permit"));
        }
        let mut s = self
            .ledger
            .state
            .lock()
            .map_err(|_| Error::temp_storage_context("I/O ledger poisoned"))?;
        let used = s
            .used_actual
            .checked_add(amount)
            .ok_or_else(|| Error::temp_storage_context("I/O count overflow"))?;
        s.reserved -= amount;
        s.used_actual = used;
        self.remaining -= amount;
        Ok(())
    }
}
impl Drop for IoReservation {
    fn drop(&mut self) {
        self.ledger
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reserved -= self.remaining;
    }
}

/// Charge physical growth before writing. The owner must retain this charge for
/// a contaminated/partial file until removal or successful truncation.
pub(crate) struct TempFileLease {
    bytes: Reservation,
    length: u64,
    limit: u64,
    _file: ResourceLease,
}
impl TempFileLease {
    pub(crate) fn grow_to(&mut self, length: u64) -> Result<()> {
        if length > self.limit {
            return Err(Error::temp_limit("offload single-file limit exceeded"));
        }
        if length > self.length {
            self.bytes.grow(length - self.length)?;
            self.length = length;
        }
        Ok(())
    }
    pub(crate) fn truncated_to(&mut self, length: u64) -> Result<()> {
        if length > self.length {
            return Err(Error::temp_limit("invalid truncation accounting"));
        }
        self.bytes.shrink(self.length - length);
        self.length = length;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum WorkspaceKind {
    Candidate,
    SourceQuery,
    SourceCache,
    SourceFilter,
    SourceMemtable,
    Publication,
}
/// Independently borrowable buffers: a source query may call a sink which borrows
/// Candidate without recursively borrowing the SourceQuery RefCell.
struct Workspace {
    candidate: RefCell<BudgetedVec<u8>>,
    source_query: RefCell<BudgetedVec<u8>>,
    source_cache: RefCell<BudgetedVec<u8>>,
    source_filter: RefCell<BudgetedVec<u8>>,
    source_memtable: RefCell<BudgetedVec<u8>>,
    publication: RefCell<BudgetedVec<u8>>,
    _control: Reservation,
}
impl Workspace {
    fn new(memory: &MemoryBudget, memtable_override: Option<u64>) -> Result<Self> {
        const MIB: u64 = 1024 * 1024;
        if memory.limit().saturating_sub(memory.current()) < 2 * MIB {
            return Err(Error::memory_limit(
                "offload mandatory workspace requires 2 MiB",
            ));
        }
        let full = memory.limit().saturating_sub(memory.current()) >= 128 * MIB;
        let mut sizes = if full {
            [4 * MIB, 8 * MIB, 4 * MIB, 4 * MIB, 32 * MIB, 64 * 1024]
        } else {
            [
                128 * 1024,
                256 * 1024,
                128 * 1024,
                64 * 1024,
                512 * 1024,
                64 * 1024,
            ]
        };
        sizes[4] = memtable_override.unwrap_or(sizes[4]).min(sizes[4]);
        let control = memory.reserve(if full { 4 * MIB } else { 128 * 1024 })?;
        fn buffer(bytes: u64, budget: &MemoryBudget) -> Result<RefCell<BudgetedVec<u8>>> {
            let len = usize::try_from(bytes)
                .map_err(|_| Error::memory_limit("workspace length overflow"))?;
            let mut b = BudgetedVec::with_capacity(len, budget)?;
            b.resize(len, 0)?;
            Ok(RefCell::new(b))
        }
        Ok(Self {
            candidate: buffer(sizes[0], memory)?,
            source_query: buffer(sizes[1], memory)?,
            source_cache: buffer(sizes[2], memory)?,
            source_filter: buffer(sizes[3], memory)?,
            source_memtable: buffer(sizes[4], memory)?,
            publication: buffer(sizes[5], memory)?,
            _control: control,
        })
    }
    fn cell(&self, kind: WorkspaceKind) -> &RefCell<BudgetedVec<u8>> {
        match kind {
            WorkspaceKind::Candidate => &self.candidate,
            WorkspaceKind::SourceQuery => &self.source_query,
            WorkspaceKind::SourceCache => &self.source_cache,
            WorkspaceKind::SourceFilter => &self.source_filter,
            WorkspaceKind::SourceMemtable => &self.source_memtable,
            WorkspaceKind::Publication => &self.publication,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Create,
    Open,
    Read,
    Write,
    Seek,
    Flush,
    Truncate,
    Close,
    Remove,
    PreRead,
    PreRewind,
    CopyRead,
    CopyWrite,
    CopyFlush,
}
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum FaultAction {
    Fail,
    Cancel,
}
#[cfg(test)]
#[derive(Debug)]
struct Fault {
    operation: Operation,
    after: u64,
    action: FaultAction,
}

pub(crate) struct CompressionSession {
    memory: MemoryBudget,
    temp: TempBudget,
    resources: ResourceConfig,
    policy: OffloadPolicy,
    identity: SessionId,
    generation: AtomicU64,
    io: Arc<IoLedger>,
    handles: Arc<CountLedger>,
    files: Arc<CountLedger>,
    workspace: Workspace,
    // Reserved for nested candidate compaction, never offered to source queries.
    emergency: RefCell<Option<ResourceLease>>,
    _external: ResourceLease,
    cancelled: Arc<AtomicBool>,
    directory: RefCell<Option<Arc<tempfile::TempDir>>>,
    candidate_index_memtable_bytes: u64,
    candidate_index_spills: Arc<AtomicU64>,
    #[cfg(test)]
    fault: Arc<Mutex<[Option<Fault>; 4]>>,
    cleanup: Arc<CleanupRegistry>,
}
impl CompressionSession {
    pub(crate) fn new(
        context: &ResourceContext,
        resources: &ResourceConfig,
        policy: OffloadPolicy,
    ) -> Result<Self> {
        policy.validate()?;
        if context.memory.limit() != resources.memory
            || context.temp.limit() != resources.temp_limit
        {
            return Err(Error::invalid_config("context/resource limits disagree"));
        }
        let handles = CountLedger::new(policy.max_handles);
        let external = handles.acquire(policy.external_handles)?;
        let emergency = handles.acquire(3)?;
        // All admission/allocation occurs BEFORE any tempfile creation.
        let workspace = Workspace::new(&context.memory, context.candidate_index_memtable_bytes)?;
        let candidate_index_memtable_bytes = workspace.source_memtable.borrow().len() as u64;
        Ok(Self {
            memory: context.memory.clone(),
            temp: context.temp.clone(),
            resources: resources.clone(),
            policy,
            identity: {
                let number = next_checked(&NEXT_SESSION)?;
                let mut nonce = [0; 16];
                getrandom::fill(&mut nonce)
                    .map_err(|e| Error::temp_storage_context(e.to_string()))?;
                SessionId(number, nonce)
            },
            generation: AtomicU64::new(0),
            io: Arc::new(IoLedger {
                state: Mutex::new(IoSnapshot::default()),
                limit: policy.io_limit,
            }),
            handles,
            files: CountLedger::new(policy.max_temp_files),
            workspace,
            emergency: RefCell::new(Some(emergency)),
            _external: external,
            cancelled: Arc::new(AtomicBool::new(false)),
            directory: RefCell::new(None),
            candidate_index_memtable_bytes,
            candidate_index_spills: context.candidate_index_spills.clone(),
            #[cfg(test)]
            fault: Arc::new(Mutex::new(std::array::from_fn(|_| None))),
            cleanup: Arc::new(CleanupRegistry::new()),
        })
    }
    pub(crate) fn memory(&self) -> &MemoryBudget {
        &self.memory
    }
    /// Zero/sub-record limits mean direct spill, not a rounded-up memtable.
    pub(crate) fn candidate_index_memtable_bytes(&self) -> u64 {
        self.candidate_index_memtable_bytes
    }
    pub(crate) fn note_candidate_index_spill(&self) -> Result<()> {
        next_checked(&self.candidate_index_spills).map(|_| ())
    }
    pub(crate) fn candidate_index_spill_count(&self) -> u64 {
        self.candidate_index_spills.load(Ordering::Acquire)
    }
    pub(crate) fn temp(&self) -> &TempBudget {
        &self.temp
    }
    pub(crate) fn resources(&self) -> &ResourceConfig {
        &self.resources
    }
    pub(crate) fn policy(&self) -> OffloadPolicy {
        self.policy
    }
    pub(crate) fn identity(&self) -> SessionId {
        self.identity
    }
    pub(crate) fn next_generation(&self) -> Result<u64> {
        next_checked(&self.generation)
    }
    pub(crate) fn workspace(&self, kind: WorkspaceKind) -> Result<RefMut<'_, [u8]>> {
        self.check_cancelled()?;
        let b = self
            .workspace
            .cell(kind)
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("workspace already lent"))?;
        Ok(RefMut::map(b, |v| v.as_mut_slice()))
    }
    /// Take a typed fixed buffer once instead of the reserved byte buffer. This
    /// releases only that slot's allocation before allocating T (safe, no casts).
    /// A failed allocation leaves the slot empty and the operation must abort.
    pub(crate) fn take_typed_workspace<T>(&self, kind: WorkspaceKind) -> Result<BudgetedVec<T>> {
        if std::mem::size_of::<T>() == 0 {
            return Err(Error::invalid_config("zero-sized workspace element"));
        }
        let mut slot = self
            .workspace
            .cell(kind)
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("workspace already lent"))?;
        let bytes = slot.len();
        if bytes == 0 {
            return Err(Error::temp_storage_context("workspace already transferred"));
        }
        *slot = BudgetedVec::new(&self.memory)?;
        BudgetedVec::with_capacity(bytes / std::mem::size_of::<T>(), &self.memory)
    }
    pub(crate) fn acquire_handles(&self, count: u64) -> Result<ResourceLease> {
        self.check_cancelled()?;
        self.handles.acquire(count)
    }
    pub(crate) fn acquire_file_slot(&self) -> Result<ResourceLease> {
        self.check_cancelled()?;
        self.files.acquire(1)
    }
    /// Lazy exclusive directory; all mandatory memory/handle admission already
    /// succeeded in new(). No file is created merely by constructing a session.
    pub(crate) fn temp_directory(&self) -> Result<Ref<'_, std::path::Path>> {
        if self.directory.borrow().is_none() {
            self.checkpoint(Operation::Create)?;
            let directory = tempfile::Builder::new()
                .prefix("srep-offload-")
                .tempdir_in(&self.resources.temp_dir)
                .map_err(Error::temp_storage)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                    .map_err(Error::temp_storage)?;
            }
            *self.directory.borrow_mut() = Some(Arc::new(directory));
        }
        Ok(Ref::map(self.directory.borrow(), |d| {
            d.as_ref().expect("initialized directory").path()
        }))
    }
    /// Every temporary-file owner retains this until its handles/path are gone.
    pub(crate) fn directory_owner(&self) -> Result<Arc<tempfile::TempDir>> {
        drop(self.temp_directory()?);
        Ok(self
            .directory
            .borrow()
            .as_ref()
            .expect("initialized directory")
            .clone())
    }
    pub(crate) fn admit_file(&self) -> Result<TempFileLease> {
        self.check_cancelled()?;
        Ok(TempFileLease {
            bytes: self.temp.reserve(0)?,
            length: 0,
            limit: self.policy.max_file_bytes.min(self.temp.limit()),
            _file: self.files.acquire(1)?,
        })
    }
    pub(crate) fn merge_fan_in(&self, wanted: usize) -> Result<(usize, ResourceLease)> {
        let free = self.policy.max_handles - self.handles.snapshot().current;
        let inputs = wanted
            .min(self.policy.max_fan_in as usize)
            .min(free.saturating_sub(1) as usize);
        if inputs < 2 {
            return Err(Error::temp_storage_context(
                "insufficient merge handle headroom",
            ));
        }
        Ok((inputs, self.acquire_handles(inputs as u64 + 1)?))
    }
    pub(crate) fn emergency_merge(&self) -> Result<EmergencyMerge<'_>> {
        let lease = self
            .emergency
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("emergency merge already borrowed"))?
            .take()
            .ok_or_else(|| Error::temp_storage_context("emergency merge already active"))?;
        Ok(EmergencyMerge {
            slot: &self.emergency,
            lease: Some(lease),
        })
    }
    pub(crate) fn handle_usage(&self) -> CountSnapshot {
        self.handles.snapshot()
    }
    pub(crate) fn file_usage(&self) -> CountSnapshot {
        self.files.snapshot()
    }
    pub(crate) fn reserve_io(&self, amount: u128) -> Result<IoReservation> {
        self.io_access().reserve_io(amount)
    }
    pub(crate) fn reserve_publication_io(&self, length: u64) -> Result<IoReservation> {
        self.reserve_io(
            u128::from(length)
                .checked_mul(3)
                .ok_or_else(|| Error::temp_storage_context("I/O envelope overflow"))?,
        )
    }
    pub(crate) fn io_snapshot(&self) -> IoSnapshot {
        *self.io.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn io_used(&self) -> u128 {
        self.io_snapshot().used_actual
    }
    pub(crate) fn io_reserved(&self) -> u128 {
        self.io_snapshot().reserved
    }
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub(crate) fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(Error::temp_storage_context("offload operation cancelled"));
        }
        Ok(())
    }
    pub(crate) fn checkpoint(&self, operation: Operation) -> Result<()> {
        self.io_access().checkpoint(operation)
    }
    #[cfg(test)]
    pub(crate) fn inject(&self, operation: Operation, after: u64, action: FaultAction) {
        let mut faults = self.fault.lock().unwrap();
        let index = faults
            .iter()
            .position(|f| f.as_ref().is_some_and(|f| f.operation == operation))
            .or_else(|| faults.iter().position(Option::is_none))
            .expect("bounded test fault slots");
        let slot = &mut faults[index];
        *slot = Some(Fault {
            operation,
            after,
            action,
        });
    }
    pub(crate) fn io_access(&self) -> SessionIo {
        SessionIo {
            io: self.io.clone(),
            cancelled: self.cancelled.clone(),
            temp: self.temp.clone(),
            cleanup: self.cleanup.clone(),
            #[cfg(test)]
            fault: self.fault.clone(),
        }
    }
    pub(crate) fn retry_cleanup(&self) -> Result<()> {
        self.cleanup.retry()
    }
}
/// Shared operation state held by session-created file owners, even if the
/// session object is dropped first. No second byte budget is created.
#[derive(Clone, Debug)]
pub(crate) struct SessionIo {
    io: Arc<IoLedger>,
    cancelled: Arc<AtomicBool>,
    temp: TempBudget,
    cleanup: Arc<CleanupRegistry>,
    #[cfg(test)]
    fault: Arc<Mutex<[Option<Fault>; 4]>>,
}
impl SessionIo {
    pub(crate) fn reserve_io(&self, amount: u128) -> Result<IoReservation> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(Error::temp_storage_context("offload operation cancelled"));
        }
        let mut s = self
            .io
            .state
            .lock()
            .map_err(|_| Error::temp_storage_context("I/O ledger poisoned"))?;
        let total = s
            .used_actual
            .checked_add(s.reserved)
            .and_then(|n| n.checked_add(amount))
            .ok_or_else(|| Error::temp_storage_context("I/O admission overflow"))?;
        if matches!(self.io.limit,IoLimit::Finite(limit) if total>limit) {
            return Err(Error::temp_storage_context("I/O budget exceeded"));
        }
        s.reserved += amount;
        Ok(IoReservation {
            ledger: self.io.clone(),
            remaining: amount,
        })
    }
    pub(crate) fn checkpoint(&self, op: Operation) -> Result<()> {
        // Cleanup must run even after cancellation.
        let cleanup = matches!(
            op,
            Operation::Truncate | Operation::Remove | Operation::Close
        );
        if !cleanup && self.cancelled.load(Ordering::Acquire) {
            return Err(Error::temp_storage_context("offload operation cancelled"));
        }
        #[cfg(test)]
        {
            let mut faults = self.fault.lock().unwrap();
            if let Some(slot) = faults
                .iter_mut()
                .find(|s| s.as_ref().is_some_and(|f| f.operation == op))
            {
                let f = slot.as_mut().unwrap();
                if f.after == 0 {
                    let action = f.action;
                    *slot = None;
                    match action {
                        FaultAction::Fail => {
                            return Err(Error::temp_storage_context(
                                "injected offload I/O failure",
                            ));
                        }
                        FaultAction::Cancel => self.cancelled.store(true, Ordering::Release),
                    }
                } else {
                    f.after -= 1;
                }
            }
        }
        if !cleanup && self.cancelled.load(Ordering::Acquire) {
            return Err(Error::temp_storage_context("offload operation cancelled"));
        }
        Ok(())
    }
    pub(crate) fn empty_temp_reservation(&self) -> Reservation {
        self.temp.reserve(0).expect("zero temp reservation")
    }
    pub(crate) fn defer_cleanup(&self, entry: CleanupEntry) {
        self.cleanup.insert(entry, None);
    }
    pub(crate) fn defer_cleanup_with_metadata(&self, entry: CleanupEntry, metadata: Reservation) {
        self.cleanup.insert(entry, Some(metadata));
    }
}
#[derive(Debug)]
pub(crate) struct CleanupEntry {
    pub(crate) path: std::path::PathBuf,
    pub(crate) bytes: Reservation,
    pub(crate) file: ResourceLease,
    pub(crate) directory: Arc<tempfile::TempDir>,
}
#[derive(Debug)]
struct CleanupRegistry {
    entries: Mutex<[Option<(CleanupEntry, Option<Reservation>)>; 192]>,
}
impl CleanupRegistry {
    fn new() -> Self {
        Self {
            entries: Mutex::new(std::array::from_fn(|_| None)),
        }
    }
    fn insert(&self, entry: CleanupEntry, metadata: Option<Reservation>) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = entries.iter_mut().find(|v| v.is_none()) {
            *slot = Some((entry, metadata));
        } else {
            // File slots are capped at192 and held by entries, so unreachable
            // under the ownership protocol. Fail closed if an owner violates it.
            eprintln!("SREP_E_TEMP_STORAGE: cleanup registry full; budget quarantined");
            std::mem::forget((entry, metadata));
        }
    }
    fn retry(&self) -> Result<()> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut failed = false;
        for slot in entries.iter_mut() {
            if let Some((entry, _metadata)) = slot {
                match std::fs::remove_file(&entry.path) {
                    Ok(()) => {
                        *slot = None;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        *slot = None;
                    }
                    Err(_) => failed = true,
                }
            }
        }
        if failed {
            Err(Error::temp_storage_context(
                "offload cleanup incomplete; reservations retained",
            ))
        } else {
            Ok(())
        }
    }
}
impl Drop for CleanupRegistry {
    fn drop(&mut self) {
        let _ = self.retry();
        for slot in self
            .entries
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
        {
            if let Some(entry) = slot.take() {
                // Cannot honestly refund a shared parent budget for a surviving
                // file. Quarantine the charge for process lifetime, emit failure.
                eprintln!(
                    "SREP_E_TEMP_STORAGE: cleanup failed; temporary bytes/file charge quarantined"
                );
                std::mem::forget(entry);
            }
        }
    }
}
pub(crate) struct EmergencyMerge<'a> {
    slot: &'a RefCell<Option<ResourceLease>>,
    lease: Option<ResourceLease>,
}
impl EmergencyMerge<'_> {
    /// The replacement is already open under this guard's emergency allowance.
    /// After inputs are closed, transfer one emergency slot into its ordinary
    /// owner and replenish the emergency pool with a retired ordinary slot.
    /// On error the caller MUST close the replacement before dropping the guard;
    /// returning Self on failure keeps that allowance live.
    pub(crate) fn adopt_output(
        mut self,
        first: ClosedHandle,
        second: ClosedHandle,
    ) -> std::result::Result<ResourceLease, (Error, Self, ClosedHandle, ClosedHandle)> {
        let pool = self.lease.as_ref().expect("active emergency lease");
        if !Arc::ptr_eq(&pool.ledger, &first.0.ledger)
            || !Arc::ptr_eq(&pool.ledger, &second.0.ledger)
            || first.0.amount != 1
            || second.0.amount != 1
        {
            return Err((
                Error::temp_storage_context(
                    "merge retirement must supply two same-ledger single handles",
                ),
                self,
                first,
                second,
            ));
        }
        let pool = self.lease.as_mut().unwrap();
        let output = pool.split(1).expect("three emergency slots reserved");
        let mut retired = first.0;
        pool.amount += retired.amount;
        retired.amount = 0;
        drop(second); // second closed input releases its ordinary slot
        // Drop restores the complete emergency pool. Output retains its charge.
        Ok(output)
    }
}
impl Drop for EmergencyMerge<'_> {
    fn drop(&mut self) {
        *self.slot.borrow_mut() = self.lease.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn small() -> ResourceConfig {
        ResourceConfig {
            memory: 2 * 1024 * 1024,
            ..ResourceConfig::default()
        }
    }
    #[test]
    fn nested_buffers_and_parent_reservations() {
        let r = small();
        let c = ResourceContext::with_resources(&r).unwrap();
        {
            let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
            let mut q = s.workspace(WorkspaceKind::SourceQuery).unwrap();
            let mut sink = s.workspace(WorkspaceKind::Candidate).unwrap();
            q[0] = 1;
            sink[0] = 2;
            assert!(s.workspace(WorkspaceKind::SourceQuery).is_err());
            assert!(c.memory.current() > 1024 * 1024);
            assert_eq!(s.memory().current(), c.memory.current());
        }
        assert_eq!(c.memory.current(), 0);
    }
    #[test]
    fn resource_leases_release_and_preserve_emergency() {
        let r = small();
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(
            &c,
            &r,
            OffloadPolicy {
                max_handles: 10,
                max_temp_files: 3,
                max_file_bytes: 8,
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        assert_eq!(s.handle_usage().current, 7);
        {
            let _h = s.acquire_handles(3).unwrap();
            assert!(s.acquire_handles(1).is_err());
            let _em = s.emergency_merge().unwrap();
            assert!(s.emergency_merge().is_err());
        }
        assert_eq!(s.handle_usage().current, 7);
        assert!(s.emergency_merge().is_ok());
        let mut f = s.admit_file().unwrap();
        f.grow_to(8).unwrap();
        assert!(f.grow_to(9).is_err());
        assert_eq!(c.temp.current(), 8);
        f.truncated_to(3).unwrap();
        assert_eq!(c.temp.current(), 3);
        {
            let _a = s.admit_file().unwrap();
            let _b = s.admit_file().unwrap();
            assert!(s.admit_file().is_err());
        }
        drop(f);
        assert_eq!(c.temp.current(), 0);
        assert_eq!(s.file_usage().current, 0);
    }
    #[test]
    fn failed_admission_and_counter_exhaustion() {
        let r = ResourceConfig {
            memory: 1024,
            ..small()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        assert!(CompressionSession::new(&c, &r, OffloadPolicy::default()).is_err());
        assert_eq!(c.memory.current(), 0);
        let counter = AtomicU64::new(u64::MAX);
        assert!(next_checked(&counter).is_err());
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        let r = small();
        let c = ResourceContext::with_resources(&r).unwrap();
        assert!(
            CompressionSession::new(
                &c,
                &r,
                OffloadPolicy {
                    max_handles: 6,
                    ..OffloadPolicy::default()
                }
            )
            .is_err()
        );
        assert_eq!(c.memory.current(), 0);
    }
    #[test]
    fn io_wide_arithmetic_and_release() {
        let r = small();
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        {
            let mut p = s.reserve_publication_io(u64::MAX).unwrap();
            assert_eq!(p.remaining(), u128::from(u64::MAX) * 3);
            p.consume(7).unwrap();
        }
        assert_eq!(s.io_used(), 7);
        assert_eq!(s.io_reserved(), 0);
        let p = s.reserve_io(u128::MAX - 7).unwrap();
        assert!(s.reserve_io(1).is_err());
        drop(p);
    }

    #[test]
    fn typed_workspace_owns_parent_charge_and_directory_cleans_up() {
        let r = small();
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        assert!(s.directory.borrow().is_none());
        let bytes = s.workspace(WorkspaceKind::SourceMemtable).unwrap().len();
        let buffer = s
            .take_typed_workspace::<u64>(WorkspaceKind::SourceMemtable)
            .unwrap();
        assert_eq!(buffer.capacity(), bytes / 8);
        assert!(
            s.take_typed_workspace::<u64>(WorkspaceKind::SourceMemtable)
                .is_err()
        );
        let path = s.temp_directory().unwrap().to_path_buf();
        assert!(path.exists());
        assert_eq!(s.next_generation().unwrap(), 0);
        assert_eq!(s.next_generation().unwrap(), 1);
        let (n, lease) = s.merge_fan_in(30).unwrap();
        assert_eq!(n, 16);
        drop(lease);
        s.generation.store(u64::MAX, Ordering::Release);
        assert!(s.next_generation().is_err());
        drop(s);
        assert!(!path.exists());
        assert_eq!(c.memory.current(), buffer.reserved_bytes());
        drop(buffer);
        assert_eq!(c.memory.current(), 0);
    }

    #[test]
    fn per_session_faults_and_cancellation_are_not_global() {
        let r = small();
        let c = ResourceContext::with_resources(&r).unwrap();
        let a = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let c2 = ResourceContext::with_resources(&r).unwrap();
        let b = CompressionSession::new(&c2, &r, OffloadPolicy::default()).unwrap();
        a.inject(Operation::Create, 0, FaultAction::Fail);
        assert!(a.temp_directory().is_err());
        assert!(a.directory.borrow().is_none());
        b.checkpoint(Operation::Create).unwrap();
        a.cancel();
        assert!(a.acquire_handles(1).is_err());
        assert!(a.reserve_io(1).is_err());
        b.check_cancelled().unwrap();
    }

    #[test]
    fn emergency_output_survives_two_consecutive_merges_at_limit() {
        let r = small();
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(
            &c,
            &r,
            OffloadPolicy {
                max_handles: 10,
                ..OffloadPolicy::default()
            },
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let a = std::fs::File::create(dir.path().join("a")).unwrap();
        let la = s.acquire_handles(1).unwrap();
        let b = std::fs::File::create(dir.path().join("b")).unwrap();
        let lb = s.acquire_handles(1).unwrap();
        let other = s.acquire_handles(1).unwrap();
        assert!(s.acquire_handles(1).is_err());
        let merge = s.emergency_merge().unwrap();
        let output = std::fs::File::create(dir.path().join("replacement")).unwrap();
        let output_lease = match merge.adopt_output(la.close_file(a), lb.close_file(b)) {
            Ok(l) => l,
            Err(_) => panic!("valid retirement"),
        };
        assert_eq!(s.handle_usage().current, 9);
        assert!(s.emergency_merge().is_ok());
        let next = std::fs::File::create(dir.path().join("next")).unwrap();
        let ln = s.acquire_handles(1).unwrap();
        assert_eq!(s.handle_usage().current, 10);
        let merge = s.emergency_merge().unwrap();
        let final_file = std::fs::File::create(dir.path().join("final")).unwrap();
        let final_lease =
            match merge.adopt_output(output_lease.close_file(output), ln.close_file(next)) {
                Ok(l) => l,
                Err(_) => panic!("valid retirement"),
            };
        assert_eq!(s.handle_usage().current, 9);
        assert_eq!(s.handle_usage().peak, 10);
        assert!(s.emergency_merge().is_ok());
        drop(final_lease.close_file(final_file));
        drop(other);
        assert_eq!(s.handle_usage().current, 7);
    }

    #[test]
    fn memtable_override_and_spill_counter_are_shared_with_context_clones() {
        for cap in [0, 1, 4096, u64::MAX] {
            let r = small();
            let mut c = ResourceContext::with_resources(&r).unwrap();
            c.candidate_index_memtable_bytes = Some(cap);
            let clone = c.clone();
            let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
            assert_eq!(s.candidate_index_memtable_bytes(), cap.min(512 * 1024));
            assert_eq!(
                s.workspace(WorkspaceKind::SourceMemtable).unwrap().len() as u64,
                cap.min(512 * 1024)
            );
            s.note_candidate_index_spill().unwrap();
            clone.note_candidate_index_spill();
            assert_eq!(c.candidate_index_spill_count(), 2);
            assert_eq!(clone.candidate_index_spill_count(), 2);
            assert_eq!(s.candidate_index_spill_count(), 2);
        }
    }
}
