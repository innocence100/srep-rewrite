use std::fs::OpenOptions;

use srep::{
    CandidateIndex, ErrorKind, HybridCandidateIndex, IndexEntry, MemoryBudget, RamCandidateIndex,
    ResourceContext, TempBudget,
};

fn key(value: u64) -> [u8; 8] {
    value.to_le_bytes()
}

fn entry(position: u64, ordinal: u64, metadata: &[u8]) -> IndexEntry {
    IndexEntry::new(0, &key(7), position, ordinal, metadata).unwrap()
}

#[test]
fn ram_index_deduplicates_exact_identity_and_orders_candidates() {
    let budget = MemoryBudget::new(4096);
    let mut index = RamCandidateIndex::new(&budget).unwrap();
    index.insert(entry(10, 9, &[])).unwrap();
    index.insert(entry(10, 3, &[])).unwrap();
    index.insert(entry(20, 2, &[])).unwrap();
    index.finish_epoch().unwrap();
    assert_eq!(index.entries()[0].position, 20);
    assert_eq!(index.entries()[1].position, 10);

    let candidates = index.candidates(0, &key(7), 30, 0).unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|item| (item.position, item.insertion_ordinal))
            .collect::<Vec<_>>(),
        vec![(20, 2), (10, 3)]
    );

    let mut metadata_index = RamCandidateIndex::new(&budget).unwrap();
    metadata_index
        .insert(IndexEntry::new(5, &[7; 16], 10, 4, &[2; 4]).unwrap())
        .unwrap();
    metadata_index
        .insert(IndexEntry::new(5, &[7; 16], 10, 3, &[1; 4]).unwrap())
        .unwrap();
    let metadata = metadata_index.candidates(5, &[7; 16], 30, 0).unwrap();
    assert_eq!(metadata[0].metadata_slice(), &[1, 1, 1, 1]);
}

#[test]
fn current_epoch_is_visible_and_distance_is_inclusive() {
    let budget = MemoryBudget::new(4096);
    let mut index = RamCandidateIndex::new(&budget).unwrap();
    index.insert(entry(8, 0, &[])).unwrap();
    assert_eq!(index.candidates(0, &key(7), 9, 1).unwrap().len(), 1);
    assert!(index.candidates(0, &key(7), 8, 0).unwrap().is_empty());
    assert_eq!(index.candidates(0, &key(7), 9, 0).unwrap().len(), 1);
    index.finish_epoch().unwrap();
    assert_eq!(index.candidates(0, &key(7), 9, 1).unwrap().len(), 1);
}

#[test]
fn invalid_shapes_and_arithmetic_are_rejected_without_mutation() {
    let budget = MemoryBudget::new(4096);
    let index = RamCandidateIndex::new(&budget).unwrap();
    assert_eq!(
        IndexEntry::new(0, &[1], 0, 0, &[]).unwrap_err().kind(),
        ErrorKind::InvalidConfiguration
    );
    assert_eq!(
        index.candidates(0, &[1], 1, 0).unwrap_err().kind(),
        ErrorKind::InvalidConfiguration
    );
    assert_eq!(index.candidates(0, &key(7), 0, 0).unwrap().len(), 0);
    assert_eq!(budget.current(), 0);
}

#[test]
fn failed_insert_does_not_change_index_or_budget() {
    let budget = MemoryBudget::new(1);
    let mut index = RamCandidateIndex::new(&budget).unwrap();
    let before = budget.current();
    assert_eq!(
        index.insert(entry(1, 0, &[])).unwrap_err().kind(),
        ErrorKind::MemoryBudgetExceeded
    );
    assert_eq!(budget.current(), before);
    assert!(index.candidates(0, &key(7), 2, 0).unwrap().is_empty());
}

