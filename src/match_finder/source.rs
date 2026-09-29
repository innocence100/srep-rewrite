use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::codec::InputSpool;
use crate::error::{Error, Result};
use crate::offload::session::{CompressionSession, Operation, ResourceLease, SessionIo};
use crate::resource::Reservation;

pub trait DataSource {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read_at(&mut self, position: u64, destination: &mut [u8]) -> Result<()>;
}

pub struct SpoolDataSource<'a> {
    file: File,
    _handle: Option<ResourceLease>,
    io: Option<SessionIo>,
    _cache_reservation: Option<Reservation>,
    length: u64,
    caches: [[u8; 64 * 1024]; 2],
    cache_start: [u64; 2],
    cache_len: [usize; 2],
    next_cache: usize,
    marker: std::marker::PhantomData<&'a InputSpool>,
}

impl<'a> SpoolDataSource<'a> {
    pub(crate) fn new(spool: &'a InputSpool) -> Result<Self> {
        Ok(Self {
            file: spool.file.try_clone().map_err(Error::temp_storage)?,
            _handle: None,
            io: None,
            _cache_reservation: None,
            length: spool.len,
            caches: [[0; 64 * 1024]; 2],
            cache_start: [0; 2],
            cache_len: [0; 2],
            next_cache: 0,
            marker: std::marker::PhantomData,
        })
    }

    /// Session-owned source reader for the bounded codec path.  The cache and
    /// cloned descriptor are admitted before the file is cloned, and physical
    /// reads consume the shared session I/O ledger.  Public raw finders retain
    /// `new` and its unchanged API/resource behavior.
    pub(crate) fn new_in_session(
        spool: &'a InputSpool,
        session: &CompressionSession,
    ) -> Result<Self> {
        let handle = session.acquire_handles(1)?;
        let cache_reservation = session.memory().reserve((2 * 64 * 1024) as u64)?;
        session.checkpoint(Operation::Open)?;
        let file = match spool.file.try_clone() {
            Ok(file) => file,
            Err(error) => return Err(Error::temp_storage(error)),
        };
        Ok(Self {
            file,
            _handle: Some(handle),
            io: Some(session.io_access()),
            _cache_reservation: Some(cache_reservation),
            length: spool.len,
            caches: [[0; 64 * 1024]; 2],
            cache_start: [0; 2],
            cache_len: [0; 2],
            next_cache: 0,
            marker: std::marker::PhantomData,
        })
    }
}

impl DataSource for SpoolDataSource<'_> {
    fn len(&self) -> u64 {
        self.length
    }

    fn read_at(&mut self, position: u64, destination: &mut [u8]) -> Result<()> {
        if let Some(io) = &self.io {
            drop(io.reserve_io(0)?);
        }
        let end = position
            .checked_add(
                u64::try_from(destination.len())
                    .map_err(|_| Error::invalid_match("data source read length exceeds u64"))?,
            )
            .ok_or_else(|| Error::invalid_match("data source read endpoint overflows"))?;
        if end > self.length {
            return Err(Error::truncated("data source read exceeds input"));
        }
        let mut copied = 0usize;
        while copied < destination.len() {
            let current = position
                .checked_add(
                    u64::try_from(copied)
                        .map_err(|_| Error::invalid_match("data source read offset exceeds u64"))?,
                )
                .ok_or_else(|| Error::invalid_match("data source read offset overflows"))?;
            let hit = (0..self.caches.len()).find(|&slot| {
                current >= self.cache_start[slot]
                    && current < self.cache_start[slot] + self.cache_len[slot] as u64
            });
            let slot = if let Some(slot) = hit {
                slot
            } else {
                let slot = self.next_cache;
                self.next_cache = (self.next_cache + 1) % self.caches.len();
                // A partial read must never leave old cache bounds describing
                // newly overwritten bytes after an error.
                self.cache_len[slot] = 0;
                let cache_start = current.saturating_sub((self.caches[slot].len() / 2) as u64);
                if let Some(io) = self.io.as_ref() {
                    io.checkpoint(Operation::Seek)?;
                }
                self.file
                    .seek(SeekFrom::Start(cache_start))
                    .map_err(Error::temp_storage)?;
                let remaining = usize::try_from(self.length - cache_start)
                    .unwrap_or(self.caches[slot].len())
                    .min(self.caches[slot].len());
                let available = remaining.max((destination.len() - copied).min(1));
                if let Some(io) = self.io.as_ref() {
                    read_accounted(
                        &mut self.file,
                        io,
                        &mut self.caches[slot][..available],
                        ReadPolicy::SourceCache,
                    )?;
                } else {
                    self.file
                        .read_exact(&mut self.caches[slot][..available])
                        .map_err(|error| Error::map_eof(error, "data source read"))?;
                }
                self.cache_start[slot] = cache_start;
                self.cache_len[slot] = available;
                slot
            };
            let offset = usize::try_from(current - self.cache_start[slot])
                .map_err(|_| Error::invalid_match("data source cache offset overflows"))?;
            let available = (self.cache_len[slot] - offset).min(destination.len() - copied);
            destination[copied..copied + available]
                .copy_from_slice(&self.caches[slot][offset..offset + available]);
            copied += available;
        }
        Ok(())
    }
}

