use super::{DbError, Reply};
use redb::{DatabaseError, backends::FileBackend};
use std::collections::VecDeque;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type FileId = (u64, u64);
type Slot<D> = Arc<Mutex<Option<(FileId, D)>>>;

/// Reassign slots, not handles: even when all are busy a new request can wait for
/// one, and a slow open, close or transaction never holds the registry lock.
pub(super) struct Pool<D, const N: usize> {
    recent: Mutex<VecDeque<(PathBuf, Slot<D>)>>,
    #[cfg(test)]
    pub(super) opens: std::sync::atomic::AtomicUsize,
}

impl<D, const N: usize> Default for Pool<D, N> {
    fn default() -> Self {
        Self {
            recent: Mutex::default(),
            #[cfg(test)]
            opens: Default::default(),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Access {
    Read,
    Update,
    Create,
}

impl<D, const N: usize> Pool<D, N> {
    fn slot(&self, path: &Path) -> Result<Slot<D>, DbError> {
        let mut recent = self.recent.lock().map_err(|_| "cache registry poisoned")?;
        let entry = match recent.iter().position(|(p, _)| p == path) {
            Some(hit) => recent.remove(hit).unwrap(),
            None => {
                let slot = if recent.len() == N {
                    let oldest_idle = recent.iter().position(|(_, s)| Arc::strong_count(s) == 1);
                    recent.remove(oldest_idle.unwrap_or(0)).unwrap().1
                } else {
                    Slot::default()
                };
                (path.to_owned(), slot)
            }
        };
        let slot = Arc::clone(&entry.1);
        recent.push_back(entry);
        Ok(slot)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.recent.lock().unwrap().len()
    }

    pub(super) fn with(
        &self,
        path: &Path,
        access: Access,
        open: impl FnOnce(FileBackend) -> Result<D, DatabaseError>,
        run: impl FnOnce(&D) -> Reply,
    ) -> Reply {
        // Missing stores don't displace working stores or consume a descriptor.
        if access != Access::Create && !path.try_exists()? {
            return Ok(None);
        }
        let slot = self.slot(path)?;
        // A panic in an earlier `run` must not disable whichever stores reuse
        // this slot: drop that handle and open a fresh one below.
        let mut held = slot.lock().unwrap_or_else(|poisoned| {
            let mut held = poisoned.into_inner();
            *held = None;
            slot.clear_poison();
            held
        });
        if let Some((id, db)) = held.as_ref() {
            // Check after admission: restore may have replaced the tree while we waited.
            match fs::metadata(path) {
                Ok(meta) if (meta.dev(), meta.ino()) == *id => return run(db),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        *held = None; // Close the previous descriptor before opening its replacement.
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(access != Access::Read)
            .create(access == Access::Create)
            .truncate(false)
            .mode(0o600)
            .open(path)
        {
            Err(e) if access != Access::Create && e.kind() == ErrorKind::NotFound => {
                return Ok(None);
            }
            result => result?,
        };
        let meta = file.metadata()?;
        if access == Access::Read && meta.len() == 0 {
            return Ok(None); // A first writer hasn't initialized redb yet.
        }
        #[cfg(test)]
        self.opens
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let db = open(FileBackend::new(file))?;
        run(&held.insert(((meta.dev(), meta.ino()), db)).1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn a_panicking_request_does_not_disable_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.redb");
        let pool = Pool::<(), 1>::default();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.with(
                &path,
                Access::Create,
                |_| Ok(()),
                |_| panic!("request failed"),
            )
        }));
        assert!(panicked.is_err());
        let reply = pool.with(&path, Access::Update, |_| Ok(()), |_| Ok(Some("ok".into())));
        assert_eq!(reply.unwrap().as_deref(), Some("ok"));
        // The handle that was in use during the panic is replaced, not reused.
        assert_eq!(pool.opens.load(Relaxed), 2);
    }
}
