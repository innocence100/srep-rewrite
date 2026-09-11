use crate::{
    config::ResourceConfig,
    error::{Error, Result},
    resource::{Reservation, ResourceContext},
};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use tempfile::{Builder, NamedTempFile};
pub struct ArchiveStore {
    pub file: File,
    pub _temp: NamedTempFile,
    pub _reservation: Reservation,
    pub len: u64,
}
pub struct OutputStore {
    pub(crate) file: File,
    pub(crate) _temp: NamedTempFile,
    pub(crate) _reservation: Reservation,
    pub(crate) len: u64,
}
impl OutputStore {
    pub fn new(r: &ResourceConfig, c: &ResourceContext) -> Result<Self> {
        let t = Builder::new()
            .prefix("srep-legacy-output-")
            .tempfile_in(&r.temp_dir)
            .map_err(Error::temp_storage)?;
        let f = t.reopen().map_err(Error::temp_storage)?;
        Ok(Self {
            file: f,
            _temp: t,
            _reservation: c.temp.reserve(0)?,
            len: 0,
        })
    }
    pub fn write_at(&mut self, pos: u64, bytes: &[u8]) -> Result<()> {
        let end = pos
            .checked_add(
                u64::try_from(bytes.len())
                    .map_err(|_| Error::output_limit("legacy output offset overflows"))?,
            )
            .ok_or_else(|| Error::output_limit("legacy output offset overflows"))?;
        let growth = end.saturating_sub(self.len);
        let mut reserved = 0;
        if growth != 0 {
            self._reservation.grow(growth)?;
            reserved = growth;
        }
        if let Err(error) = self
            .file
            .seek(SeekFrom::Start(pos))
            .map_err(Error::temp_storage)
            .and_then(|_| self.file.write_all(bytes).map_err(Error::temp_storage))
        {
            let _ = self.file.set_len(self.len);
            if reserved != 0 {
                self._reservation.shrink(reserved);
            }
            return Err(error);
        }
        self.len = self.len.max(end);
        Ok(())
    }
    pub fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(pos))
            .map_err(Error::temp_storage)?;
        self.file.read_exact(buf).map_err(Error::temp_storage)
    }
    pub fn copy_to<W: Write>(mut self, out: &mut W) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(Error::temp_storage)?;
        let mut b = [0u8; 65536];
        let mut left = self.len;
        while left > 0 {
            let n = left.min(b.len() as u64) as usize;
            self.file
                .read_exact(&mut b[..n])
                .map_err(Error::temp_storage)?;
            out.write_all(&b[..n]).map_err(Error::output_io)?;
            left -= n as u64
        }
        out.flush().map_err(Error::output_io)
    }
}
pub fn spool_archive<R: Read>(
    header: Vec<u8>,
    input: &mut R,
    r: &ResourceConfig,
    c: &ResourceContext,
) -> Result<ArchiveStore> {
    let t = Builder::new()
        .prefix("srep-legacy-")
        .tempfile_in(&r.temp_dir)
        .map_err(Error::temp_storage)?;
    let mut f = t.reopen().map_err(Error::temp_storage)?;
    let mut q = c.temp.reserve(0)?;
    let header_len =
        u64::try_from(header.len()).map_err(|_| Error::temp_limit("legacy header too large"))?;
    q.grow(header_len)?;
    if let Err(error) = f.write_all(&header) {
        q.shrink(header_len);
        return Err(Error::temp_storage(error));
    }
    let mut b = [0u8; 65536];
    let mut n = header.len() as u64;
    loop {
        let z = input.read(&mut b).map_err(Error::input_io)?;
        if z == 0 {
            break;
        }
        let nn = n
            .checked_add(z as u64)
            .ok_or_else(|| Error::temp_limit("legacy archive size overflows"))?;
        if nn > r.temp_limit {
            return Err(Error::temp_limit("legacy archive exceeds temporary limit"));
        }
        q.grow(z as u64)?;
        if let Err(error) = f.write_all(&b[..z]) {
            q.shrink(z as u64);
            return Err(Error::temp_storage(error));
        }
        n = nn
    }
    f.flush().map_err(Error::temp_storage)?;
    f.seek(SeekFrom::Start(0)).map_err(Error::temp_storage)?;
    Ok(ArchiveStore {
        file: f,
        _temp: t,
        _reservation: q,
        len: n,
    })
}
