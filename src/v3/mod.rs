//! SREP-NG v3 shared module contract.
//!
//! Spec baseline (unchanged): `docs/FORMAT-V3.md`, SHA-256
//! `2f26772be0c1e12325021c0ba99961a882139b6bc9ad60562374055963c7606c`
//! (745 lines, review PASS ROUND 4/10). This assignment owns only the common
//! wire primitives in [`crate::format_v3`] and the types/signatures below.
//! The writer and reader share only these wire-level contracts.
//!
//! # File split
//!
//! * Shared (this assignment): [`crate::format_v3`], this module.
//! * Writer (Grok): `src/v3/writer.rs` plus any later `src/reference`
//!   visibility changes for candidate validation. Writer may spool input.
//! * Reader (Luna): `src/v3/reader.rs` plus any new disjoint store types.
//!   Do not reuse the removed NGv2 reference stores in place.
//! * External wire vectors (DeepSeek): `tests/v3_wire.rs` against
//!   [`crate::format_v3`] APIs. Not created here.
//! * Integration: `src/codec.rs`, `src/dispatch.rs`, `src/main.rs`, and the
//!   public crate re-exports route the default APIs to this implementation.
//!
//! Writer and reader modules implement complete archive operations. Do not add
//! `unimplemented!` scaffolding that could be called as a working codec.
//!
//! # I/O and hashing rules
//!
//! * Writer may copy input into an internal temporary spool.
//! * Index-LZ decoder: seek the tail, or spool non-seekable *input* first.
//! * Future-LZ and I/O-LZ decoder: sequential body parse then the fixed
//!   tail. MUST NOT force an input spool merely to find the tail. Future MAY
//!   stream a verified block to a non-seekable output; a later global digest
//!   failure can then occur after partial stdout. Index-LZ and I/O-LZ public
//!   output uses a verified internal temporary spool.
//! * Encoded digest hashes physical archive bytes in on-disk order, domain
//!   [`crate::format_v3::ENCODED_DOMAIN`] then `archive[0 .. tail_start+44+w]`.
//!   Stream that prefix through [`crate::format_v3::GlobalDigest::encoded`].
//! * Do not leak Future collector/pending types from this shared module.
//! * `max_distance == 0` means complete history. A nonzero value is an
//!   inclusive cap. Decoded base matches are checked only against that global
//!   cap, not against `rep_distance`.
//! * v3 matching MUST call the public `find_matches_m*_with_context` APIs or
//!   the internal full finder. Do not call `*_spooled_compact` (m0/fixed
//!   compact skip) and do not use a destination-only `match_ir` filter.
//!   Finder sources are not edited in this assignment.
//!
//! # ErrorKind
//!
//! Use only existing [`crate::error::ErrorKind`] values from the spec
//! §10.5 mapping (`InvalidConfiguration`, `UnsupportedVersion`,
//! `UnknownChecksum`, `CorruptHeader`, `TruncatedArchive`, `CorruptRecord`,
//! `CorruptIndex`, `InvalidMatch`, `ChecksumMismatch`, resource/I/O kinds).
//!
//! # Exact function signatures (implemented in writer.rs / reader.rs)
//!
//! Writer entry points used by the codec integration:
//!
//! ```ignore
//! pub(crate) fn compress_spooled_with_candidates<W: std::io::Write>(
//!     spool: crate::codec::InputSpool,
//!     config: &crate::config::CompressionConfig,
//!     candidates: crate::resource::BudgetedVec<crate::match_ir::MatchCandidate>,
//!     context: &crate::resource::ResourceContext,
//!     output: W,
//! ) -> crate::error::Result<crate::codec::CompressionStats>;
//!
//! pub fn compress_with_candidates<R, W, I>(
//!     input: R,
//!     output: W,
//!     config: &crate::config::CompressionConfig,
//!     candidates: I,
//! ) -> crate::error::Result<crate::codec::CompressionStats>
//! where
//!     R: std::io::Read,
//!     W: std::io::Write,
//!     I: IntoIterator<Item = crate::match_ir::MatchCandidate>;
//!
//! pub fn compress_with_candidates_with_context<R, W, I>(
//!     input: R,
//!     output: W,
//!     config: &crate::config::CompressionConfig,
//!     candidates: I,
//!     context: &crate::resource::ResourceContext,
//! ) -> crate::error::Result<crate::codec::CompressionStats>
//! where
//!     R: std::io::Read,
//!     W: std::io::Write,
//!     I: IntoIterator<Item = crate::match_ir::MatchCandidate>;
//! ```
//!
//! `compress_with_candidates` validates `config`, builds a
//! [`crate::resource::ResourceContext`] from `config.resources` (no hidden
//! default), spools input, and forwards to
//! `compress_spooled_with_candidates`. Candidate byte validation stays in
//! the writer assignment (`candidate_validation::validate_candidates` owns
//! that boundary and is not part of this shared wire module).
//!
//! Decoder APIs take a **full archive** starting at byte 0. Dispatch reads
//! the 8-byte magic outside this module and MUST prepend those bytes (and
//! any further consumed header prefix) before calling v3. Do not accept a
//! body-only reader.
//!
//! ```ignore
//! pub fn decode<R: std::io::Read, W: std::io::Write>(
//!     input: R,
//!     output: W,
//!     resources: &crate::config::ResourceConfig,
//!     context: &crate::resource::ResourceContext,
//!     write_output: bool,
//! ) -> crate::error::Result<crate::codec::CompressionStats>;
//!
//! pub fn inspect<R: std::io::Read>(
//!     input: R,
//!     resources: &crate::config::ResourceConfig,
//!     context: &crate::resource::ResourceContext,
//! ) -> crate::error::Result<crate::codec::ArchiveInfo>;
//!
//! pub fn inspect_matches<R: std::io::Read>(
//!     input: R,
//!     resources: &crate::config::ResourceConfig,
//!     context: &crate::resource::ResourceContext,
//! ) -> crate::error::Result<crate::match_ir::InspectedMatches>;
//!
//! pub(crate) fn decode_archive<R: std::io::Read, W: std::io::Write>(
//!     input: R,
//!     output: W,
//!     resources: &crate::config::ResourceConfig,
//!     context: &crate::resource::ResourceContext,
//!     write_output: bool,
//!     collect_matches: bool,
//! ) -> crate::error::Result<V3Decoded>;
//! ```
//!
//! `decode_archive` is the single reconstruction path. It reserves metadata
//! and output-verification memory through RAII [`crate::resource::BudgetedVec`]
//! / temp spools. `inspect` / `inspect_matches` MUST NOT copy reconstructed
//! plaintext to a public output (`write_output = false`). `inspect_matches`
//! sets `collect_matches = true` and returns [`crate::match_ir::InspectedMatches`]
//! built with [`inspected_matches`]. History used for overlapping copies is
//! not cloned into `V3Decoded`.
//!
//! [`crate::match_ir::InspectedMatches`] has a public `matches` field and no
//! `new` in `match_ir`; v3 constructs it only through [`inspected_matches`].
//! Header-to-config conversion is
//! [`crate::format_v3::ArchiveHeader::to_compression_config`], which requires
//! an explicit [`crate::config::ResourceConfig`] and never substitutes
//! `ResourceConfig::default()`.

