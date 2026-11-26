//! The committed Raft image of a Turso database. Blocking checkpoints leave
//! a base file and a prefix of complete MVCC log frames sufficient for replay.
//! All integers are little endian. LTX reserves the SQLite lock-byte page, so
//! the logical payload skips that physical page even for databases over 1 GiB.
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

pub(super) const PAGE: usize = 4096;
const MAGIC: &[u8; 16] = b"Tako Turso MVCC\x01";
const RESERVED: u64 = 0x4000_0000 / PAGE as u64;
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid Turso recovery image")
}
fn physical(page: u64) -> u64 {
    if page >= RESERVED { page + 1 } else { page }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Layout {
    pub base: u64,
    pub log: u64,
}

impl Layout {
    fn log_page(self) -> u64 {
        1 + self.base.div_ceil(PAGE as u64)
    }
    pub fn size(self) -> io::Result<u64> {
        let end = self
            .log_page()
            .checked_add(self.log.div_ceil(PAGE as u64))
            .ok_or_else(invalid)?;
        physical(end - 1)
            .checked_add(1)
            .and_then(|v| v.checked_mul(PAGE as u64))
            .ok_or_else(invalid)
    }
    fn header(self) -> Vec<u8> {
        let mut bytes = vec![0; PAGE];
        bytes[..16].copy_from_slice(MAGIC);
        bytes[16..24].copy_from_slice(&self.base.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.log.to_le_bytes());
        bytes
    }
    pub fn inspect(file: &mut File) -> io::Result<Option<Self>> {
        if file.metadata()?.len() < PAGE as u64 {
            return Ok(None);
        }
        file.rewind()?;
        let mut header = [0; 32];
        file.read_exact(&mut header)?;
        if &header[..16] != MAGIC {
            return Ok(None);
        }
        let result = Self {
            base: u64::from_le_bytes(header[16..24].try_into().unwrap()),
            log: u64::from_le_bytes(header[24..].try_into().unwrap()),
        };
        if result.base == 0 || result.size()? != file.metadata()?.len() {
            return Err(invalid());
        }
        Ok(Some(result))
    }
    /// Materialize only into a fresh private directory owned by this process.
    pub fn extract(self, file: &mut File, dir: &Path) -> io::Result<()> {
        self.extract_file(file, &dir.join("data.db"), 1, self.base)?;
        self.extract_file(file, &dir.join("data.db-log"), self.log_page(), self.log)?;
        File::create_new(dir.join("data.db-wal"))?;
        Ok(())
    }
    fn extract_file(self, src: &mut File, dest: &Path, start: u64, len: u64) -> io::Result<()> {
        let mut output = File::create_new(dest)?;
        let mut left = len;
        let mut page = start;
        while left > 0 {
            src.seek(SeekFrom::Start(physical(page) * PAGE as u64))?;
            let chunk = if page < RESERVED {
                left.min((RESERVED - page) * PAGE as u64)
            } else {
                left
            };
            if io::copy(&mut src.take(chunk), &mut output)? != chunk {
                return Err(invalid());
            }
            left -= chunk;
            page += chunk.div_ceil(PAGE as u64);
        }
        output.flush()
    }
    /// A frame may start inside a page, so reread both partial boundary pages.
    /// A header-version upgrade can change the first log page independently.
    pub fn changes(
        self,
        dir: &Path,
        previous_log: u64,
        new_base: bool,
    ) -> io::Result<BTreeMap<u64, Vec<u8>>> {
        let mut pages = BTreeMap::from([(0, self.header())]);
        if new_base {
            read_pages(&mut pages, &dir.join("data.db"), 1, 0, self.base)?;
        }
        let file = dir.join("data.db-log");
        if self.log > 0 {
            if !new_base && previous_log >= PAGE as u64 {
                read_pages(&mut pages, &file, self.log_page(), 0, PAGE as u64)?;
            }
            read_pages(
                &mut pages,
                &file,
                self.log_page(),
                if new_base {
                    0
                } else {
                    previous_log / PAGE as u64
                },
                self.log,
            )?;
        }
        Ok(pages)
    }
}

fn read_pages(
    pages: &mut BTreeMap<u64, Vec<u8>>,
    path: &Path,
    start: u64,
    first: u64,
    len: u64,
) -> io::Result<()> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(first * PAGE as u64))?;
    for index in first..len.div_ceil(PAGE as u64) {
        let mut bytes = vec![0; PAGE];
        let n = (len - index * PAGE as u64).min(PAGE as u64) as usize;
        file.read_exact(&mut bytes[..n])?;
        pages.insert(physical(start + index), bytes);
    }
    Ok(())
}
