use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::codec::InputSpool;
use crate::error::{Error, Result};

pub trait DataSource {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read_at(&mut self, position: u64, destination: &mut [u8]) -> Result<()>;
}

pub struct SpoolDataSource<'a> {
    file: File,
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
                let cache_start = current.saturating_sub((self.caches[slot].len() / 2) as u64);
                self.file
                    .seek(SeekFrom::Start(cache_start))
                    .map_err(Error::temp_storage)?;
                let remaining = usize::try_from(self.length - cache_start)
                    .unwrap_or(self.caches[slot].len())
                    .min(self.caches[slot].len());
                let available = remaining.max((destination.len() - copied).min(1));
                self.file
                    .read_exact(&mut self.caches[slot][..available])
                    .map_err(|error| Error::map_eof(error, "data source read"))?;
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