pub mod reader;
pub mod writer;

use crate::codec::{ArchiveInfo, CompressionStats};
use crate::format_v3::{ArchiveHeader, VERSION};
use crate::match_ir::{InspectedMatches, Match};
use crate::resource::BudgetedVec;

pub use crate::format_v3::{
    ArchiveHeader as V3ArchiveHeader, ArchiveTail, Crc32c, DerivedLayout, ENCODED_DOMAIN,
    GlobalDigest, HEADER_LEN, IO_LITERAL_TAG, IO_MATCH_TAG, MAX_ULEB128_LEN, MAX_WIRE, NG_V3_MAGIC,
    PLAINTEXT_DOMAIN, POSITIVE_GAIN_MIN_LEN, TAIL_LEN_BLAKE3, TAIL_LEN_XXH3, TAIL_MAGIC,
    TAIL_PREFIX_LEN, VERSION as V3_VERSION, crc32c, crc32c_le, decode_compact_index,
    decode_uleb128, decode_uleb128_index, encode_archive_header, encode_archive_tail,
    encode_compact_index, encode_uleb128_into, parse_archive_header, parse_archive_tail,
    plaintext_digest, tail_len, validate_tail_layout,
};

/// Shared decoder result: archive metadata plus optional owned match IR.
///
/// History / reconstructed plaintext is not stored here. Public output is a
/// separate `Write` / verified spool owned by the reader assignment.
#[derive(Debug)]
pub struct V3Decoded {
    pub info: ArchiveInfo,
    pub stats: CompressionStats,
    pub matches: Option<InspectedMatches>,
}