#[test]
fn duplicate_lower_ordinal_repositions_without_needing_sort_scratch() {
    let budget = MemoryBudget::new(4096);
    let mut index = RamCandidateIndex::new(&budget).unwrap();
    index.insert(entry(10, 9, &[])).unwrap();
    index.insert(entry(20, 2, &[])).unwrap();
    index.insert(entry(10, 1, &[])).unwrap();
    assert_eq!(
        index
            .entries()
            .iter()
            .map(|item| (item.position, item.insertion_ordinal))
            .collect::<Vec<_>>(),
        vec![(20, 2), (10, 1)]
    );
}

#[test]
fn hybrid_memtable_identity_upsert_is_allocation_free_and_matches_ram() {
    let memory = MemoryBudget::new(64 * 1024);
    let context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    let mut hybrid = HybridCandidateIndex::with_memtable_bytes(
        &context,
        2 * std::mem::size_of::<IndexEntry>() as u64,
    )
    .unwrap();
    let mut ram = RamCandidateIndex::new(&memory).unwrap();
    let values = [
        entry(10, 9, &[]),
        entry(20, 4, &[]),
        entry(10, 12, &[]),
        entry(10, 1, &[]),
    ];

    hybrid.insert(values[0]).unwrap();
    ram.insert(values[0]).unwrap();
    hybrid.insert(values[1]).unwrap();
    ram.insert(values[1]).unwrap();
    let before_memory = context.memory.current();
    let before_reserved = hybrid.memtable_reserved_bytes();
    let before_temp = context.temp.current();
    let before_generation = hybrid.next_generation();
    let before_spills = context.candidate_index_spill_count();
    let before_paths = hybrid.run_paths();

    for value in &values[2..] {
        hybrid.insert(*value).unwrap();
        ram.insert(*value).unwrap();
        assert_eq!(context.memory.current(), before_memory);
        assert_eq!(hybrid.memtable_reserved_bytes(), before_reserved);
        assert_eq!(context.temp.current(), before_temp);
        assert_eq!(hybrid.next_generation(), before_generation);
        assert_eq!(context.candidate_index_spill_count(), before_spills);
        assert_eq!(hybrid.run_paths(), before_paths);
    }

    assert_eq!(
        hybrid.memtable_entries(),
        &[entry(20, 4, &[]), entry(10, 1, &[])]
    );
    let expected = ram.candidates(0, &key(7), 30, 0).unwrap();
    let actual = hybrid.candidates(0, &key(7), 30, 0).unwrap();
    assert_eq!(actual.as_slice(), expected.as_slice());
    assert_eq!(actual.len(), 2);
    assert_eq!(actual[1].insertion_ordinal, 1);
}

#[test]
fn hybrid_memtable_lower_ordinal_upsert_is_rollback_safe_at_boundary() {
    let context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    let mut hybrid = HybridCandidateIndex::with_memtable_bytes(
        &context,
        2 * std::mem::size_of::<IndexEntry>() as u64,
    )
    .unwrap();
    hybrid.insert(entry(10, 9, &[])).unwrap();
    hybrid.insert(entry(20, 4, &[])).unwrap();
    let before_entries = hybrid.memtable_entries().to_vec();
    let before_capacity = hybrid.memtable_capacity();
    let before_memory = context.memory.current();
    let before_temp = context.temp.current();
    let before_generation = hybrid.next_generation();
    let before_spills = context.candidate_index_spill_count();

    hybrid.insert(entry(10, 1, &[])).unwrap();

    assert_eq!(hybrid.memtable_capacity(), before_capacity);
    assert_eq!(
        hybrid.memtable_reserved_bytes(),
        before_capacity as u64 * std::mem::size_of::<IndexEntry>() as u64
    );
    assert_eq!(context.memory.current(), before_memory);
    assert_eq!(context.temp.current(), before_temp);
    assert_eq!(hybrid.next_generation(), before_generation);
    assert_eq!(context.candidate_index_spill_count(), before_spills);
    assert_eq!(hybrid.run_paths(), Vec::<std::path::PathBuf>::new());
    assert_eq!(before_entries.len(), hybrid.memtable_entries().len());
    assert_eq!(
        hybrid.memtable_entries(),
        &[entry(20, 4, &[]), entry(10, 1, &[])]
    );
}

