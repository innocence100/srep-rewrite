use std::ops::Range;

use crate::resource::BudgetedVec;

#[derive(Debug)]
pub(super) struct BlockPlan {
    pub(super) start: u64,
    pub(super) len: u64,
    pub(super) literals: BudgetedVec<(u64, u64)>,
    pub(super) registers: BudgetedVec<(u64, u64, u64, u64, u64)>,
    pub(super) operations: BudgetedVec<Operation>,
    pub(super) payload_len: u64,
}

#[derive(Clone, Debug)]
pub(super) enum Operation {
    Literal(u64, u64),
    Match(u64, u64, u64, u64),
}

#[derive(Debug)]
pub(super) struct LiteralRef {
    pub(super) offset: u64,
    pub(super) bytes: Range<usize>,
}