/// Snapshot bytes are optional but never free: independent handle lease, every
/// successful read charged, caller-provided BudgetedVec allocation owns RAM.
pub(crate) fn read_snapshot(
    spool: &InputSpool,
    session: &CompressionSession,
    out: &mut [u8],
) -> Result<()> {
    let _handle = session.acquire_handles(1)?;
    session.checkpoint(Operation::Open)?;
    let mut file = spool._temp.reopen().map_err(Error::temp_storage)?;
    read_accounted(&mut file, &session.io_access(), out, ReadPolicy::Snapshot)
}

/// Preserve the callers' released read_exact mappings. Only underlying reader
/// errors pass through this policy: resource admission/checkpoint/cancellation
/// failures already have authoritative identities and must not be remapped.
#[derive(Clone, Copy)]
enum ReadPolicy {
    SourceCache,
    Snapshot,
}
impl ReadPolicy {
    fn map(self, error: std::io::Error) -> Error {
        match self {
            Self::SourceCache => Error::map_eof(error, "data source read"),
            Self::Snapshot => Error::temp_storage(error),
        }
    }
}

fn read_accounted(
    reader: &mut impl Read,
    io: &SessionIo,
    out: &mut [u8],
    policy: ReadPolicy,
) -> Result<()> {
    let mut permit = io.reserve_io(out.len() as u128)?;
    let mut at = 0;
    while at < out.len() {
        io.checkpoint(Operation::Read)?;
        let end = (at + 65536).min(out.len());
        let n = match reader.read(&mut out[at..end]) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            r => r.map_err(|error| policy.map(error))?,
        };
        if n == 0 {
            // Match read_exact's short-input error, including snapshot context.
            return Err(policy.map(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            )));
        }
        permit.consume(n as u64)?;
        at += n;
    }
    Ok(())
}

