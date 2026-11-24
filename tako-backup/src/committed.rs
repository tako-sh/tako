//! Encode already committed SQLite pages in the same LTX format as WAL capture.
//! A member elected after a failure can upload the same transaction at the
//! same position using the commit time chosen by its writer.
use super::*;
use std::io::Seek;
use terse_ltx::{Encoder, Header, NO_CHECKSUM};

pub struct CommittedSpool {
    files: FileStore,
    _lock: CaptureLock,
}
impl CommittedSpool {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            files: FileStore::new(dir),
            _lock: lock_file(&dir.join("capture.lock"), "a capture owner")?,
        })
    }
    pub fn last(&self) -> Result<u64> {
        Ok(self
            .files
            .list(0)?
            .into_iter()
            .chain(self.files.list(9)?)
            .map(|s| s.max_txid)
            .max()
            .unwrap_or(0))
    }
    fn segment(
        &self,
        base: bool,
        txid: u64,
        at: i64,
        size: u64,
        page_size: u32,
        write: impl FnOnce(&mut Encoder<Box<dyn SegmentWriter>>) -> Result<()>,
    ) -> Result<()> {
        ensure!(
            page_size.is_power_of_two()
                && (512..=65536).contains(&page_size)
                && size.is_multiple_of(u64::from(page_size)),
            "invalid SQLite page size"
        );
        let segment = Segment::new(if base { 9 } else { 0 }, if base { 1 } else { txid }, txid)?;
        if self.files.path(&segment).try_exists()? {
            return Ok(());
        }
        let header = Header {
            flags: NO_CHECKSUM,
            page_size,
            commit: (size / u64::from(page_size)).try_into()?,
            min_txid: segment.min_txid,
            max_txid: txid,
            timestamp: at,
            ..Default::default()
        };
        let mut encoder = Encoder::new(self.files.create(&segment)?, header)?;
        write(&mut encoder)?;
        encoder.finish(0)?.commit()
    }
    pub fn snapshot(&self, mut file: File, txid: u64, at: i64, page_size: u32) -> Result<()> {
        let size = file.metadata()?.len();
        file.rewind()?;
        self.segment(true, txid, at, size, page_size, |encoder| {
            let mut page = vec![0; page_size as usize];
            for number in 1..=u32::try_from(size / u64::from(page_size))? {
                file.read_exact(&mut page)?;
                if number != 0x40000000 / page_size + 1 {
                    encoder.write_page(number, &page)?;
                }
            }
            Ok(())
        })
    }
    pub fn delta(
        &self,
        txid: u64,
        at: i64,
        size: u64,
        page_size: u32,
        pages: &BTreeMap<u64, Vec<u8>>,
    ) -> Result<()> {
        self.segment(false, txid, at, size, page_size, |encoder| {
            for (&index, bytes) in pages {
                let number = u32::try_from(index + 1)?;
                if number != 0x40000000 / page_size + 1 {
                    encoder.write_page(number, bytes)?;
                }
            }
            Ok(())
        })
    }
    pub fn replaced(
        &self,
        txid: u64,
        at: i64,
        size: u64,
        page_size: u32,
        pages: &BTreeMap<u64, Vec<u8>>,
    ) -> Result<()> {
        self.segment(true, txid, at, size, page_size, |encoder| {
            let zeros = vec![0; page_size as usize];
            for number in 1..=u32::try_from(size / u64::from(page_size))? {
                if number != 0x40000000 / page_size + 1 {
                    encoder
                        .write_page(number, pages.get(&u64::from(number - 1)).unwrap_or(&zeros))?;
                }
            }
            Ok(())
        })
    }
    pub fn captured_at(&self, txid: u64) -> Result<i64> {
        for level in [9, 0] {
            let segment = Segment::new(level, if level == 9 { 1 } else { txid }, txid)?;
            if self.files.path(&segment).try_exists()? {
                return Ok(terse_ltx::Decoder::new(self.files.open(&segment)?)?
                    .header()
                    .timestamp);
            }
        }
        bail!("transaction {txid} is not in the spool")
    }
}
