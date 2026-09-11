use std::fs::File;

use tempfile::NamedTempFile;

use super::layouts::{LiteralRef, Operation};
use crate::format::{ArchiveHeader, FRAME_LEN, LayoutMetadata, MethodParameters};
use crate::match_ir::Match;
use crate::resource::{BudgetedVec, Reservation};

#[derive(Debug)]
pub(super) struct ArchiveSpool {
    pub(super) file: File,
    pub(super) _temp: NamedTempFile,
    pub(super) len: u64,
    pub(super) _reservation: Reservation,
}

#[derive(Debug)]
pub(super) struct ParsedArchive {
    pub(super) header: ArchiveHeader,
    pub(super) header_bytes: [u8; crate::format::HEADER_LEN],
    pub(super) method_parameters: MethodParameters,
    pub(super) effective_min_match: u64,
    pub(super) method_payload: BudgetedVec<u8>,
    pub(super) layout_payload: BudgetedVec<u8>,
    pub(super) meta: LayoutMetadata,
    pub(super) blocks: BudgetedVec<ParsedBlock>,
    pub(super) matches: BudgetedVec<Match>,
    pub(super) summary_payload: BudgetedVec<u8>,
    pub(super) summary_offset: u64,
    pub(super) summary_total: u64,
}

#[derive(Debug)]
pub(super) struct ParsedBlock {
    pub(super) frame: [u8; FRAME_LEN],
    pub(super) digest: [u8; 64],
    pub(super) payload: BudgetedVec<u8>,
    pub(super) literals: BudgetedVec<LiteralRef>,
    pub(super) operations: BudgetedVec<Operation>,
    pub(super) registers: BudgetedVec<(u64, u64, u64, u64, u64)>,
}

pub(super) struct RecordParts {
    pub(super) frame: [u8; FRAME_LEN],
    pub(super) payload: BudgetedVec<u8>,
    pub(super) digest: [u8; 64],
    pub(super) total: u64,
}

pub(super) struct BlockParts {
    pub(super) literals: BudgetedVec<LiteralRef>,
    pub(super) operations: BudgetedVec<Operation>,
    pub(super) registers: BudgetedVec<(u64, u64, u64, u64, u64)>,
    pub(super) literal_count: u64,
    pub(super) operation_count: u64,
}