#[test]
fn hybrid_run_and_memtable_duplicate_query_and_finish_are_canonical() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut hybrid = HybridCandidateIndex::with_memtable_bytes_in(
        &context,
        temp_dir.path(),
        srep::candidate_index::INDEX_RECORD_LEN as u64,
    )
    .unwrap();
    let original = entry(10, 9, &[]);
    hybrid.insert(original).unwrap();
    hybrid.finish_epoch().unwrap();
    hybrid.insert(entry(10, 3, &[])).unwrap();

    let query = hybrid.candidates(0, &key(7), 11, 0).unwrap();
    assert_eq!(query.len(), 1);
    assert_eq!(query[0].insertion_ordinal, 3);
    hybrid.finish_epoch().unwrap();
    let canonical = hybrid.candidates(0, &key(7), 11, 0).unwrap();
    assert_eq!(canonical.len(), 1);
    assert_eq!(canonical[0].insertion_ordinal, 3);
    drop(query);
    drop(canonical);
    drop(hybrid);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert!(temp_dir.path().read_dir().unwrap().next().is_none());
}

#[test]
fn hybrid_memtable_identity_upsert_preserves_capacity_and_budget() {
    let context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    let mut hybrid = HybridCandidateIndex::with_memtable_bytes(
        &context,
        2 * std::mem::size_of::<IndexEntry>() as u64,
    )
    .unwrap();
    let first = entry(10, 9, &[]);
    let second = entry(20, 4, &[]);
    hybrid.insert(first).unwrap();
    hybrid.insert(second).unwrap();
    let before = hybrid.memtable_entries().to_vec();
    let before_capacity = hybrid.memtable_capacity();
    let before_reserved = hybrid.memtable_reserved_bytes();
    let before_memory = context.memory.current();
    let before_temp = context.temp.current();
    let before_generation = hybrid.next_generation();
    let before_spills = context.candidate_index_spill_count();

    hybrid.insert(entry(10, 12, &[])).unwrap();
    hybrid.insert(entry(10, 1, &[])).unwrap();

    assert_eq!(hybrid.memtable_capacity(), before_capacity);
    assert_eq!(hybrid.memtable_reserved_bytes(), before_reserved);
    assert_eq!(context.memory.current(), before_memory);
    assert_eq!(context.temp.current(), before_temp);
    assert_eq!(hybrid.next_generation(), before_generation);
    assert_eq!(context.candidate_index_spill_count(), before_spills);
    assert!(hybrid.run_paths().is_empty());
    assert_eq!(before.len(), hybrid.memtable_entries().len());
    assert_eq!(
        hybrid.memtable_entries(),
        &[entry(20, 4, &[]), entry(10, 1, &[])]
    );

    let candidates = hybrid.candidates(0, &key(7), 30, 0).unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[1].insertion_ordinal, 1);
}

#[test]
fn empty_memtable_duplicate_sequence_matches_ram_and_keeps_one_identity() {
    let context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    let memory = MemoryBudget::new(64 * 1024);
    let mut hybrid = HybridCandidateIndex::with_memtable_bytes(
        &context,
        srep::candidate_index::INDEX_RECORD_LEN as u64,
    )
    .unwrap();
    let mut ram = RamCandidateIndex::new(&memory).unwrap();
    let values = [entry(10, 9, &[]), entry(10, 12, &[]), entry(10, 1, &[])];

    for value in values {
        hybrid.insert(value).unwrap();
        ram.insert(value).unwrap();
    }

    let expected = ram.candidates(0, &key(7), 11, 0).unwrap();
    let actual = hybrid.candidates(0, &key(7), 11, 0).unwrap();
    assert_eq!(actual.as_slice(), expected.as_slice());
    assert_eq!(actual.len(), 1);
    assert_eq!(actual[0].insertion_ordinal, 1);
    assert_eq!(hybrid.memtable_entries().len(), 1);
    assert!(hybrid.run_paths().is_empty());
    assert_eq!(context.candidate_index_spill_count(), 0);
}

