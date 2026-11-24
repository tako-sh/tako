use crate::{Applied, Commit, Replica, Types, disk::error};
use openraft::{Raft, ReadPolicy, async_runtime::WatchReceiver};
use std::collections::BTreeMap;
use std::{io, path::Path, time::Duration};
use tempfile::NamedTempFile;
use tokio::{runtime::Handle, time::timeout};

/// An engine writes to a disposable checkout. Its contents are visible to
/// other clients only after the corresponding Raft entry has been applied.
pub struct Writer {
    file: NamedTempFile,
    raft: Raft<Types, Replica>,
    replica: Replica,
    runtime: Handle,
    revision: Option<u64>,
    term: u64,
}

impl Writer {
    pub async fn checkout(
        raft: Raft<Types, Replica>,
        replica: Replica,
        dir: &Path,
    ) -> io::Result<Self> {
        let term = raft.metrics().borrow_watched().current_term;
        timeout(
            Duration::from_secs(10),
            raft.ensure_linearizable(ReadPolicy::ReadIndex),
        )
        .await
        .map_err(error)?
        .map_err(error)?;
        let (file, revision) = replica.checkout(dir).await?;
        Ok(Self {
            file,
            raft,
            replica,
            runtime: Handle::current(),
            revision: Some(revision),
            term,
        })
    }
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// A local check may reject stale work early. Success is only a hint
    /// until the caller confirms with ReadIndex or commits through Raft.
    pub fn revision_hint(&self) -> io::Result<u64> {
        let revision = self.revision.ok_or_else(|| error("writer fenced"))?;
        let metrics = self.raft.metrics();
        let current = metrics.borrow_watched();
        if current.current_term != self.term || current.current_leader != Some(current.id) {
            return Err(error("writer lost leadership"));
        }
        Ok(revision)
    }

    /// Called on a blocking engine thread before using the private copy for
    /// a fresh request. An open transaction may retain its existing snapshot.
    pub fn check_current(&self) -> io::Result<()> {
        let revision = self
            .revision
            .ok_or_else(|| error("writer fenced; reopen from the committed replica"))?;
        self.runtime.block_on(async {
            timeout(
                Duration::from_secs(10),
                self.raft.ensure_linearizable(ReadPolicy::ReadIndex),
            )
            .await
            .map_err(error)?
            .map_err(error)?;
            if self.revision_hint()? != revision || revision != self.replica.revision().await? {
                return Err(error("writer snapshot is stale"));
            }
            Ok(())
        })
    }

    /// Publish the initial, closed engine file. A concurrent successful
    /// creator wins the base-revision check and can be reused on retry.
    pub fn seed(&mut self, path: &Path) -> io::Result<()> {
        use std::io::Read;
        if self.revision != Some(0) {
            return Ok(());
        }
        let mut file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        if size == 0 || !size.is_multiple_of(4096) {
            return Err(error("invalid initial engine file size"));
        }
        let mut pages = BTreeMap::new();
        for i in 0..size / 4096 {
            let mut page = vec![0; 4096];
            file.read_exact(&mut page)?;
            if page.iter().any(|&b| b != 0) {
                pages.insert(i, page);
            }
        }
        self.publish(4096, size, pages)
    }

    pub fn fence(&mut self) {
        self.revision = None;
    }

    /// Every unsuccessful proposal, including a timeout after a quorum
    /// commit, invalidates this checkout. Recovery starts from committed bytes.
    pub fn publish(
        &mut self,
        page_size: u32,
        file_size: u64,
        pages: BTreeMap<u64, Vec<u8>>,
    ) -> io::Result<()> {
        let base_revision = self.revision.take().ok_or_else(|| error("writer fenced"))?;
        let commit = Commit {
            base_revision,
            page_size,
            file_size,
            pages,
        };
        let reply = self.runtime.block_on(async {
            timeout(
                Duration::from_secs(10),
                self.raft.client_write(crate::Proposal::Pages(commit)),
            )
            .await
            .map_err(error)?
            .map_err(error)
        })?;
        match reply.data {
            Applied::Committed(revision) => {
                self.revision = Some(revision);
                Ok(())
            }
            Applied::Stale { .. } => Err(error("writer lost its base revision")),
            _ => Err(error("invalid resource commit")),
        }
    }
}

/// One hosted resource on this member. Read-only callers share a checkout of
/// the last locally applied revision. A writer always has a private file.
#[derive(Clone)]
pub struct Resource {
    raft: Raft<Types, Replica>,
    replica: Replica,
    dir: std::path::PathBuf,
    runtime: Handle,
    cache: std::sync::Arc<parking_lot::Mutex<Option<(u64, std::sync::Arc<NamedTempFile>)>>>,
}
impl Resource {
    pub fn new(raft: Raft<Types, Replica>, replica: Replica, dir: &Path) -> Self {
        Self {
            raft,
            replica,
            dir: dir.to_owned(),
            runtime: Handle::current(),
            cache: Default::default(),
        }
    }
    pub fn grant_matches(&self, scope: &str, hash: &[u8; 32]) -> bool {
        self.replica.grant_matches(scope, hash)
    }

    pub fn current_leader(&self) -> Option<u128> {
        self.raft.metrics().borrow_watched().current_leader
    }

    pub fn leader_barrier(&self) -> io::Result<()> {
        self.runtime.block_on(async {
            timeout(
                Duration::from_secs(10),
                self.raft.ensure_linearizable(ReadPolicy::ReadIndex),
            )
            .await
            .map_err(error)?
            .map_err(error)?;
            Ok(())
        })
    }
    pub fn writer(&self) -> io::Result<Writer> {
        self.runtime.block_on(Writer::checkout(
            self.raft.clone(),
            self.replica.clone(),
            &self.dir,
        ))
    }
    pub fn local_file(&self) -> io::Result<std::sync::Arc<NamedTempFile>> {
        let mut cache = self.cache.lock();
        let revision = self.runtime.block_on(self.replica.revision())?;
        if let Some((cached, file)) = cache.as_ref()
            && *cached == revision
        {
            return Ok(file.clone());
        }
        let (file, revision) = self.runtime.block_on(self.replica.checkout(&self.dir))?;
        let file = std::sync::Arc::new(file);
        *cache = Some((revision, file.clone()));
        Ok(file)
    }
}
