//! Capture from the authoritative engine for the hosted generation.
use std::path::Path;
use tako_backup::{CommittedSpool, anyhow::Result};
use tako_raft::Resource;

pub(super) struct Source {
    resource: Resource,
    spool: CommittedSpool,
    position: u64,
}
impl Source {
    pub(super) fn open(dir: &Path, resource: Resource, remote: u64) -> Result<Self> {
        resource.leader_barrier()?;
        let spool = CommittedSpool::open(dir)?;
        let position = remote.max(spool.last()?);
        Ok(Self {
            resource,
            spool,
            position,
        })
    }
    pub(super) fn sync(&mut self) -> Result<u64> {
        let Self {
            resource,
            spool,
            position,
        } = self;
        resource.leader_barrier()?;
        let last = resource.transaction_id()?;
        while *position < last {
            let next = *position + 1;
            let Some(change) = resource.backup_delta(next)? else {
                let (image, base) = resource.backup_image()?;
                spool.snapshot(
                    image.reopen()?,
                    base.txid,
                    base.captured_at_ms,
                    base.page_size,
                )?;
                *position = base.txid;
                break;
            };
            write_change(spool, next, &change)?;
            *position = next;
        }
        Ok(*position)
    }
    pub(super) fn snapshot(&mut self) -> Result<u64> {
        self.sync()?;
        let Self {
            resource,
            spool,
            position,
        } = self;
        let (image, base) = resource.backup_image()?;
        // Commits can land during the file copy. Spool their deltas before
        // naming the full image, preserving intermediate restore positions.
        while *position < base.txid {
            let next = *position + 1;
            if let Some(c) = resource.backup_delta(next)? {
                write_change(spool, next, &c)?;
            }
            *position = next;
        }
        spool.snapshot(
            image.reopen()?,
            base.txid,
            base.captured_at_ms,
            base.page_size,
        )?;
        Ok(base.txid)
    }

    pub(super) fn captured_at(&self, id: u64) -> Result<i64> {
        self.spool.captured_at(id)
    }
    pub(super) fn position(&self) -> u64 {
        self.position
    }
    pub(super) fn close(self) -> Result<u64> {
        Ok(self.position)
    }
}

fn write_change(spool: &CommittedSpool, next: u64, c: &tako_raft::Commit) -> Result<()> {
    if c.replace {
        spool.replaced(next, c.captured_at_ms, c.file_size, c.page_size, &c.pages)
    } else {
        spool.delta(next, c.captured_at_ms, c.file_size, c.page_size, &c.pages)
    }
}
