use std::io::{self, Write};
use std::ops::{Deref, DerefMut, Index, IndexMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

#[derive(Debug)]
struct BudgetState {
    current: AtomicU64,
    high_water: AtomicU64,
    limit: u64,
}

impl BudgetState {
    fn new(limit: u64) -> Self {
        Self {
            current: AtomicU64::new(0),
            high_water: AtomicU64::new(0),
            limit,
        }
    }

    fn reserve(&self, amount: u64, kind: BudgetKind) -> Result<()> {
        let mut current = self.current.load(Ordering::Relaxed);
        loop {
            let next = current.checked_add(amount).ok_or_else(|| kind.error())?;
            if next > self.limit {
                return Err(kind.error());
            }
            match self.current.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    let mut high = self.high_water.load(Ordering::Relaxed);
                    while next > high {
                        match self.high_water.compare_exchange_weak(
                            high,
                            next,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(observed) => high = observed,
                        }
                    }
                    return Ok(());
                }
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, amount: u64) {
        self.current.fetch_sub(amount, Ordering::AcqRel);
    }

    fn current(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }

    fn high_water(&self) -> u64 {
        self.high_water.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug)]
enum BudgetKind {
    Memory,
    Temp,
}

impl BudgetKind {
    fn error(self) -> Error {
        match self {
            Self::Memory => Error::memory_limit("working memory budget exceeded"),
            Self::Temp => Error::temp_limit("temporary budget exceeded"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MemoryBudget {
    state: Arc<BudgetState>,
}

pub const fn checked_layout_bytes(block_count: u64) -> Option<u64> {
    block_count.checked_mul(64)
}

impl MemoryBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            state: Arc::new(BudgetState::new(limit)),
        }
    }

    pub fn reserve(&self, amount: u64) -> Result<Reservation> {
        self.state.reserve(amount, BudgetKind::Memory)?;
        Ok(Reservation {
            state: Arc::clone(&self.state),
            amount,
            kind: BudgetKind::Memory,
        })
    }

    pub fn current(&self) -> u64 {
        self.state.current()
    }

    pub fn high_water(&self) -> u64 {
        self.state.high_water()
    }

    pub fn limit(&self) -> u64 {
        self.state.limit
    }
}

#[derive(Clone, Debug)]
pub struct TempBudget {
    state: Arc<BudgetState>,
}

impl TempBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            state: Arc::new(BudgetState::new(limit)),
        }
    }

    pub fn reserve(&self, amount: u64) -> Result<Reservation> {
        self.state.reserve(amount, BudgetKind::Temp)?;
        Ok(Reservation {
            state: Arc::clone(&self.state),
            amount,
            kind: BudgetKind::Temp,
        })
    }

    pub fn current(&self) -> u64 {
        self.state.current()
    }

    pub fn high_water(&self) -> u64 {
        self.state.high_water()
    }

    pub fn limit(&self) -> u64 {
        self.state.limit
    }
}

#[derive(Debug)]
pub struct Reservation {
    state: Arc<BudgetState>,
    amount: u64,
    kind: BudgetKind,
}

pub struct BudgetedWriter<W> {
    inner: W,
    reservation: Reservation,
    budget_error: Option<Error>,
}

pub struct BudgetedVec<T> {
    data: Vec<T>,
    budget: MemoryBudget,
    reservation: Reservation,
}

/// Owning iterator that keeps the source allocation charged until it is
/// dropped. The iterator field comes first so its allocation is released
/// before the reservation.
pub struct BudgetedVecIntoIter<T> {
    iter: std::vec::IntoIter<T>,
    _reservation: Reservation,
}

impl<T> std::fmt::Debug for BudgetedVec<T>
where
    T: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(&self.data).finish()
    }
}

impl<T> BudgetedVec<T> {
    pub fn new(budget: &MemoryBudget) -> Result<Self> {
        Ok(Self {
            data: Vec::new(),
            budget: budget.clone(),
            reservation: budget.reserve(0)?,
        })
    }

    pub fn with_capacity(capacity: usize, budget: &MemoryBudget) -> Result<Self> {
        let mut result = Self::new(budget)?;
        result.ensure_capacity(capacity)?;
        Ok(result)
    }

    pub fn push(&mut self, value: T) -> Result<()> {
        let target = self
            .len()
            .checked_add(1)
            .ok_or_else(|| Error::memory_limit("buffer length overflows"))?;
        if target > self.capacity() {
            let doubled = self.capacity().saturating_mul(2).max(1);
            self.ensure_capacity(target.max(doubled))?;
        }
        self.data.push(value);
        Ok(())
    }

    pub(crate) fn insert(&mut self, index: usize, value: T) -> Result<()> {
        if index > self.len() {
            return Err(Error::memory_limit("buffer insertion index is invalid"));
        }
        self.ensure_capacity(
            self.len()
                .checked_add(1)
                .ok_or_else(|| Error::memory_limit("buffer length overflows"))?,
        )?;
        self.data.insert(index, value);
        Ok(())
    }