#[test]
fn memtable_boundary_duplicate_does_not_spill_or_change_generation() {
    let context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    let mut hybrid = HybridCandidateIndex::with_memtable_bytes(
        &context,
        2 * srep::candidate_index::INDEX_RECORD_LEN as u64,
    )
    .unwrap();
    hybrid.insert(entry(10, 9, &[])).unwrap();
    hybrid.insert(entry(20, 4, &[])).unwrap();
    let before_entries = hybrid.memtable_entries().to_vec();
    let before_memory = context.memory.current();
    let before_reserved = hybrid.memtable_reserved_bytes();
    let before_temp = context.temp.current();
    let before_generation = hybrid.next_generation();
    let before_spills = context.candidate_index_spill_count();
    let before_paths = hybrid.run_paths();

    hybrid.insert(entry(10, 12, &[])).unwrap();
    hybrid.insert(entry(10, 1, &[])).unwrap();

    assert_eq!(
        hybrid.memtable_entries(),
        &[entry(20, 4, &[]), entry(10, 1, &[])]
    );
    assert_eq!(before_entries.len(), hybrid.memtable_entries().len());
    assert_eq!(context.memory.current(), before_memory);
    assert_eq!(hybrid.memtable_reserved_bytes(), before_reserved);
    assert_eq!(context.temp.current(), before_temp);
    assert_eq!(hybrid.next_generation(), before_generation);
    assert_eq!(context.candidate_index_spill_count(), before_spills);
    assert_eq!(hybrid.run_paths(), before_paths);
    assert_eq!(hybrid.candidates(0, &key(7), 30, 0).unwrap().len(), 2);
}

#[test]
fn run_and_memtable_duplicate_query_uses_minimum_and_finish_is_canonical() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut hybrid = HybridCandidateIndex::with_memtable_bytes_in(
        &context,
        temp_dir.path(),
        srep::candidate_index::INDEX_RECORD_LEN as u64,
    )
    .unwrap();
    hybrid.insert(entry(10, 9, &[])).unwrap();
    hybrid.finish_epoch().unwrap();
    hybrid.insert(entry(10, 3, &[])).unwrap();

    let before_finish = hybrid.candidates(0, &key(7), 11, 0).unwrap();
    assert_eq!(before_finish.len(), 1);
    assert_eq!(before_finish[0].insertion_ordinal, 3);
    hybrid.finish_epoch().unwrap();

    let after_finish = hybrid.candidates(0, &key(7), 11, 0).unwrap();
    assert_eq!(after_finish.len(), 1);
    assert_eq!(after_finish[0].insertion_ordinal, 3);
    assert!(hybrid.memtable_entries().is_empty());
    drop(before_finish);
    drop(after_finish);
    drop(hybrid);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert!(temp_dir.path().read_dir().unwrap().next().is_none());
}

#[test]
fn result_budget_failure_leaves_existing_index_unchanged() {
    let entry_bytes = std::mem::size_of::<IndexEntry>() as u64;
    let probe_budget = MemoryBudget::new(entry_bytes * 8);
    let mut probe = RamCandidateIndex::new(&probe_budget).unwrap();
    probe.insert(entry(1, 0, &[])).unwrap();
    probe.insert(entry(2, 1, &[])).unwrap();
    let result = probe.candidates(0, &key(7), 3, 0).unwrap();
    let result_bytes = (result.capacity() * std::mem::size_of::<IndexEntry>()) as u64;
    drop(result);
    drop(probe);

    let budget = MemoryBudget::new(entry_bytes * 2 + result_bytes - 1);
    let mut index = RamCandidateIndex::new(&budget).unwrap();
    index.insert(entry(1, 0, &[])).unwrap();
    index.insert(entry(2, 1, &[])).unwrap();
    let before = index.entries().to_vec();
    let result = index.candidates(0, &key(7), 3, 0);
    assert_eq!(result.unwrap_err().kind(), ErrorKind::MemoryBudgetExceeded);
    assert_eq!(index.entries(), before.as_slice());
    assert_eq!(budget.current(), entry_bytes * 2);
}

