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
        if self.revision != Some(0) {
            return Ok(());
        }
        self.replace_file(path)
    }

    /// The replacement is already a closed, validated SQLite or redb file.
    pub fn replace_file(&mut self, path: &Path) -> io::Result<()> {
        use std::io::{Read, Seek};
        let mut file = std::fs::File::open(path)?;
        let mut header = [0; 18];
        file.read_exact(&mut header)?;
        let page_size = if &header[..16] == b"SQLite format 3\0" {
            match u16::from_be_bytes([header[16], header[17]]) {
                1 => 65536,
                other => u32::from(other),
            }
        } else {
            4096
        };
        let size = file.metadata()?.len();
        if !(512..=65536).contains(&page_size)
            || !page_size.is_power_of_two()
            || !size.is_multiple_of(u64::from(page_size))
        {
            return Err(error("invalid engine file size"));
        }
        file.rewind()?;
        let mut pages = BTreeMap::new();
        for i in 0..size / u64::from(page_size) {
            let mut page = vec![0; page_size as usize];
            file.read_exact(&mut page)?;
            if page.iter().any(|&b| b != 0) {
                pages.insert(i, page);
            }
        }
        self.publish_pages(page_size, size, pages, true)
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
        self.publish_pages(page_size, file_size, pages, false)
    }

    pub fn publish_pages(
        &mut self,
        page_size: u32,
        file_size: u64,
        pages: BTreeMap<u64, Vec<u8>>,
        replace: bool,
    ) -> io::Result<()> {
        let base_revision = self.revision.take().ok_or_else(|| error("writer fenced"))?;
        let commit = Commit {
            replace,
            base_revision,
            captured_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(error)?
                .as_millis()
                .try_into()
                .map_err(error)?,
            page_size,
            file_size,
            pages,
        };
        const PART_SIZE: usize = 512 * 1024;
        let submit = |proposal| {
            self.runtime.block_on(async {
                let reply = timeout(Duration::from_secs(10), self.raft.client_write(proposal))
                    .await
                    .map_err(error)?
                    .map_err(error)?;
                Ok::<_, io::Error>(reply.data)
            })
        };
        let result = if commit.pages.len().saturating_mul(page_size as usize + 10) <= PART_SIZE {
            submit(crate::Proposal::Pages(commit))?
        } else {
            let encoded = crate::disk::encode(&commit)?;
            drop(commit);
            let mut nonce = [0; 16];
            getrandom::fill(&mut nonce).map_err(error)?;
            let mut result = Applied::Invalid;
            let parts = encoded.chunks(PART_SIZE);
            let count = parts.len();
            for (index, bytes) in parts.enumerate() {
                let last = index + 1 == count;
                result = submit(crate::Proposal::CommitPart {
                    base: base_revision,
                    nonce,
                    index: index as u64,
                    last,
                    bytes: bytes.to_vec(),
                })?;
                if !last && result != Applied::NoChange {
                    return Err(error(
                        "staged transaction lost its base revision or was superseded",
                    ));
                }
            }
            result
        };
        match result {
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
    pub fn has_grant(&self, scope: &str) -> bool {
        self.replica.has_grant(scope)
    }

    pub fn grant_matches(&self, scope: &str, hash: &[u8; 32]) -> bool {
        self.replica.grant_matches(scope, hash)
    }

    pub async fn set_backup_settings(&self, track: bool, sealed: Vec<u8>) -> io::Result<()> {
        let reply = timeout(
            Duration::from_secs(10),
            self.raft
                .client_write(crate::Proposal::BackupSettings { track, sealed }),
        )
        .await
        .map_err(error)?
        .map_err(error)?;
        if reply.data != Applied::NoChange {
            return Err(error("backup settings were not committed"));
        }
        Ok(())
    }

    pub fn file_size(&self) -> io::Result<u64> {
        self.replica.file_size()
    }
    pub fn grant_scopes(&self) -> Vec<String> {
        self.replica.grant_scopes()
    }
    pub fn backup_settings(&self) -> Option<Vec<u8>> {
        self.replica.backup_settings()
    }
    pub fn leads(&self) -> bool {
        let rx = self.raft.metrics();
        let metrics = rx.borrow_watched();
        metrics.current_leader == Some(metrics.id)
    }
    pub fn backup_image(&self) -> io::Result<(NamedTempFile, crate::BackupPosition)> {
        self.runtime
            .block_on(self.replica.checkout_position(&self.dir))
    }

    pub fn transaction_id(&self) -> io::Result<u64> {
        self.runtime.block_on(self.replica.transaction_id())
    }
    pub fn backup_delta(&self, txid: u64) -> io::Result<Option<Commit>> {
        self.runtime.block_on(self.replica.backup_delta(txid))
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