    pub(crate) fn insert_without_growth(&mut self, index: usize, value: T) -> Result<()> {
        if index > self.len() {
            return Err(Error::memory_limit("buffer insertion index is invalid"));
        }
        if self.len() == self.capacity() {
            return Err(Error::memory_limit(
                "buffer insertion would require allocation",
            ));
        }
        self.data.insert(index, value);
        Ok(())
    }

    pub(crate) fn remove(&mut self, index: usize) -> T {
        self.data.remove(index)
    }

    pub fn extend_from_slice(&mut self, values: &[T]) -> Result<()>
    where
        T: Clone,
    {
        let target = self
            .len()
            .checked_add(values.len())
            .ok_or_else(|| Error::memory_limit("buffer length overflows"))?;
        self.ensure_capacity(target)?;
        self.data.extend_from_slice(values);
        Ok(())
    }

    pub fn push_bytes(&mut self, bytes: &[T]) -> Result<()>
    where
        T: Clone,
    {
        self.extend_from_slice(bytes)
    }

    pub fn resize(&mut self, new_len: usize, value: T) -> Result<()>
    where
        T: Clone,
    {
        self.ensure_capacity(new_len)?;
        self.data.resize(new_len, value);
        Ok(())
    }

    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.data
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.data.iter()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }

    pub fn reserved_bytes(&self) -> u64 {
        u64::try_from(self.capacity())
            .ok()
            .and_then(|capacity| capacity.checked_mul(std::mem::size_of::<T>() as u64))
            .unwrap_or(u64::MAX)
    }

    pub fn budget_limit(&self) -> u64 {
        self.budget.state.limit
    }

    pub fn available_bytes(&self) -> u64 {
        self.reservation.available()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub(crate) fn budget(&self) -> &MemoryBudget {
        &self.budget
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.data.get(index)
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.data.get_mut(index)
    }

    pub fn dedup_by<F>(&mut self, same_bucket: F)
    where
        F: FnMut(&mut T, &mut T) -> bool,
    {
        self.data.dedup_by(same_bucket);
    }

    fn ensure_capacity(&mut self, desired: usize) -> Result<()> {
        if desired <= self.capacity() || std::mem::size_of::<T>() == 0 {
            return Ok(());
        }
        let element_size = u64::try_from(std::mem::size_of::<T>())
            .map_err(|_| Error::memory_limit("element size exceeds platform limits"))?;
        let available = self.reservation.available();
        let requested_bytes = u64::try_from(desired)
            .ok()
            .and_then(|elements| elements.checked_mul(element_size))
            .ok_or_else(|| Error::memory_limit("buffer growth overflows"))?;
        if requested_bytes > available {
            return Err(Error::memory_limit(
                "buffer growth requires retaining the existing allocation",
            ));
        }

        // Keep the old Vec and its reservation live while the replacement is
        // allocated. Reserving all remaining budget makes allocator
        // over-allocation observable before any element is moved.
        let mut replacement_reservation = self.budget.reserve(available)?;
        let mut replacement = Vec::new();
        if let Err(error) = replacement.try_reserve_exact(desired) {
            drop(replacement_reservation);
            return Err(Error::memory_limit(format!(
                "buffer allocation failed: {error}"
            )));
        }
        let replacement_bytes = match u64::try_from(replacement.capacity())
            .ok()
            .and_then(|capacity| capacity.checked_mul(element_size))
        {
            Some(replacement_bytes) => replacement_bytes,
            None => {
                drop(replacement);
                return Err(Error::memory_limit("buffer capacity overflows"));
            }
        };
        if replacement_bytes > available {
            drop(replacement);
            drop(replacement_reservation);
            return Err(Error::memory_limit(
                "allocator capacity exceeds memory budget",
            ));
        }

        let mut old_data = std::mem::replace(&mut self.data, replacement);
        self.data.append(&mut old_data);
        drop(old_data);

        replacement_reservation.shrink(available - replacement_bytes);
        let old_reservation = std::mem::replace(&mut self.reservation, replacement_reservation);
        drop(old_reservation);
        Ok(())
    }
}

impl<T> Deref for BudgetedVec<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl<T> DerefMut for BudgetedVec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut_slice()
    }
}

impl<'a, T> IntoIterator for &'a BudgetedVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T> IntoIterator for BudgetedVec<T> {
    type Item = T;
    type IntoIter = BudgetedVecIntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        let Self {
            data,
            budget: _,
            reservation,
        } = self;
        BudgetedVecIntoIter {
            iter: data.into_iter(),
            _reservation: reservation,
        }
    }
}

impl<T> Iterator for BudgetedVecIntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<T> DoubleEndedIterator for BudgetedVecIntoIter<T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.iter.next_back()
    }
}

impl<T> ExactSizeIterator for BudgetedVecIntoIter<T> {}
impl<T> std::iter::FusedIterator for BudgetedVecIntoIter<T> {}

impl<T> Index<usize> for BudgetedVec<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        &self.data[index]
    }
}

impl<T> IndexMut<usize> for BudgetedVec<T> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.data[index]
    }
}

