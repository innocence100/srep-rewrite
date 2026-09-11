#![forbid(unsafe_code)]

//! SREP-NG version 2 library.
//!
//! The writer emits self-contained NG v2 archives and the reader strictly
//! supports embedded legacy SREP v1-v4 archives. Match IR, all three reference
//! layouts, and m0-m5 discovery use the deterministic RAM-or-spill
//! CandidateIndex.
//! Prototype NG v1 archives are rejected as
//! [`ErrorKind::UnsupportedVersion`].

pub mod candidate_index;
pub mod checksum;
pub mod codec;
pub mod config;
pub mod dispatch;
pub mod error;
pub mod format;
pub mod legacy;
pub mod match_finder;
pub mod match_ir;
pub mod path;
mod polynomial;
mod reference;
pub mod requirement;
pub mod resource;

pub use candidate_index::{
    CandidateIndex, HybridCandidateIndex, IndexEntry, RamCandidateIndex, ScratchHeader,
};
pub use codec::{
    ArchiveInfo, CompressionStats, compress, compress_with_candidates,
    compress_with_candidates_with_context, compress_with_context, decompress,
    decompress_with_context, decompress_with_resources, inspect, inspect_matches,
    inspect_matches_with_context, inspect_matches_with_resources, inspect_with_resources, verify,
    verify_with_resources,
};
pub use config::{
    Checksum, CompressionConfig, Config, Layout, Method, RepConfig, ResourceConfig, m5_seed_size,
    parse_size,
};
pub use error::{Error, ErrorKind, Result};
pub use match_finder::{
    DataSource, find_matches_m0, find_matches_m0_with_context, find_matches_m0_with_resources,
    find_matches_m1, find_matches_m1_with_context, find_matches_m1_with_resources, find_matches_m2,
    find_matches_m2_with_context, find_matches_m2_with_resources, find_matches_m3,
    find_matches_m3_with_context, find_matches_m3_with_resources, find_matches_m4,
    find_matches_m4_with_context, find_matches_m4_with_resources, find_matches_m5,
    find_matches_m5_with_context, find_matches_m5_with_resources, packed_slice_metadata,
};
pub use match_ir::{
    InspectedMatches, Match, MatchCandidate, NormalizedMatches, normalize_matches,
    normalize_matches_with_budget,
};
pub use polynomial::polynomial_hash;
pub use resource::{BudgetedVec, MemoryBudget, ResourceContext, TempBudget};
