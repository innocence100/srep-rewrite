//! Private bounded-memory compression contracts.
//!
//! This module is intentionally not part of the public API.  The 0.2
//! implementation is assembled behind these owned-row and fallible-cursor
//! contracts so the finder, storage, scheduler, and writer stages cannot
//! accidentally exchange borrowed data from an evictable cache.

pub(crate) mod candidate_sink;
pub(crate) mod contracts;
pub(crate) mod normalize;
pub(crate) mod session;
pub(crate) mod sort;
pub(crate) mod storage;

#[cfg(test)]
mod tests;