#[test]
fn preallocated_large_same_key_query_has_exact_budget_boundary() {
    const COUNT: usize = 1024;
    fn insert_same_key(index: &mut RamCandidateIndex, position: u64) {
        index.insert(entry(position, position, &[])).unwrap();
    }
    let generous = MemoryBudget::new(16 * 1024 * 1024);
    let mut probe = RamCandidateIndex::with_capacity(COUNT, &generous).unwrap();
    for position in 0..COUNT as u64 {
        insert_same_key(&mut probe, position);
    }
    let index_bytes = probe.reserved_bytes();
    let result = probe.candidates(0, &key(7), COUNT as u64, 0).unwrap();
    assert_eq!(result.len(), COUNT);
    let result_bytes = result.reserved_bytes();
    assert_eq!(generous.current(), index_bytes + result_bytes);
    let exact_limit = index_bytes + result_bytes;
    drop(result);
    assert_eq!(generous.current(), index_bytes);
    drop(probe);
    assert_eq!(generous.current(), 0);

    let exact = MemoryBudget::new(exact_limit);
    let mut success = RamCandidateIndex::with_capacity(COUNT, &exact).unwrap();
    for position in 0..COUNT as u64 {
        insert_same_key(&mut success, position);
    }
    let result = success.candidates(0, &key(7), COUNT as u64, 0).unwrap();
    assert_eq!(result.len(), COUNT);
    assert_eq!(exact.current(), index_bytes + result_bytes);
    assert!(exact.high_water() <= exact.limit());
    drop(result);
    drop(success);
    assert_eq!(exact.current(), 0);

    let tight = MemoryBudget::new(exact_limit - 1);
    let mut failure = RamCandidateIndex::with_capacity(COUNT, &tight).unwrap();
    for position in 0..COUNT as u64 {
        insert_same_key(&mut failure, position);
    }
    let before = failure.entries().to_vec();
    assert_eq!(
        failure
            .candidates(0, &key(7), COUNT as u64, 0)
            .unwrap_err()
            .kind(),
        ErrorKind::MemoryBudgetExceeded
    );
    assert_eq!(failure.entries(), before.as_slice());
    assert_eq!(tight.current(), index_bytes);
    assert!(tight.high_water() <= tight.limit());
}

#[test]
fn hybrid_callback_query_matches_ram_when_forced_to_spill() {
    let memory = MemoryBudget::new(64 * 1024);
    let temp = TempBudget::new(1024 * 1024);
    let context = ResourceContext {
        memory: memory.clone(),
        temp,
        temp_dir: std::env::temp_dir(),
        candidate_index_memtable_bytes: None,
        candidate_index_spills: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };
    let mut ram = RamCandidateIndex::new(&memory).unwrap();
    let mut hybrid = HybridCandidateIndex::with_memtable_bytes(&context, 1).unwrap();
    for (position, ordinal) in [(8, 3), (24, 0), (16, 2), (24, 4), (8, 1)] {
        let value = entry(position, ordinal, &[]);
        ram.insert(value).unwrap();
        hybrid.insert(value).unwrap();
        hybrid.finish_epoch().unwrap();
    }
    let mut expected = Vec::new();
    ram.for_each_candidate(0, &key(7), 32, 0, &mut |value| {
        expected.push(value);
        Ok(())
    })
    .unwrap();
    let mut actual = Vec::new();
    hybrid
        .for_each_candidate(0, &key(7), 32, 0, &mut |value| {
            actual.push(value);
            Ok(())
        })
        .unwrap();
    assert_eq!(actual, expected);
    assert!(hybrid.run_count() > 0);
}