impl<T: PartialEq> PartialEq for BudgetedVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<T: Eq> Eq for BudgetedVec<T> {}

impl<T: PartialEq> PartialEq<Vec<T>> for BudgetedVec<T> {
    fn eq(&self, other: &Vec<T>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: PartialEq> PartialEq<BudgetedVec<T>> for Vec<T> {
    fn eq(&self, other: &BudgetedVec<T>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<W> BudgetedWriter<W> {
    pub fn new(inner: W, budget: &TempBudget) -> Result<Self> {
        Ok(Self {
            inner,
            reservation: budget.reserve(0)?,
            budget_error: None,
        })
    }

    pub fn with_reservation(inner: W, reservation: Reservation) -> Self {
        Self {
            inner,
            reservation,
            budget_error: None,
        }
    }

    pub fn into_parts(self) -> (W, Reservation) {
        (self.inner, self.reservation)
    }

    pub fn budget_error(&self) -> Option<&Error> {
        self.budget_error.as_ref()
    }

    pub fn take_budget_error(&mut self) -> Option<Error> {
        self.budget_error.take()
    }

    pub fn inner(&self) -> &W {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.inner
    }
}

impl<W: Write> Write for BudgetedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Err(error) = self
            .reservation
            .grow(u64::try_from(bytes.len()).map_err(|_| io::Error::other("write too large"))?)
        {
            self.budget_error = Some(error);
            return Err(io::Error::other("temporary budget exceeded"));
        }
        match self.inner.write(bytes) {
            Ok(written) => {
                let unused = bytes.len().saturating_sub(written);
                self.reservation
                    .shrink(u64::try_from(unused).unwrap_or(u64::MAX));
                Ok(written)
            }
            Err(error) => {
                self.reservation
                    .shrink(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Reservation {
    pub fn grow(&mut self, amount: u64) -> Result<()> {
        self.state.reserve(amount, self.kind)?;
        self.amount = self
            .amount
            .checked_add(amount)
            .ok_or_else(|| self.kind.error())?;
        Ok(())
    }

    pub fn shrink(&mut self, amount: u64) {
        let released = amount.min(self.amount);
        self.amount -= released;
        self.state.release(released);
    }

    fn available(&self) -> u64 {
        self.state.limit.saturating_sub(self.state.current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_enforce_and_release_budget() {
        let budget = MemoryBudget::new(10);
        let mut first = budget.reserve(6).unwrap();
        assert!(budget.reserve(5).is_err());
        first.shrink(2);
        let second = budget.reserve(5).unwrap();
        assert_eq!(budget.high_water(), 9);
        assert_eq!(budget.current(), 9);
        drop(second);
    }

    #[test]
    fn owned_iterator_keeps_allocation_charged_until_drop() {
        let budget = MemoryBudget::new(64);
        let mut values = BudgetedVec::with_capacity(4, &budget).unwrap();
        values.resize(4, 0u64).unwrap();
        assert_eq!(budget.current(), 32);
        let mut iterator = values.into_iter();
        assert_eq!(iterator.next(), Some(0));
        assert!(budget.reserve(33).is_err());
        assert_eq!(iterator.next(), Some(0));
        assert_eq!(iterator.next(), Some(0));
        assert_eq!(iterator.next(), Some(0));
        assert_eq!(iterator.next(), None);
        assert!(budget.reserve(33).is_err());
        drop(iterator);
        assert_eq!(budget.current(), 0);
        assert!(budget.reserve(32).is_ok());
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.state.release(self.amount);
    }
}

#[derive(Clone, Debug)]
pub struct ResourceContext {
    pub memory: MemoryBudget,
    pub temp: TempBudget,
    pub temp_dir: std::path::PathBuf,
    pub candidate_index_memtable_bytes: Option<u64>,
    pub candidate_index_spills: Arc<AtomicU64>,
}

impl ResourceContext {
    pub fn from_limits(memory: u64, temp: u64) -> Self {
        Self {
            memory: MemoryBudget::new(memory),
            temp: TempBudget::new(temp),
            temp_dir: std::env::temp_dir(),
            candidate_index_memtable_bytes: None,
            candidate_index_spills: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn with_resources(resources: &crate::config::ResourceConfig) -> Result<Self> {
        if resources.memory == 0 {
            return Err(Error::memory_limit("memory limit must be positive"));
        }
        if resources.temp_limit == 0 {
            return Err(Error::temp_limit("temporary limit must be positive"));
        }
        let mut context = Self::from_limits(resources.memory, resources.temp_limit);
        context.temp_dir = resources.temp_dir.clone();
        Ok(context)
    }
}

impl ResourceContext {
    pub fn validate(&self) -> Result<()> {
        if self.memory.current() != 0 || self.temp.current() != 0 {
            return Err(Error::invalid_config("resource context is already in use"));
        }
        Ok(())
    }

    pub(crate) fn note_candidate_index_spill(&self) {
        self.candidate_index_spills.fetch_add(1, Ordering::Relaxed);
    }

    pub fn candidate_index_spill_count(&self) -> u64 {
        self.candidate_index_spills.load(Ordering::Acquire)
    }
}