/// Keep mandatory source-run metadata/pages and sink compaction headroom out of
/// optional accelerator allocations. Released once all optional caches exist.
pub(crate) fn accelerator_headroom(session: &CompressionSession) -> Result<Reservation> {
    let remaining = session
        .memory()
        .limit()
        .saturating_sub(session.memory().current());
    session.memory().reserve(remaining.min(2 * 1024 * 1024))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::codec::spool_input;
    use crate::config::ResourceConfig;
    use crate::offload::session::{CompressionSession, OffloadPolicy};
    use crate::resource::ResourceContext;

    #[test]
    fn session_source_charges_reader_io_and_releases_reader_resources() {
        let resources = ResourceConfig::default();
        let context = ResourceContext::with_resources(&resources).unwrap();
        let spool = spool_input(
            Cursor::new(b"session-counted-source".repeat(8192)),
            &resources,
            &context,
        )
        .unwrap();
        let session =
            CompressionSession::new(&context, &resources, OffloadPolicy::default()).unwrap();
        let handles_before = session.handle_usage().current;
        let mut source = SpoolDataSource::new_in_session(&spool, &session).unwrap();
        let mut bytes = [0; 64];
        source.read_at(0, &mut bytes).unwrap();
        assert!(session.io_used() >= bytes.len() as u128);
        assert_eq!(session.handle_usage().current, handles_before + 1);
        drop(source);
        assert_eq!(session.handle_usage().current, handles_before);
        drop(session);
        drop(spool);
        assert_eq!(context.memory.current(), 0);
        assert_eq!(context.temp.current(), 0);
    }

    #[test]
    fn counted_source_retries_short_reads_preserves_error_prefix_and_cancels_hits() {
        struct Short {
            n: usize,
            interrupted: bool,
        }
        impl Read for Short {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                if self.n == 7 {
                    return Err(std::io::Error::other("after prefix"));
                }
                let n = (7 - self.n).min(3).min(b.len());
                b[..n].fill(4);
                self.n += n;
                Ok(n)
            }
        }
        let r = ResourceConfig::default();
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        assert!(
            read_accounted(
                &mut Short {
                    n: 0,
                    interrupted: false
                },
                &s.io_access(),
                &mut [0; 12],
                ReadPolicy::SourceCache
            )
            .is_err()
        );
        assert_eq!(s.io_used(), 7);
        assert_eq!(s.io_reserved(), 0);
        let spool = spool_input(&b"cache-hit-cancel"[..], &r, &c).unwrap();
        let mut source = SpoolDataSource::new_in_session(&spool, &s).unwrap();
        source.read_at(0, &mut [0; 4]).unwrap();
        let before = s.io_used();
        s.cancel();
        assert!(source.read_at(0, &mut [0; 4]).is_err());
        assert_eq!(s.io_used(), before);
        drop(source);
        drop(spool);
        drop(s);
        assert_eq!(c.memory.current(), 0);
        assert_eq!(c.temp.current(), 0);
    }

    #[test]
    fn counted_read_policies_match_released_read_exact_errors_and_contexts() {
        #[derive(Clone)]
        struct Failing {
            position: usize,
            interrupted: bool,
            failure: Option<std::io::ErrorKind>,
        }
        impl Read for Failing {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                if self.position == 7 {
                    return match self.failure {
                        None => Ok(0),
                        Some(kind) => Err(std::io::Error::new(kind, "controlled reader failure")),
                    };
                }
                let n = (7 - self.position).min(3).min(bytes.len());
                bytes[..n].fill(9);
                self.position += n;
                Ok(n)
            }
        }
        let r = ResourceConfig::default();
        let c = ResourceContext::with_resources(&r).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        for policy in [ReadPolicy::SourceCache, ReadPolicy::Snapshot] {
            for failure in [
                None,
                Some(std::io::ErrorKind::UnexpectedEof),
                Some(std::io::ErrorKind::Other),
            ] {
                let mut reader = Failing {
                    position: 0,
                    interrupted: false,
                    failure,
                };
                let mut baseline = reader.clone();
                let mut expected_bytes = [0; 12];
                let raw = baseline.read_exact(&mut expected_bytes).unwrap_err();
                let expected = match policy {
                    ReadPolicy::SourceCache => Error::map_eof(raw, "data source read"),
                    ReadPolicy::Snapshot => Error::temp_storage(raw),
                };
                let before = s.io_used();
                let mut actual_bytes = [0; 12];
                let actual = read_accounted(&mut reader, &s.io_access(), &mut actual_bytes, policy)
                    .unwrap_err();
                assert_eq!(actual.kind(), expected.kind());
                assert_eq!(actual.context(), expected.context());
                assert_eq!(actual_bytes, expected_bytes);
                assert_eq!(s.io_used() - before, 7);
                assert_eq!(s.io_reserved(), 0);
            }
        }
    }

    #[test]
    fn actual_source_cache_and_snapshot_truncation_keep_distinct_policies() {
        let r = ResourceConfig::default();
        let c = ResourceContext::with_resources(&r).unwrap();
        let spool = spool_input(&vec![3; 200_000][..], &r, &c).unwrap();
        let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
        let mut raw = SpoolDataSource::new(&spool).unwrap();
        let mut counted = SpoolDataSource::new_in_session(&spool, &s).unwrap();
        // Warm both slots, then evict the first into a partially readable range.
        for position in [0, 100_000] {
            raw.read_at(position, &mut [0; 4]).unwrap();
            counted.read_at(position, &mut [0; 4]).unwrap();
        }
        let next = counted.next_cache;
        assert!(counted.cache_len[next] > 0);
        let target = 180_000;
        let start = target - 32768;
        spool.file.set_len(start + 7).unwrap();
        let expected = raw.read_at(target, &mut [0; 4]).unwrap_err();
        let before = s.io_used();
        let actual = counted.read_at(target, &mut [0; 4]).unwrap_err();
        assert_eq!(actual.kind(), crate::ErrorKind::TruncatedArchive);
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.context(), Some("data source read"));
        assert_eq!(actual.context(), expected.context());
        assert_eq!(s.io_used() - before, 7);
        assert_eq!(s.io_reserved(), 0);
        assert_eq!(counted.cache_len[next], 0);
        let before = s.io_used();
        let bounds = counted.read_at(spool.len, &mut [0; 1]).unwrap_err();
        assert_eq!(bounds.context(), Some("data source read exceeds input"));
        assert_eq!(s.io_used(), before);

        spool.file.set_len(7).unwrap();
        let mut legacy = spool._temp.reopen().unwrap();
        let mut buf = [0; 12];
        let expected = legacy
            .read_exact(&mut buf)
            .map_err(Error::temp_storage)
            .unwrap_err();
        let before = s.io_used();
        let actual = read_snapshot(&spool, &s, &mut [0; 12]).unwrap_err();
        assert_eq!(actual.kind(), crate::ErrorKind::TemporaryStorageFailure);
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.context(), expected.context());
        assert_eq!(s.io_used() - before, 7);
        assert_eq!(s.io_reserved(), 0);
        drop(raw);
        drop(counted);
        drop(spool);
        drop(s);
        assert_eq!(c.temp.current(), 0);
        assert_eq!(c.memory.current(), 0);
    }

    #[test]
    fn counted_read_does_not_remap_policy_or_cancellation_errors() {
        use crate::offload::session::{FaultAction, IoLimit};
        for policy in [ReadPolicy::SourceCache, ReadPolicy::Snapshot] {
            let r = ResourceConfig::default();
            let c = ResourceContext::with_resources(&r).unwrap();
            let s = CompressionSession::new(
                &c,
                &r,
                OffloadPolicy {
                    io_limit: IoLimit::Finite(0),
                    ..OffloadPolicy::default()
                },
            )
            .unwrap();
            let expected = s.reserve_io(1).unwrap_err();
            let actual = read_accounted(&mut Cursor::new([1]), &s.io_access(), &mut [0; 1], policy)
                .unwrap_err();
            assert_eq!(actual.kind(), expected.kind());
            assert_eq!(actual.context(), expected.context());
            assert_eq!(s.io_used(), 0);
            assert_eq!(s.io_reserved(), 0);
            drop(s);
            let s = CompressionSession::new(&c, &r, OffloadPolicy::default()).unwrap();
            s.inject(Operation::Read, 0, FaultAction::Fail);
            let actual = read_accounted(&mut Cursor::new([1]), &s.io_access(), &mut [0; 1], policy)
                .unwrap_err();
            assert_eq!(actual.kind(), crate::ErrorKind::TemporaryStorageFailure);
            assert_eq!(actual.context(), Some("injected offload I/O failure"));
            assert_eq!(s.io_reserved(), 0);
            s.cancel();
            let expected = s.check_cancelled().unwrap_err();
            let actual = read_accounted(&mut Cursor::new([1]), &s.io_access(), &mut [0; 1], policy)
                .unwrap_err();
            assert_eq!(actual.kind(), expected.kind());
            assert_eq!(actual.context(), expected.context());
            assert_eq!(s.io_used(), 0);
            assert_eq!(s.io_reserved(), 0);
        }
    }
}