#[test]
fn index_record_and_header_have_exact_wire_sizes_and_checksums() {
    let value = IndexEntry::new(5, &[0xabu8; 16], 0x0102, 0x0304, &[0x55; 4]).unwrap();
    let record = value.encode_record().unwrap();
    assert_eq!(record.len(), 104);
    assert_eq!(&record[0..2], &1u16.to_le_bytes());
    assert_eq!(record[2], 5);
    assert_eq!(record[3], 16);
    assert_eq!(record[4], 4);
    assert_eq!(&record[8..16], &0x0102u64.to_le_bytes());
    assert_eq!(&record[16..24], &0x0304u64.to_le_bytes());
    assert_eq!(IndexEntry::decode_record(&record).unwrap(), value);

    let header = srep::candidate_index::RunHeader::new(7, 9).unwrap();
    let bytes = header.encode();
    assert_eq!(bytes.len(), 64);
    assert_eq!(&bytes[0..8], b"SREPIDX1");
    assert_eq!(
        srep::candidate_index::RunHeader::decode(&bytes).unwrap(),
        header
    );
}

#[test]
fn malformed_index_record_and_header_are_rejected() {
    let value = IndexEntry::new(0, &key(7), 10, 2, &[]).unwrap();
    let mut record = value.encode_record().unwrap();
    record[7] = 1;
    assert_eq!(
        IndexEntry::decode_record(&record).unwrap_err().kind(),
        ErrorKind::CorruptRecord
    );
    let mut record = value.encode_record().unwrap();
    record[103] ^= 1;
    assert_eq!(
        IndexEntry::decode_record(&record).unwrap_err().kind(),
        ErrorKind::CorruptRecord
    );

    let header = srep::candidate_index::RunHeader::new(1, 0).unwrap();
    let mut bytes = header.encode().to_vec();
    bytes[12] = 1;
    assert_eq!(
        srep::candidate_index::RunHeader::decode(&bytes)
            .unwrap_err()
            .kind(),
        ErrorKind::CorruptIndex
    );
    let mut bytes = header.encode().to_vec();
    bytes.push(0);
    assert_eq!(
        srep::candidate_index::RunHeader::decode(&bytes)
            .unwrap_err()
            .kind(),
        ErrorKind::CorruptIndex
    );
}

#[test]
fn every_record_field_and_padding_byte_is_strictly_validated() {
    let value = IndexEntry::new(5, &[0xabu8; 16], 10, 2, &[0x55; 4]).unwrap();
    let original = value.encode_record().unwrap();
    for offset in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 16, 24, 55, 56, 87, 88, 103] {
        let mut bytes = original;
        bytes[offset] ^= 1;
        assert!(
            IndexEntry::decode_record(&bytes).is_err(),
            "mutating record byte {offset} must fail"
        );
    }

    let mut key_padding = original;
    key_padding[40] = 1;
    assert!(IndexEntry::decode_record(&key_padding).is_err());
    let mut metadata_padding = original;
    metadata_padding[60] = 1;
    assert!(IndexEntry::decode_record(&metadata_padding).is_err());
}

#[test]
fn singleton_entry_spills_when_no_index_entry_fits_memory() {
    let context = ResourceContext::from_limits(1, 4096);
    let mut index =
        HybridCandidateIndex::with_memtable_bytes_in(&context, &context.temp_dir, 1).unwrap();
    index.insert(entry(10, 0, &[])).unwrap();
    let mut actual = Vec::new();
    index
        .for_each_candidate(0, &key(7), 11, 0, &mut |value| {
            actual.push(value);
            Ok(())
        })
        .unwrap();
    assert_eq!(actual.len(), 1);
}

