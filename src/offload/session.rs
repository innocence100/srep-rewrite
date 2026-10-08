//! One operation, shared parent byte budgets, independently leased other resources.
use crate::config::ResourceConfig;
use crate::error::{Error, Result};
use crate::resource::{BudgetedVec, MemoryBudget, Reservation, ResourceContext, TempBudget};
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::ops::{Deref, DerefMut};
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
impl From<Option<u128>> for IoLimit {
    fn from(limit: Option<u128>) -> Self {
        match limit {
            Some(n) => Self::Finite(n),
            None => Self::Unlimited,
        }
    }
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
            io_limit: None.into(),
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
    // A CAS loop is available on the declared MSRV and avoids the deprecated
    // atomic update API on current stable toolchains.
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current
            .checked_add(1)
            .ok_or_else(|| Error::temp_storage_context("offload identity/generation exhausted"))?;
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
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
    _control: Arc<SharedControl>,
}
/// Reservation owner for heap allocations shared by session-created owners.
/// It is deliberately separate from CompressionSession so ResourceLease and
/// SessionIo clones keep this charge live after the session is dropped.
#[derive(Debug)]
pub(crate) struct SharedControl {
    pub(crate) _reservation: Reservation,
}
#[derive(Debug)]
pub(crate) struct DirectoryOwner {
    pub(crate) tempdir: tempfile::TempDir,
    /// The resolved absolute parent. It is retained so later temporary-file
    /// creation never repeats the cwd query and a relative configured parent
    /// stays consistent even if the process cwd changes. This is file-pathname
    /// processing; per the `--memory` contract its heap bytes are excluded from
    /// managed working-memory accounting. Only the fixed inline control state
    /// below is charged.
    pub(crate) absolute_parent: std::path::PathBuf,
    pub(crate) _control: Reservation,
}
impl DirectoryOwner {
    pub(crate) fn path(&self) -> &std::path::Path {
        self.tempdir.path()
    }
    /// The cached absolute parent. Temporary-file owners use it to verify that
    /// the created directory path lies under the same absolute parent, which
    /// keeps cleanup correct even if the process current directory changes
    /// after a relative parent was configured.
    pub(crate) fn parent(&self) -> &std::path::Path {
        &self.absolute_parent
    }
    /// Defensive consistency check for a freshly created temporary file: its
    /// path must be under the cached absolute parent. This is control-only and
    /// costs no managed working bytes.
    pub(crate) fn verify_contains(&self, child: &std::path::Path) -> Result<()> {
        if child.starts_with(self.parent()) {
            Ok(())
        } else {
            Err(Error::temp_storage_context(
                "temporary path escaped resolved parent",
            ))
        }
    }
}
/// Fixed working-control charge for one `DirectoryOwner`. It covers the owner
/// value, its `Arc` header, and the allocator/box envelope for the inline
/// control state. The absolute path bytes and the `TempDir` pathname are
/// excluded as file-pathname processing under the `--memory` contract.
fn directory_owner_control() -> u64 {
    u64::try_from(
        std::mem::size_of::<DirectoryOwner>()
            + 2 * std::mem::size_of::<usize>()
            + PATH_ALLOCATOR_ENVELOPE,
    )
    .unwrap_or(u64::MAX)
}
/// Fixed working-control charge for one per-file cleanup owner. It covers the
/// `TempPath` box header, allocator envelope, and the deferred cleanup entry
/// slot-control state that persists until the file is verified removed. The
/// pathname bytes themselves are excluded as file-pathname processing; this
/// charge is deliberately independent of path length.
pub(crate) fn file_path_control() -> u64 {
    u64::try_from(
        std::mem::size_of::<CleanupEntry>()
            + 2 * std::mem::size_of::<usize>()
            + PATH_ALLOCATOR_ENVELOPE,
    )
    .unwrap_or(u64::MAX)
}
/// Fixed allocator/box envelope for pathname owners. This is control overhead,
/// not the pathname bytes themselves, and is length-independent.
const PATH_ALLOCATOR_ENVELOPE: usize = 64;
/// Resolve a configured temporary parent to an absolute path. The returned
/// `PathBuf` owns its exact-size allocation so no uncharged duplicate path
/// survives; its bytes are excluded from managed working-memory accounting as
/// file-pathname processing. No global current-directory mutation occurs.
fn resolve_absolute_parent(configured: &std::path::Path) -> Result<std::path::PathBuf> {
    if configured.is_absolute() {
        return Ok(configured.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(Error::temp_storage)?;
    let mut joined = std::path::PathBuf::with_capacity(
        cwd.as_os_str()
            .len()
            .saturating_add(configured.as_os_str().len())
            .saturating_add(1),
    );
    joined.push(&cwd);
    joined.push(configured);
    Ok(joined)
}
impl CountLedger {
    fn new(limit: u64, control: Arc<SharedControl>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CountSnapshot::default()),
            limit,
            _control: control,
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
    _control: Arc<SharedControl>,
}
#[derive(Debug)]
pub(crate) struct IoReservation {
    ledger: Arc<IoLedger>,
    remaining: u128,
}
impl IoReservation {
    #[cfg(test)]
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
#[cfg(test)]
pub(crate) struct TempFileLease {
    bytes: Reservation,
    length: u64,
    limit: u64,
    _file: ResourceLease,
}
#[cfg(test)]
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
    #[cfg(test)]
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

/// A fixed-capacity allocation lent to one source index. Returning the loan
/// transfers the SAME BudgetedVec/reservation; no byte/typed conversion or new
/// allocation occurs on the next base/overlay index.
pub(crate) struct SourceLoan<'a, T> {
    value: Option<BudgetedVec<T>>,
    slot: &'a RefCell<Option<BudgetedVec<T>>>,
}
impl<T> Deref for SourceLoan<'_, T> {
    type Target = BudgetedVec<T>;
    fn deref(&self) -> &Self::Target {
        self.value.as_ref().unwrap()
    }
}
impl<T> DerefMut for SourceLoan<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value.as_mut().unwrap()
    }
}
impl<T> Drop for SourceLoan<'_, T> {
    fn drop(&mut self) {
        let mut value = self.value.take().unwrap();
        value.clear_with_capacity();
        *self.slot.borrow_mut() = Some(value);
    }
}
impl Workspace {
    fn new(
        memory: &MemoryBudget,
        memtable_override: Option<u64>,
        compact: bool,
        tiny: bool,
        full: bool,
    ) -> Result<Self> {
        const MIB: u64 = 1024 * 1024;
        if memory.limit().saturating_sub(memory.current()) < 96 * 1024 {
            return Err(Error::memory_limit(
                "offload mandatory workspace requires 96 KiB",
            ));
        }
        let mut sizes = if tiny {
            [1024, 1024, 8192, 512, 1024, 1024]
        } else if compact {
            [4096, 4096, 65536, 1024, 4096, 4096]
        } else if full {
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
        sizes[4] = memtable_override.unwrap_or(sizes[4]).min(if tiny {
            64 * 1024
        } else if compact {
            512 * 1024
        } else {
            sizes[4]
        });
        let control = memory.reserve(if tiny {
            4096
        } else if compact {
            8192
        } else if full {
            4 * MIB
        } else {
            128 * 1024
        })?;
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
            #[cfg(test)]
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
    Remove,
    PreRead,
    PreRewind,
    CopyRead,
    CopyWrite,
    CopyFlush,
    RetireVerify,
}
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LifecycleEvent {
    EmergencyFileClosed,
    EmergencyReturned,
    RetireVerify,
}
#[cfg(test)]
#[derive(Debug)]
struct Lifecycle {
    events: [Option<LifecycleEvent>; 32],
    len: usize,
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
    profile: bool,
    profile_started: std::time::Instant,
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
    // Covers heap-backed ledgers and construction-time control metadata before
    // their Arc allocations. The workspace's compact 8 KiB control reservation
    // separately covers its fixed cells/RefCells; cleanup_registry_bytes()
    // covers the 192 deferred-owner slots and its Arc allocation.
    // Reserved for nested candidate compaction, never offered to source queries.
    emergency: RefCell<Option<ResourceLease>>,
    _external: ResourceLease,
    cancelled: Arc<AtomicBool>,
    directory: RefCell<Option<Arc<DirectoryOwner>>>,
    candidate_index_memtable_bytes: u64,
    candidate_index_spills: Arc<AtomicU64>,
    #[cfg(test)]
    fault: Arc<Mutex<[Option<Fault>; 4]>>,
    cleanup: Arc<CleanupRegistry>,
    shared_control: Arc<SharedControl>,
    source_memtable_pool: RefCell<Option<BudgetedVec<crate::candidate_index::IndexEntry>>>,
    source_cache_pool: RefCell<Option<BudgetedVec<u8>>>,
    source_filter_pool: RefCell<Option<BudgetedVec<u8>>>,
    source_initialized: Cell<bool>,
    source_failed: Cell<bool>,
    generation_finished: Cell<bool>,
    m3_epoch: Cell<u8>,
    #[cfg(test)]
    lifecycle: Arc<Mutex<Lifecycle>>,
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
        // This reservation is deliberately acquired before CountLedger::new:
        // the two Arc<CountLedger> allocations and their ResourceLease fields
        // must not be created before their parent memory admission succeeds.
        let available = context
            .memory
            .limit()
            .saturating_sub(context.memory.current());
        let compact = available < 2 * 1024 * 1024;
        let tiny = available < 512 * 1024;
        let full = available >= 128 * 1024 * 1024;
        // io_control is fixed working state only. It deliberately does NOT
        // include any pathname-length envelope: the configured temp_dir is a
        // file pathname, and its length is excluded from managed working
        // memory under the `--memory` contract.
        let io_control = std::mem::size_of::<IoLedger>()
            .saturating_add(std::mem::size_of::<AtomicBool>())
            .saturating_add(4 * std::mem::size_of::<usize>());
        let admission_control = context.memory.reserve(
            session_control_bytes()
                .checked_add(io_control as u64)
                .ok_or_else(|| Error::memory_limit("shared control size overflows"))?,
        )?;
        let shared_control = Arc::new(SharedControl {
            _reservation: admission_control,
        });
        let handles = CountLedger::new(policy.max_handles, shared_control.clone());
        let external = handles.acquire(policy.external_handles)?;
        let emergency = handles.acquire(3)?;
        // All admission/allocation occurs BEFORE any tempfile creation.
        // Cleanup owners can outlive the session (for example when a platform
        // keeps a temporary file open).  Charge the complete registry object,
        // its Arc allocation header, and the retained Arc handle before any
        // workspace allocation.  The reservation is moved into the registry
        // itself, so deferred cleanup cannot outlive its memory owner or
        // silently steal another workspace's allowance.
        let cleanup_control = context.memory.reserve(cleanup_registry_bytes())?;
        let workspace = Workspace::new(
            &context.memory,
            context.candidate_index_memtable_bytes,
            compact,
            tiny,
            full,
        )?;
        let candidate_index_memtable_bytes = workspace.source_memtable.borrow().len() as u64;
        Ok(Self {
            profile: std::env::var_os("SREP_OFFLOAD_PROFILE").is_some(),
            profile_started: std::time::Instant::now(),
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
                _control: shared_control.clone(),
            }),
            handles,
            files: CountLedger::new(policy.max_temp_files, shared_control.clone()),
            workspace,
            emergency: RefCell::new(Some(emergency)),
            _external: external,
            cancelled: Arc::new(AtomicBool::new(false)),
            directory: RefCell::new(None),
            candidate_index_memtable_bytes,
            candidate_index_spills: context.candidate_index_spills.clone(),
            #[cfg(test)]
            fault: Arc::new(Mutex::new(std::array::from_fn(|_| None))),
            cleanup: Arc::new(CleanupRegistry::new(cleanup_control)),
            shared_control,
            source_memtable_pool: RefCell::new(None),
            source_cache_pool: RefCell::new(None),
            source_filter_pool: RefCell::new(None),
            source_initialized: Cell::new(false),
            source_failed: Cell::new(false),
            generation_finished: Cell::new(false),
            m3_epoch: Cell::new(0),
            #[cfg(test)]
            lifecycle: Arc::new(Mutex::new(Lifecycle {
                events: [None; 32],
                len: 0,
            })),
        })
    }
    pub(crate) fn memory(&self) -> &MemoryBudget {
        &self.memory
    }
    /// Opt-in bounded diagnostic stream, never retained as an event collection.
    pub(crate) fn profile(&self, event: &str, detail: std::fmt::Arguments<'_>) {
        if self.profile {
            let io = *self.io.state.lock().unwrap_or_else(|e| e.into_inner());
            eprintln!(
                "OFFLOAD_PROFILE event={event} elapsed_ms={} ram={} ram_peak={} temp={} temp_peak={} handles={} files={} io={} {detail}",
                self.profile_started.elapsed().as_millis(),
                self.memory.current(),
                self.memory.high_water(),
                self.temp.current(),
                self.temp.high_water(),
                self.handle_usage().current,
                self.file_usage().current,
                io.used_actual
            );
        }
    }
    /// Existing exact accelerators take ResourceContext; this view shares the
    /// same ledgers/counter rather than creating fresh resource budgets.
    pub(crate) fn finder_context(&self) -> ResourceContext {
        ResourceContext {
            memory: self.memory.clone(),
            temp: self.temp.clone(),
            temp_dir: self.resources.temp_dir.clone(),
            candidate_index_memtable_bytes: Some(self.candidate_index_memtable_bytes),
            candidate_index_spills: self.candidate_index_spills.clone(),
        }
    }
    /// Zero/sub-record limits mean direct spill, not a rounded-up memtable.
    pub(crate) fn candidate_index_memtable_bytes(&self) -> u64 {
        self.candidate_index_memtable_bytes
    }
    pub(crate) fn note_candidate_index_spill(&self) -> Result<()> {
        next_checked(&self.candidate_index_spills).map(|_| ())
    }
    #[cfg(test)]
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
    pub(crate) fn source_failed(&self) {
        self.source_failed.set(true);
    }
    fn initialize_source_pools(&self) -> Result<()> {
        if self.generation_finished.get() {
            return Err(Error::temp_storage_context(
                "generation phase already finished",
            ));
        }
        self.check_cancelled()?;
        if self.source_failed.get() {
            return Err(Error::temp_storage_context(
                "source workspace session failed",
            ));
        }
        if self.source_initialized.get() {
            return Ok(());
        }
        // Mark before moving buffers: any partial initialization failure is
        // terminal and cannot allocate a second unaccounted replacement set.
        self.source_initialized.set(true);
        let result = (|| {
            let memtable = if self.candidate_index_memtable_bytes
                < std::mem::size_of::<crate::candidate_index::IndexEntry>() as u64
            {
                BudgetedVec::new(&self.memory)?
            } else {
                self.take_typed_workspace(WorkspaceKind::SourceMemtable)?
            };
            let cache = self.take_typed_workspace(WorkspaceKind::SourceCache)?;
            let filter = self.take_typed_workspace(WorkspaceKind::SourceFilter)?;
            *self.source_memtable_pool.borrow_mut() = Some(memtable);
            *self.source_cache_pool.borrow_mut() = Some(cache);
            *self.source_filter_pool.borrow_mut() = Some(filter);
            Ok(())
        })();
        if result.is_err() {
            self.source_failed.set(true);
        }
        result
    }
    fn loan<'a, T>(
        &'a self,
        slot: &'a RefCell<Option<BudgetedVec<T>>>,
    ) -> Result<SourceLoan<'a, T>> {
        self.initialize_source_pools()?;
        let value = slot
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("source loan slot busy"))?
            .take()
            .ok_or_else(|| Error::temp_storage_context("source workspace already in use"))?;
        Ok(SourceLoan {
            value: Some(value),
            slot,
        })
    }
    pub(crate) fn source_memtable(
        &self,
    ) -> Result<SourceLoan<'_, crate::candidate_index::IndexEntry>> {
        self.loan(&self.source_memtable_pool)
    }
    pub(crate) fn source_cache(&self) -> Result<SourceLoan<'_, u8>> {
        self.loan(&self.source_cache_pool)
    }
    pub(crate) fn source_filter(&self) -> Result<SourceLoan<'_, u8>> {
        self.loan(&self.source_filter_pool)
    }
    /// One-way phase transition, only after verified finders/sinks have finished.
    /// Any outstanding source loan prevents reclamation; no budget reset occurs.
    pub(crate) fn finish_generation(&self) -> Result<()> {
        self.check_cancelled()?;
        if self.generation_finished.get()
            || self.source_failed.get()
            || matches!(self.m3_epoch.get(), 1..=3)
        {
            return Err(Error::temp_storage_context("invalid generation transition"));
        }
        let mut mem = self
            .source_memtable_pool
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("source memtable borrowed"))?;
        let mut cache = self
            .source_cache_pool
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("source cache borrowed"))?;
        let mut filter = self
            .source_filter_pool
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("source filter borrowed"))?;
        if self.source_initialized.get() && (mem.is_none() || cache.is_none() || filter.is_none()) {
            return Err(Error::temp_storage_context("source loans still active"));
        }
        // Obtain all raw workspace borrows before dropping any allocation.
        let mut raw_mem = self
            .workspace
            .source_memtable
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("source workspace borrowed"))?;
        let mut raw_cache = self
            .workspace
            .source_cache
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("source workspace borrowed"))?;
        let mut raw_filter = self
            .workspace
            .source_filter
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("source workspace borrowed"))?;
        let mut query = self
            .workspace
            .source_query
            .try_borrow_mut()
            .map_err(|_| Error::temp_storage_context("query workspace borrowed"))?;
        mem.take();
        cache.take();
        filter.take();
        *raw_mem = BudgetedVec::new(&self.memory)?;
        *raw_cache = BudgetedVec::new(&self.memory)?;
        *raw_filter = BudgetedVec::new(&self.memory)?;
        *query = BudgetedVec::new(&self.memory)?;
        self.generation_finished.set(true);
        Ok(())
    }
    /// Parent-owned linear m3 epoch transitions; counters/poison are never reset.
    pub(super) fn m3_transition(&self, expected: u8, next: u8) -> Result<()> {
        self.check_cancelled()?;
        if self.generation_finished.get()
            || self.source_failed.get()
            || self.m3_epoch.get() != expected
        {
            return Err(Error::corrupt_index("invalid m3 epoch transition"));
        }
        if self.source_initialized.get()
            && (self.source_memtable_pool.borrow().is_none()
                || self.source_cache_pool.borrow().is_none()
                || self.source_filter_pool.borrow().is_none())
        {
            return Err(Error::temp_storage_context(
                "m3 epoch has outstanding source loans",
            ));
        }
        self.m3_epoch.set(next);
        Ok(())
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
    ///
    /// Only fixed working-control state is charged: the owner value, its `Arc`
    /// header, and the allocator/box envelope. The configured parent may be
    /// relative; it is resolved to an absolute, cached path for correct cleanup
    /// and cwd-change consistency, but the pathname bytes are excluded from
    /// managed working-memory accounting as file-pathname processing. No OS
    /// pathname length is rejected by a custom memory policy; a genuinely
    /// over-long platform path fails only as an ordinary OS I/O error.
    pub(crate) fn temp_directory(&self) -> Result<Ref<'_, std::path::Path>> {
        if self.directory.borrow().is_none() {
            self.checkpoint(Operation::Create)?;
            let control = self.memory.reserve(directory_owner_control())?;
            let absolute = resolve_absolute_parent(&self.resources.temp_dir)?;
            std::fs::create_dir_all(&absolute).map_err(Error::temp_storage)?;
            let tempdir = tempfile::Builder::new()
                .prefix("srep-offload-")
                .tempdir_in(&absolute)
                .map_err(Error::temp_storage)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(tempdir.path(), std::fs::Permissions::from_mode(0o700))
                    .map_err(Error::temp_storage)?;
            }
            *self.directory.borrow_mut() = Some(Arc::new(DirectoryOwner {
                tempdir,
                absolute_parent: absolute,
                _control: control,
            }));
        }
        Ok(Ref::map(self.directory.borrow(), |d| {
            d.as_ref().expect("initialized directory").path()
        }))
    }
    /// Every temporary-file owner retains this until its handles/path are gone.
    pub(crate) fn directory_owner(&self) -> Result<Arc<DirectoryOwner>> {
        drop(self.temp_directory()?);
        Ok(self
            .directory
            .borrow()
            .as_ref()
            .expect("initialized directory")
            .clone())
    }
    #[cfg(test)]
    pub(crate) fn admit_file(&self) -> Result<TempFileLease> {
        self.check_cancelled()?;
        Ok(TempFileLease {
            bytes: self.temp.reserve(0)?,
            length: 0,
            limit: self.policy.max_file_bytes.min(self.temp.limit()),
            _file: self.files.acquire(1)?,
        })
    }
    #[cfg(test)]
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
            #[cfg(test)]
            lifecycle: self.lifecycle.clone(),
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
    #[cfg(test)]
    pub(crate) fn io_snapshot(&self) -> IoSnapshot {
        *self.io.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    #[cfg(test)]
    pub(crate) fn io_used(&self) -> u128 {
        self.io_snapshot().used_actual
    }
    #[cfg(test)]
    pub(crate) fn io_reserved(&self) -> u128 {
        self.io_snapshot().reserved
    }
    #[cfg(test)]
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
            _shared_control: self.shared_control.clone(),
            #[cfg(test)]
            lifecycle: self.lifecycle.clone(),
            #[cfg(test)]
            fault: self.fault.clone(),
        }
    }
    pub(crate) fn retry_cleanup(&self) -> Result<()> {
        self.cleanup.retry()
    }
    #[cfg(test)]
    pub(crate) fn clear_lifecycle(&self) {
        let mut l = self.lifecycle.lock().unwrap();
        l.events = [None; 32];
        l.len = 0;
    }
    #[cfg(test)]
    pub(crate) fn lifecycle_events(&self) -> Vec<LifecycleEvent> {
        let l = self.lifecycle.lock().unwrap();
        l.events[..l.len].iter().flatten().copied().collect()
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
    _shared_control: Arc<SharedControl>,
    #[cfg(test)]
    fault: Arc<Mutex<[Option<Fault>; 4]>>,
    #[cfg(test)]
    lifecycle: Arc<Mutex<Lifecycle>>,
}
impl SessionIo {
    #[cfg(test)]
    pub(crate) fn note_lifecycle(&self, event: LifecycleEvent) {
        let mut l = self.lifecycle.lock().unwrap();
        let at = l.len;
        if at < l.events.len() {
            l.events[at] = Some(event);
            l.len += 1;
        }
    }
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
        let cleanup = matches!(op, Operation::Truncate | Operation::Remove);
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
    pub(crate) fn defer_cleanup_with_metadata(&self, entry: CleanupEntry, metadata: Reservation) {
        self.cleanup.insert(entry, Some(metadata));
    }
    #[cfg(test)]
    pub(crate) fn retry_cleanup(&self) -> Result<()> {
        self.cleanup.retry()
    }
}
#[derive(Debug)]
pub(crate) struct CleanupEntry {
    pub(crate) path: std::path::PathBuf,
    pub(crate) bytes: Reservation,
    pub(crate) file: ResourceLease,
    pub(crate) directory: Arc<DirectoryOwner>,
}
impl Drop for CleanupEntry {
    fn drop(&mut self) {
        // Owned guards intentionally survive until verified file removal.
        let _ = (&self.bytes, &self.file, &self.directory);
    }
}
#[derive(Debug)]
struct CleanupRegistry {
    entries: Mutex<CleanupSlots>,
    // Kept inside the Arc-owned registry: SessionIo clones may retain this
    // charge after CompressionSession itself has been dropped.
    _control: Reservation,
}
type CleanupSlots = [Option<(CleanupEntry, Option<Reservation>)>; 192];
fn session_control_bytes() -> u64 {
    // CountLedger allocations are made before their Arc values can be stored
    // in CompressionSession. Include both ledger values, both Arc headers, the
    // three initial handle-lease fields, and a small mutex/allocator padding
    // envelope. This is admission metadata, not a replacement for each
    // ledger's runtime limits or the workspace control reservation.
    let ledgers = 2 * std::mem::size_of::<CountLedger>() + std::mem::size_of::<IoLedger>();
    let shared = std::mem::size_of::<SharedControl>();
    let arc_headers = 2 * 2 * std::mem::size_of::<usize>();
    let leases = 3 * std::mem::size_of::<ResourceLease>();
    let envelope = 256usize;
    u64::try_from(
        ledgers
            .saturating_add(shared)
            .saturating_add(arc_headers)
            .saturating_add(leases)
            .saturating_add(envelope),
    )
    .expect("session control size fits u64")
}
#[cfg(test)]
pub(crate) fn shared_control_charge() -> u64 {
    session_control_bytes()
}
fn cleanup_registry_bytes() -> u64 {
    // `Arc::new` adds the strong/weak counters in its allocation header.  The
    // pointer stored in CompressionSession/SessionIo is inline; charge the
    // header plus the complete registry value, including the retained
    // reservation and mutex.  This is intentionally derived from the concrete
    // types rather than the old 8 KiB compact guess (which was smaller than a
    // 192-slot registry on 64-bit targets).
    let value = std::mem::size_of::<CleanupRegistry>();
    let arc_header = 2 * std::mem::size_of::<usize>();
    u64::try_from(value.saturating_add(arc_header)).expect("registry size fits u64")
}
impl CleanupRegistry {
    fn new(control: Reservation) -> Self {
        Self {
            entries: Mutex::new(std::array::from_fn(|_| None)),
            _control: control,
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
    #[cfg(test)]
    lifecycle: Arc<Mutex<Lifecycle>>,
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
        #[cfg(test)]
        {
            let mut l = self.lifecycle.lock().unwrap();
            let at = l.len;
            if at < l.events.len() {
                l.events[at] = Some(LifecycleEvent::EmergencyReturned);
                l.len += 1;
            }
        }
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
    fn compact_registry_control_is_admitted_before_workspace_and_released() {
        let r = ResourceConfig {
            memory: 1024 * 1024,
            ..small()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        assert!(cleanup_registry_bytes() > 8192);
        assert!(c.memory.current() >= cleanup_registry_bytes());
        assert!(s.workspace(WorkspaceKind::Candidate).is_ok());
        drop(s);
        assert_eq!(c.memory.current(), 0);

        let r = ResourceConfig {
            memory: cleanup_registry_bytes().saturating_sub(1),
            ..small()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        assert!(CompressionSession::new(&c, &r, OffloadPolicy::default()).is_err());
        // Admission fails before directory creation or any temporary file.
        assert_eq!(c.memory.current(), 0);
        assert!(!r.temp_dir.join("srep-offload-").exists());
    }

    #[test]
    fn deferred_cleanup_registry_reservation_follows_session_io_owner() {
        let r = small();
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let io = s.io_access();
        let mut spool = crate::codec::TempSpool::new_in_session(&s).unwrap();
        spool.append(&[7; 32]).unwrap();
        s.inject(Operation::Remove, 0, FaultAction::Fail);
        drop(spool);
        drop(s);
        assert!(c.memory.current() >= cleanup_registry_bytes());
        io.retry_cleanup().unwrap();
        drop(io);
        assert_eq!(c.memory.current(), 0);
        assert_eq!(c.temp.current(), 0);
    }

    #[test]
    fn directory_admission_failure_leaves_isolated_root_empty() {
        let root = tempfile::tempdir().unwrap();
        let r = ResourceConfig {
            memory: 128 * 1024,
            temp_dir: root
                .path()
                .join("controlled-long-parent-component-0123456789"),
            ..small()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let held = c
            .memory
            .reserve(c.memory.limit() - c.memory.current())
            .unwrap();
        assert!(s.temp_directory().is_err());
        drop(held);
        drop(s);
        assert_eq!(c.memory.current(), 0);
        if r.temp_dir.exists() {
            assert!(std::fs::read_dir(&r.temp_dir).unwrap().next().is_none());
        }
    }

    #[test]
    fn failed_remove_with_all_remaining_memory_held_uses_preowned_cleanup_charge() {
        let root = tempfile::tempdir().unwrap();
        let r = ResourceConfig {
            temp_dir: root
                .path()
                .join("a-long-controlled-parent-component-0123456789"),
            ..small()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let io = s.io_access();
        let spool = crate::codec::TempSpool::new_in_session(&s).unwrap();
        let path = spool._temp.path().to_path_buf();
        // The pre-owned per-file control charge is fixed working state; it is
        // length-independent and stays live through deferred cleanup.
        let expected_control = file_path_control();
        let before = c.memory.current();
        let held = c
            .memory
            .reserve(c.memory.limit().saturating_sub(c.memory.current()))
            .unwrap();
        s.inject(Operation::Remove, 0, FaultAction::Fail);
        drop(spool);
        assert!(path.exists());
        drop(held);
        let retained = c.memory.current();
        assert!(retained >= cleanup_registry_bytes());
        // The spool's fixed cleanup control charge is part of the retained
        // total; the pathname bytes themselves are excluded.
        assert!(retained >= before);
        assert!(expected_control > 0);
        drop(s);
        io.retry_cleanup().unwrap();
        assert!(!path.exists());
        assert_eq!(c.temp.current(), 0);
        drop(io);
        assert_eq!(c.memory.current(), 0);
    }

    /// Exact io-control admission charged in `new()`. Fixed working control
    /// state only; no pathname bytes are included under the `--memory`
    /// contract.
    fn io_control_charge() -> u64 {
        std::mem::size_of::<IoLedger>() as u64
            + std::mem::size_of::<AtomicBool>() as u64
            + 4 * std::mem::size_of::<usize>() as u64
    }

    /// A long absolute configured parent must not consume managed working
    /// budget: only the recorded configuration length metadata is counted, and
    /// the directory/spool control charges are fixed. Two live spools plus one
    /// live table coexist and the fixed owners stay live through cleanup.
    #[test]
    fn long_absolute_parent_does_not_consume_managed_working_budget() {
        use crate::offload::contracts::CandidateRow;
        use crate::offload::storage::TypedTableBuilder;
        let root = tempfile::tempdir().unwrap();
        let long = root
            .path()
            .join("controlled-long-parent-component-0123456789-abcdefghijklmnopqrstuvwxyz");
        let r = ResourceConfig {
            temp_dir: long.clone(),
            ..small()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let base = c.memory.current();
        let dir_path = s.temp_directory().unwrap().to_path_buf();
        assert!(dir_path.exists());
        {
            let owner = s.directory_owner().unwrap();
            assert!(owner.parent().is_absolute());
            assert_eq!(owner.parent(), long.as_path());
        }
        // Fixed directory control only; the long resolved path adds nothing to
        // managed working memory.
        assert_eq!(c.memory.current(), base + directory_owner_control());
        // Two live spools plus one live table coexist: each adds fixed control.
        let spool_a = crate::codec::TempSpool::new_in_session(&s).unwrap();
        assert_eq!(
            c.memory.current(),
            base + directory_owner_control() + file_path_control()
        );
        let spool_b = crate::codec::TempSpool::new_in_session(&s).unwrap();
        let table = TypedTableBuilder::<CandidateRow>::new(&s, 64).unwrap();
        assert_eq!(
            c.memory.current(),
            base + directory_owner_control() + 3 * file_path_control()
        );
        // A much longer pathname leaves the managed charge identical.
        assert!(long.as_os_str().len() > 32);
        // Clean table drop releases exactly its fixed control.
        drop(table);
        s.retry_cleanup().unwrap();
        assert_eq!(
            c.memory.current(),
            base + directory_owner_control() + 2 * file_path_control()
        );
        // Open-failure path: no file may survive and memory returns to the
        // pre-spool level after cleanup.
        let pre_fail = c.memory.current();
        s.inject(Operation::Open, 0, FaultAction::Fail);
        assert!(crate::codec::TempSpool::new_in_session(&s).is_err());
        s.retry_cleanup().unwrap();
        assert_eq!(c.memory.current(), pre_fail);
        assert_eq!(c.temp.current(), 0);
        // Clean drop of one spool, then remove-failure on the other with all
        // remaining memory held: the survivor defers into the pre-owned fixed
        // control registry charge. Assert the exact retained total after
        // session drop, then zero after cleanup.
        let path_b = spool_b._temp.path().to_path_buf();
        drop(spool_b);
        assert!(!path_b.exists());
        assert_eq!(
            c.memory.current(),
            base + directory_owner_control() + file_path_control()
        );
        let held = c
            .memory
            .reserve(c.memory.limit().saturating_sub(c.memory.current()))
            .unwrap();
        s.inject(Operation::Remove, 0, FaultAction::Fail);
        let path_a = spool_a._temp.path().to_path_buf();
        drop(spool_a);
        assert!(path_a.exists());
        drop(held);
        assert_eq!(
            c.memory.current(),
            base + directory_owner_control() + file_path_control()
        );
        let io = s.io_access();
        drop(s);
        let retained = c.memory.current();
        // Complete retained charges: shared control + io control + registry
        // + fixed directory control + one deferred spool fixed control.
        let expected = shared_control_charge()
            + io_control_charge()
            + cleanup_registry_bytes()
            + directory_owner_control()
            + file_path_control();
        assert_eq!(retained, expected);
        io.retry_cleanup().unwrap();
        assert!(!path_a.exists());
        assert_eq!(c.temp.current(), 0);
        drop(io);
        assert_eq!(c.memory.current(), 0);
    }

    /// Short relative configured parent under a long current directory.
    /// Changing the process-wide CWD is unsafe under parallel tests, so the
    /// CWD-dependent half runs in a subprocess (`Command::current_dir`) that
    /// re-invokes this test binary for the child entry below. The resolved
    /// absolute path may be long; that must NOT consume managed working budget
    /// and must NOT produce a custom MemoryBudgetExceeded. A genuinely
    /// over-long OS path may only fail as an ordinary I/O error.
    #[test]
    fn short_relative_parent_under_long_cwd_ignores_path_length_for_budget() {
        use std::process::Command;
        if std::env::var_os("SREP_PATH_CHILD").is_some() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        // A definite, unambiguous absolute parent; the budget must ignore its
        // length entirely (the removed 4096 allowance no longer exists). This
        // is intentionally a moderate single-component fixture, not a >4096
        // CWD: the point is that path length never reaches the memory ledger.
        let long_cwd = root
            .path()
            .join("long-cwd-component-0123456789-abcdefghijklmnopqrstuvwxyz-0123456789");
        std::fs::create_dir_all(&long_cwd).unwrap();
        let exe = std::env::current_exe().unwrap();
        let status_path = root.path().join("child-status.txt");
        let output = Command::new(&exe)
            .arg("--exact")
            .arg("offload::session::tests::short_relative_parent_child_entry")
            .arg("--nocapture")
            .env("SREP_PATH_CHILD", "1")
            .env("SREP_PATH_STATUS", &status_path)
            .env("SREP_PATH_REL", "r")
            .current_dir(&long_cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let status = std::fs::read_to_string(&status_path).unwrap();
        let mut retained = None;
        for line in status.lines() {
            let (k, v) = line.split_once('=').unwrap();
            match k {
                "retained" => retained = Some(v.parse::<u64>().unwrap()),
                "resolved_parent_verified" => assert_eq!(v, "1"),
                _ => panic!("unexpected status {line}"),
            }
        }
        let retained = retained.unwrap();
        // Retained charges are fixed control only, independent of the long
        // resolved path length.
        let expected = shared_control_charge()
            + io_control_charge()
            + cleanup_registry_bytes()
            + directory_owner_control()
            + file_path_control();
        assert_eq!(retained, expected);
    }

    /// Subprocess child entry for the test above. Never invoked directly by
    /// the suite; the parent re-invokes the test binary with SREP_PATH_CHILD
    /// set and the CWD pointed at a long directory.
    #[test]
    fn short_relative_parent_child_entry() {
        if std::env::var_os("SREP_PATH_CHILD").is_none() {
            return;
        }
        let rel = std::env::var_os("SREP_PATH_REL").unwrap();
        let status_path = std::env::var_os("SREP_PATH_STATUS").unwrap();
        let r = ResourceConfig {
            temp_dir: std::path::PathBuf::from(rel.clone()),
            ..small()
        };
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let base = c.memory.current();
        s.temp_directory().unwrap();
        let owner = s.directory_owner().unwrap();
        assert!(owner.parent().is_absolute());
        // Check the path contract using the child process's actual CWD rather
        // than assumptions about textual path length (macOS may expose a
        // symlink-resolved /private prefix, and Windows has verbatim forms).
        let expected_parent = std::env::current_dir().unwrap().join(&rel);
        assert_eq!(owner.parent(), expected_parent.as_path());
        // The resolved parent is long, but managed working memory only counts
        // the fixed directory control.
        assert!(owner.parent().as_os_str().len() > 64);
        assert_eq!(c.memory.current(), base + directory_owner_control());
        drop(owner);
        let spool = crate::codec::TempSpool::new_in_session(&s).unwrap();
        assert_eq!(
            c.memory.current(),
            base + directory_owner_control() + file_path_control()
        );
        // Fail removal with all remaining memory held, then record the exact
        // retained total after session drop for the parent to verify.
        let held = c
            .memory
            .reserve(c.memory.limit().saturating_sub(c.memory.current()))
            .unwrap();
        s.inject(Operation::Remove, 0, FaultAction::Fail);
        let path = spool._temp.path().to_path_buf();
        drop(spool);
        assert!(path.exists());
        drop(held);
        let io = s.io_access();
        drop(s);
        let retained = c.memory.current();
        std::fs::write(
            &status_path,
            format!("resolved_parent_verified=1\nretained={retained}\n"),
        )
        .unwrap();
        io.retry_cleanup().unwrap();
        assert!(!path.exists());
        assert_eq!(c.temp.current(), 0);
        drop(io);
        assert_eq!(c.memory.current(), 0);
    }

    /// Managed working-memory usage is independent of path length for the same
    /// active object count. The fixed-control charge must be identical whether
    /// the parent is short or long.
    #[test]
    fn managed_usage_is_independent_of_path_length_for_same_object_count() {
        let short_root = tempfile::tempdir().unwrap();
        let long_root = tempfile::tempdir().unwrap();
        // Build a distinctly longer, but portable, absolute parent.  Keep the
        // total below conservative native pathname limits: the assertion is
        // about accounting independence, not exercising OS path limits.
        let mut long_parent = long_root.path().to_path_buf();
        for _ in 0..8 {
            long_parent.push("long-component-0123456789-abcdefghijklmnopqrstuvwxyz");
        }
        assert!(long_parent.as_os_str().len() > short_root.path().as_os_str().len());
        std::fs::create_dir_all(&long_parent).unwrap();
        let short = ResourceConfig {
            temp_dir: short_root.path().to_path_buf(),
            ..small()
        };
        let long = ResourceConfig {
            temp_dir: long_parent,
            ..small()
        };
        let mut baseline_delta = None;
        for r in [&short, &long] {
            let c = ResourceContext::with_resources(r).unwrap();
            let s = CompressionSession::new(&c, r, OffloadPolicy::default()).unwrap();
            let base = c.memory.current();
            let delta = {
                let _s1 = crate::codec::TempSpool::new_in_session(&s).unwrap();
                let _s2 = crate::codec::TempSpool::new_in_session(&s).unwrap();
                c.memory.current() - base
            };
            match baseline_delta {
                None => baseline_delta = Some(delta),
                Some(first) => assert_eq!(delta, first),
            }
            drop(s);
            assert_eq!(c.memory.current(), 0);
            assert_eq!(c.temp.current(), 0);
        }
        assert_eq!(
            baseline_delta,
            Some(directory_owner_control() + 2 * file_path_control())
        );
    }

    #[test]
    fn ledger_arc_owners_retain_shared_control_after_session_drop() {
        let r = small();
        let c = ResourceContext::with_resources(&r).unwrap();
        let (before, lease) = {
            let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
            let before = c.memory.current();
            let lease = s.acquire_handles(1).unwrap();
            drop(s);
            (before, lease)
        };
        let retained_with_lease = c.memory.current();
        // Retained shared control is fixed and path-length independent: the
        // shared-control charge plus the fixed io-control charge.
        let expected = shared_control_charge() + io_control_charge();
        assert_eq!(retained_with_lease, expected);
        drop(lease);
        assert_eq!(c.memory.current(), 0);
        assert!(before > retained_with_lease);

        let c = ResourceContext::with_resources(&r).unwrap();
        let permit = {
            let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
            let permit = s.reserve_io(7).unwrap();
            drop(s);
            permit
        };
        let retained_with_io = c.memory.current();
        assert_eq!(retained_with_io, expected);
        drop(permit);
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

    #[test]
    fn source_loans_return_identical_allocations_and_exclude_concurrent_owners() {
        let r = small();
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let (mp, cp, fp, used) = {
            let m = s.source_memtable().unwrap();
            let mut cache = s.source_cache().unwrap();
            let mut filter = s.source_filter().unwrap();
            assert!(s.source_memtable().is_err());
            assert!(s.source_cache().is_err());
            assert!(s.source_filter().is_err());
            let n = cache.capacity();
            cache.resize(n, 99).unwrap();
            let n = filter.capacity();
            filter.resize(n, 255).unwrap();
            (
                m.as_slice().as_ptr(),
                cache.as_slice().as_ptr(),
                filter.as_slice().as_ptr(),
                c.memory.current(),
            )
        };
        assert_eq!(c.memory.current(), used);
        {
            let m = s.source_memtable().unwrap();
            let mut cache = s.source_cache().unwrap();
            let mut filter = s.source_filter().unwrap();
            assert_eq!(m.as_slice().as_ptr(), mp);
            assert_eq!(cache.as_slice().as_ptr(), cp);
            assert_eq!(filter.as_slice().as_ptr(), fp);
            assert!(m.is_empty());
            assert!(cache.is_empty());
            assert!(filter.is_empty());
            let n = cache.capacity();
            cache.resize(n, 0).unwrap();
            let n = filter.capacity();
            filter.resize(n, 0).unwrap();
            assert!(cache.iter().all(|b| *b == 0));
            assert!(filter.iter().all(|b| *b == 0));
            let _candidate = s.workspace(WorkspaceKind::Candidate).unwrap();
            assert_eq!(c.memory.current(), used);
        }
        drop(s);
        assert_eq!(c.memory.current(), 0);
    }
}
