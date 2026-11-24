//! Capture from the authoritative engine for the hosted generation.
use std::path::Path;
use tako_backup::{Capture, CommittedSpool, anyhow::Result};
use tako_raft::Resource;

pub(super) enum Source {
    Wal(Capture),
    Committed {
        resource: Resource,
        spool: CommittedSpool,
        position: u64,
    },
}
impl Source {
    pub(super) fn open(
        path: &Path,
        dir: &Path,
        floor: u64,
        resource: Option<Resource>,
        remote: u64,
    ) -> Result<Self> {
        match resource {
            None => Ok(Self::Wal(Capture::open(path, dir, floor)?)),
            Some(resource) => {
                resource.leader_barrier()?;
                let spool = CommittedSpool::open(dir)?;
                let position = remote.max(spool.last()?);
                Ok(Self::Committed {
                    resource,
                    spool,
                    position,
                })
            }
        }
    }

    pub(super) fn sync(&mut self) -> Result<u64> {
        match self {
            Self::Wal(c) => c.sync(),
            Self::Committed {
                resource,
                spool,
                position,
            } => {
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
        }
    }

    pub(super) fn snapshot(&mut self) -> Result<u64> {
        if let Self::Wal(c) = self {
            return c.snapshot();
        }
        self.sync()?;
        let Self::Committed {
            resource,
            spool,
            position,
        } = self
        else {
            unreachable!()
        };
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
        match self {
            Self::Wal(c) => c.captured_at(id),
            Self::Committed { spool, .. } => spool.captured_at(id),
        }
    }
    pub(super) fn position(&self) -> u64 {
        match self {
            Self::Wal(c) => c.position(),
            Self::Committed { position, .. } => *position,
        }
    }
    pub(super) fn close(self) -> Result<u64> {
        match self {
            Self::Wal(c) => c.close(),
            Self::Committed { position, .. } => Ok(position),
        }
    }
}

fn write_change(spool: &CommittedSpool, next: u64, c: &tako_raft::Commit) -> Result<()> {
    if c.replace {
        spool.replaced(next, c.captured_at_ms, c.file_size, c.page_size, &c.pages)
    } else {
        spool.delta(next, c.captured_at_ms, c.file_size, c.page_size, &c.pages)
    }
}