#[test]
fn temp_budget_failure_does_not_leave_a_visible_run() {
    let context = ResourceContext::from_limits(64 * 1024, 64);
    let mut index = HybridCandidateIndex::with_memtable_bytes_in(
        &context,
        &context.temp_dir,
        srep::candidate_index::INDEX_RECORD_LEN as u64,
    )
    .unwrap();
    index.insert(entry(10, 0, &[])).unwrap();
    assert_eq!(
        index.finish_epoch().unwrap_err().kind(),
        ErrorKind::TempBudgetExceeded
    );
    assert_eq!(index.run_count(), 0);
    assert_eq!(context.temp.current(), 0);
}

#[test]
fn spilled_duplicate_resolution_is_logical_and_not_sort_adjacency_dependent() {
    let context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    let mut index = HybridCandidateIndex::with_memtable_bytes(&context, 1).unwrap();
    let first = IndexEntry::new(5, &[3; 16], 90, 100, &[1; 4]).unwrap();
    let interleaving = IndexEntry::new(5, &[3; 16], 90, 2, &[2; 4]).unwrap();
    let duplicate = IndexEntry::new(5, &[3; 16], 90, 1, &[1; 4]).unwrap();
    index.insert(first).unwrap();
    index.finish_epoch().unwrap();
    index.insert(interleaving).unwrap();
    index.finish_epoch().unwrap();
    index.insert(duplicate).unwrap();
    index.finish_epoch().unwrap();

    let mut actual = Vec::new();
    index
        .for_each_candidate(5, &[3; 16], 91, 0, &mut |value| {
            actual.push((
                value.position,
                value.insertion_ordinal,
                value.metadata_slice().to_vec(),
            ));
            Ok(())
        })
        .unwrap();
    assert_eq!(actual, vec![(90, 1, vec![1; 4]), (90, 2, vec![2; 4])]);
}

#[test]
fn strict_run_reader_rejects_truncated_and_extra_physical_bytes() {
    for truncate in [true, false] {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
        context.temp_dir = temp_dir.path().to_path_buf();
        let mut index =
            HybridCandidateIndex::with_memtable_bytes_in(&context, temp_dir.path(), 1).unwrap();
        index.insert(entry(10, 0, &[])).unwrap();
        index.finish_epoch().unwrap();
        let path = index.run_paths().pop().unwrap();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        let length = file.metadata().unwrap().len();
        file.set_len(if truncate { length - 1 } else { length + 1 })
            .unwrap();
        file.sync_all().unwrap();
        let error = index
            .for_each_candidate(0, &key(7), 11, 0, &mut |_| Ok(()))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::CorruptIndex);
    }
}

#[test]
fn spilled_query_matches_ram_without_rebuilding_scratch() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut hybrid =
        HybridCandidateIndex::with_memtable_bytes_in(&context, temp_dir.path(), 1).unwrap();
    let ram_budget = MemoryBudget::new(64 * 1024);
    let mut ram = RamCandidateIndex::new(&ram_budget).unwrap();
    for position in 0..12u64 {
        let value = entry(position * 3, position, &[]);
        hybrid.insert(value).unwrap();
        hybrid.finish_epoch().unwrap();
        ram.insert(value).unwrap();
    }
    let before_temp = context.temp.current();
    let mut expected = Vec::new();
    ram.for_each_candidate(0, &key(7), 40, 0, &mut |value| {
        expected.push(value);
        Ok(())
    })
    .unwrap();
    let mut actual = Vec::new();
    hybrid
        .for_each_candidate(0, &key(7), 40, 0, &mut |value| {
            actual.push(value);
            Ok(())
        })
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(context.temp.current(), before_temp);
    assert!(hybrid.run_count() > 0);
    drop(hybrid);
    assert_eq!(context.temp.current(), 0);
    assert!(temp_dir.path().read_dir().unwrap().next().is_none());
}

