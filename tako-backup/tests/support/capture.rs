//! Feed checkpointed Turso transactions to the production LTX encoder. The writer
//! acknowledges each commit after checkpointing, before the test requests capture.
use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use tako_backup::CommittedSpool;

pub struct Capture {
    database: PathBuf,
    spool: CommittedSpool,
    position: u64,
    previous: Vec<u8>,
}

impl Capture {
    pub fn open(database: &Path, directory: &Path, floor: u64) -> Result<Self> {
        let spool = CommittedSpool::open(directory)?;
        Ok(Self {
            database: database.into(),
            position: floor.max(spool.last()?),
            spool,
            previous: Vec::new(),
        })
    }

    pub fn sync(&mut self) -> Result<u64> {
        if self.position == 0 {
            return self.snapshot();
        }
        let image = fs::read(&self.database)?;
        if image != self.previous {
            let size = page_size(&image)?;
            let pages: BTreeMap<_, _> = image
                .chunks_exact(size)
                .enumerate()
                .filter(|(n, page)| self.previous.get(n * size..(n + 1) * size) != Some(*page))
                .map(|(n, page)| (n as u64, page.to_vec()))
                .collect();
            let next = self.position + 1;
            self.spool.delta(
                next,
                super::super::chrono_millis(),
                image.len() as u64,
                size as u32,
                &pages,
            )?;
            self.previous = image;
            self.position = next;
        }
        Ok(self.position)
    }

    pub fn captured_at(&self, txid: u64) -> Result<i64> {
        self.spool.captured_at(txid)
    }

    pub fn snapshot(&mut self) -> Result<u64> {
        let image = fs::read(&self.database)?;
        let next = self.position + 1;
        self.spool.snapshot(
            fs::File::open(&self.database)?,
            next,
            super::super::chrono_millis(),
            page_size(&image)? as u32,
        )?;
        self.previous = image;
        self.position = next;
        Ok(next)
    }
}

fn page_size(image: &[u8]) -> Result<usize> {
    ensure!(image.len() >= 100, "missing database header");
    let size = u16::from_be_bytes([image[16], image[17]]);
    let size = if size == 1 { 65536 } else { size as usize };
    ensure!(
        (512..=65536).contains(&size) && size.is_power_of_two() && image.len().is_multiple_of(size),
        "invalid database page size"
    );
    Ok(size)
}