impl V3Decoded {
    pub fn new(
        info: ArchiveInfo,
        stats: CompressionStats,
        matches: Option<InspectedMatches>,
    ) -> Self {
        Self {
            info,
            stats,
            matches,
        }
    }
}

/// Construct [`InspectedMatches`] from a RAII-owned match buffer.
pub fn inspected_matches(matches: BudgetedVec<Match>) -> InspectedMatches {
    InspectedMatches { matches }
}

/// Map a validated v3 header and completed stats into [`ArchiveInfo`].
pub fn archive_info_from_header(header: &ArchiveHeader, stats: &CompressionStats) -> ArchiveInfo {
    ArchiveInfo {
        version: VERSION,
        method: Some(header.method),
        layout: Some(header.layout),
        checksum: Some(header.checksum),
        legacy_layout: None,
        legacy_checksum: None,
        legacy_base_len: None,
        block_size: Some(header.block_size),
        min_match: Some(header.min_match),
        original_size: stats.original_size,
        payload_size: stats.payload_size,
        block_count: stats.block_count,
        semantic_match_count: stats.semantic_match_count,
        covered_bytes: stats.covered_bytes,
        literal_bytes: stats.literal_bytes,
    }
}

/// Fill [`CompressionStats`] from a validated header and measured sizes.
pub fn compression_stats_from_header(
    header: &ArchiveHeader,
    archive_size: u64,
    payload_size: u64,
    semantic_match_count: u64,
    covered_bytes: u64,
    literal_bytes: u64,
) -> crate::error::Result<CompressionStats> {
    Ok(CompressionStats {
        original_size: header.uncompressed_length,
        archive_size,
        payload_size,
        block_count: header.block_count()?,
        compressed_blocks: 0,
        reference_count: semantic_match_count,
        semantic_match_count,
        covered_bytes,
        literal_bytes,
        method: Some(header.method),
        layout: Some(header.layout),
        checksum: Some(header.checksum),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Checksum, Layout, Method, ResourceConfig};
    use crate::format_v3::ArchiveHeader;
    use crate::resource::MemoryBudget;

    #[test]
    fn header_stats_and_inspected_matches_constructors() {
        let header = ArchiveHeader {
            version: VERSION,
            checksum: Checksum::Xxh3,
            layout: Layout::Index,
            method: Method::M1RollingCdc,
            semantic_flags: 0,
            block_size: 1024,
            min_match: 32,
            seed_size: 48,
            target_chunk: 4096,
            max_distance: 0,
            rep_distance: 0,
            rep_min_match: 0,
            uncompressed_length: 3,
        };
        let stats = compression_stats_from_header(&header, 164, 7, 0, 0, 3).unwrap();
        assert_eq!(stats.block_count, 1);
        assert_eq!(stats.original_size, 3);
        let info = archive_info_from_header(&header, &stats);
        assert_eq!(info.version, 3);
        assert_eq!(info.block_size, Some(1024));
        let decoded = V3Decoded::new(info.clone(), stats, None);
        assert!(decoded.matches.is_none());
        let budget = MemoryBudget::new(ResourceConfig::default().memory);
        let owned = BudgetedVec::<Match>::new(&budget).unwrap();
        let inspected = inspected_matches(owned);
        assert!(inspected.as_slice().is_empty());
        let config = header
            .to_compression_config(ResourceConfig::default())
            .unwrap();
        assert_eq!(config.layout, Layout::Index);
        assert_eq!(config.target_chunk, Some(4096));
        assert!(config.seed_size.is_none());
    }
}