#[test]
fn scratch_header_is_exactly_srepqry1_and_order_authoritative() {
    let header = srep::ScratchHeader::new(1, 7, 9).unwrap();
    let bytes = header.encode();
    assert_eq!(bytes.len(), 64);
    assert_eq!(&bytes[0..8], b"SREPQRY1");
    assert_eq!(&bytes[8..10], &1u16.to_le_bytes());
    assert_eq!(&bytes[10..12], &104u16.to_le_bytes());
    assert_eq!(bytes[12], 1);
    assert_eq!(bytes[13], 0);
    assert_eq!(&bytes[14..16], &[0; 2]);
    assert_eq!(&bytes[16..24], &7u64.to_le_bytes());
    assert_eq!(&bytes[24..32], &9u64.to_le_bytes());
    assert_eq!(srep::ScratchHeader::decode(&bytes).unwrap(), header);
    assert_eq!(
        srep::ScratchHeader::new(0, 0, 0).unwrap_err().kind(),
        ErrorKind::InvalidConfiguration
    );
    let mut wrong_magic = bytes;
    wrong_magic[0] = b'X';
    assert_eq!(
        srep::ScratchHeader::decode(&wrong_magic)
            .unwrap_err()
            .kind(),
        ErrorKind::CorruptIndex
    );
    let mut wrong_order = bytes;
    wrong_order[12] = 2;
    assert_eq!(
        srep::ScratchHeader::decode(&wrong_order)
            .unwrap_err()
            .kind(),
        ErrorKind::CorruptIndex
    );
}

#[test]
fn scratch_resources_are_local_and_finish_epoch_failure_is_retryable() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut context = ResourceContext::from_limits(64 * 1024, 1024 * 1024);
    context.temp_dir = temp_dir.path().to_path_buf();
    let mut index = HybridCandidateIndex::with_memtable_bytes_in(
        &context,
        temp_dir.path(),
        srep::candidate_index::INDEX_RECORD_LEN as u64,
    )
    .unwrap();
    index.insert(entry(10, 0, &[])).unwrap();
    let before = context.temp.current();
    let before_paths = index.run_paths();
    let before_generation = index.next_generation();
    let before_spills = context.candidate_index_spill_count();
    let mut seen = Vec::new();
    index
        .for_each_candidate(0, &key(7), 11, 0, &mut |value| {
            seen.push(value);
            Ok(())
        })
        .unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(context.temp.current(), before);

    let held = context
        .temp
        .reserve(context.temp.limit() - context.temp.current() - 1)
        .unwrap();
    assert_eq!(
        index.finish_epoch().unwrap_err().kind(),
        ErrorKind::TempBudgetExceeded
    );
    assert_eq!(index.run_paths(), before_paths);
    assert_eq!(index.run_count(), 0);
    assert_eq!(index.memtable_entries().len(), 1);
    assert_eq!(index.next_generation(), before_generation);
    assert_eq!(context.candidate_index_spill_count(), before_spills);
    assert_eq!(context.temp.current(), context.temp.limit() - 1);

    let mut after_failure = Vec::new();
    index
        .for_each_candidate(0, &key(7), 11, 0, &mut |value| {
            after_failure.push(value);
            Ok(())
        })
        .unwrap();
    assert_eq!(after_failure, vec![entry(10, 0, &[])]);
    drop(held);
    index.finish_epoch().unwrap();
    let mut after_retry = Vec::new();
    index
        .for_each_candidate(0, &key(7), 11, 0, &mut |value| {
            after_retry.push(value);
            Ok(())
        })
        .unwrap();
    assert_eq!(after_retry, vec![entry(10, 0, &[])]);
    assert_eq!(index.run_paths().len(), 1);
    assert_eq!(index.next_generation(), before_generation + 2);
    assert_eq!(context.candidate_index_spill_count(), before_spills + 1);
    assert!(index.memtable_entries().is_empty());
    drop(index);
    assert_eq!(context.memory.current(), 0);
    assert_eq!(context.temp.current(), 0);
    assert!(temp_dir.path().read_dir().unwrap().next().is_none());
}
