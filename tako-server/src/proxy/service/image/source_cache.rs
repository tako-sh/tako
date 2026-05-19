use super::ImageSourceBytes;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tako_images::ImageError;
use tokio::sync::watch;

const SOURCE_CACHE_TTL: Duration = Duration::from_secs(10);
const SOURCE_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const SOURCE_CACHE_MAX_ENTRIES: usize = 256;

#[derive(Clone, Copy)]
struct SourceCachePolicy {
    ttl: Duration,
    max_bytes: usize,
    max_entries: usize,
}

impl Default for SourceCachePolicy {
    fn default() -> Self {
        Self {
            ttl: SOURCE_CACHE_TTL,
            max_bytes: SOURCE_CACHE_MAX_BYTES,
            max_entries: SOURCE_CACHE_MAX_ENTRIES,
        }
    }
}

pub(super) async fn get_or_load<F, Fut>(key: &str, load: F) -> Result<ImageSourceBytes, ImageError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<ImageSourceBytes, ImageError>>,
{
    let mut load = Some(load);

    loop {
        if let Some(source) = read(key) {
            return Ok(source);
        }

        match acquire_source_lease(key) {
            SourceLease::Owner(_owner) => {
                if let Some(source) = read(key) {
                    return Ok(source);
                }

                let source = load.take().ok_or(ImageError::TransformFailed)?().await?;
                insert(key, source.clone());
                return Ok(source);
            }
            SourceLease::Waiter(waiter) => {
                waiter.wait().await;
            }
        }
    }
}

fn read(key: &str) -> Option<ImageSourceBytes> {
    lock_source_cache().get(key, Instant::now(), SourceCachePolicy::default())
}

fn insert(key: &str, source: ImageSourceBytes) {
    lock_source_cache().insert(key, source, Instant::now(), SourceCachePolicy::default());
}

#[derive(Default)]
struct SourceCacheState {
    entries: HashMap<String, SourceEntry>,
    total_bytes: usize,
}

impl SourceCacheState {
    fn get(
        &mut self,
        key: &str,
        now: Instant,
        policy: SourceCachePolicy,
    ) -> Option<ImageSourceBytes> {
        self.prune_expired(now, policy.ttl);
        let entry = self.entries.get_mut(key)?;
        entry.last_accessed_at = now;
        Some(entry.source.clone())
    }

    fn insert(
        &mut self,
        key: &str,
        source: ImageSourceBytes,
        now: Instant,
        policy: SourceCachePolicy,
    ) {
        self.remove_entry(key);
        self.prune_expired(now, policy.ttl);

        let size = source.len();
        if size > policy.max_bytes || policy.max_entries == 0 {
            return;
        }

        self.total_bytes = self.total_bytes.saturating_add(size);
        self.entries.insert(
            key.to_string(),
            SourceEntry {
                source,
                inserted_at: now,
                last_accessed_at: now,
                size,
            },
        );
        self.prune_to_limits(policy);
    }

    fn prune_expired(&mut self, now: Instant, ttl: Duration) {
        let expired = self
            .entries
            .iter()
            .filter(|(_, entry)| now.saturating_duration_since(entry.inserted_at) >= ttl)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();

        for key in expired {
            self.remove_entry(&key);
        }
    }

    fn prune_to_limits(&mut self, policy: SourceCachePolicy) {
        while self.total_bytes > policy.max_bytes || self.entries.len() > policy.max_entries {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_accessed_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.remove_entry(&oldest_key);
        }
    }

    fn remove_entry(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.total_bytes = self.total_bytes.saturating_sub(entry.size);
        }
    }
}

struct SourceEntry {
    source: ImageSourceBytes,
    inserted_at: Instant,
    last_accessed_at: Instant,
    size: usize,
}

enum SourceLease {
    Owner(SourceOwner),
    Waiter(SourceWaiter),
}

struct SourceOwner {
    key: String,
    entry: Arc<InFlightSource>,
}

impl Drop for SourceOwner {
    fn drop(&mut self) {
        let mut sources = lock_in_flight_sources();
        if sources
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
        {
            sources.remove(&self.key);
        }
        let _ = self.entry.done.send(true);
    }
}

struct SourceWaiter {
    done: watch::Receiver<bool>,
}

impl SourceWaiter {
    async fn wait(mut self) {
        while !*self.done.borrow_and_update() {
            if self.done.changed().await.is_err() {
                break;
            }
        }
    }
}

struct InFlightSource {
    done: watch::Sender<bool>,
}

fn acquire_source_lease(key: &str) -> SourceLease {
    let mut sources = lock_in_flight_sources();
    if let Some(entry) = sources.get(key) {
        return SourceLease::Waiter(SourceWaiter {
            done: entry.done.subscribe(),
        });
    }

    let (done, _receiver) = watch::channel(false);
    let entry = Arc::new(InFlightSource { done });
    sources.insert(key.to_string(), entry.clone());
    SourceLease::Owner(SourceOwner {
        key: key.to_string(),
        entry,
    })
}

fn source_cache() -> &'static Mutex<SourceCacheState> {
    static CACHE: OnceLock<Mutex<SourceCacheState>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SourceCacheState::default()))
}

fn lock_source_cache() -> MutexGuard<'static, SourceCacheState> {
    match source_cache().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn in_flight_sources() -> &'static Mutex<HashMap<String, Arc<InFlightSource>>> {
    static SOURCES: OnceLock<Mutex<HashMap<String, Arc<InFlightSource>>>> = OnceLock::new();
    SOURCES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_in_flight_sources() -> MutexGuard<'static, HashMap<String, Arc<InFlightSource>>> {
    match in_flight_sources().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    static TEST_KEY_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn policy(max_bytes: usize, max_entries: usize) -> SourceCachePolicy {
        SourceCachePolicy {
            ttl: Duration::from_secs(10),
            max_bytes,
            max_entries,
        }
    }

    fn source(bytes: &[u8]) -> ImageSourceBytes {
        ImageSourceBytes::new(bytes.to_vec(), Some("image/png".to_string()))
    }

    fn test_key(name: &str) -> String {
        format!(
            "source-cache-test-{name}-{}-{}",
            std::process::id(),
            TEST_KEY_COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn source_cache_returns_cached_value_before_ttl() {
        let mut cache = SourceCacheState::default();
        let now = Instant::now();

        cache.insert("hero", source(b"image"), now, policy(1024, 8));
        let cached = cache
            .get("hero", now + Duration::from_secs(9), policy(1024, 8))
            .expect("cached source");

        assert_eq!(cached.bytes(), b"image");
    }

    #[test]
    fn source_cache_expires_entries_after_ttl() {
        let mut cache = SourceCacheState::default();
        let now = Instant::now();

        cache.insert("hero", source(b"image"), now, policy(1024, 8));
        let cached = cache.get("hero", now + Duration::from_secs(10), policy(1024, 8));

        assert!(cached.is_none());
        assert_eq!(cache.total_bytes, 0);
    }

    #[test]
    fn source_cache_prunes_oldest_entry_to_fit_byte_limit() {
        let mut cache = SourceCacheState::default();
        let now = Instant::now();
        let cache_policy = policy(5, 8);

        cache.insert("old", source(b"123"), now, cache_policy);
        cache.insert(
            "new",
            source(b"456"),
            now + Duration::from_millis(1),
            cache_policy,
        );

        assert!(
            cache
                .get("old", now + Duration::from_millis(2), cache_policy)
                .is_none()
        );
        assert!(
            cache
                .get("new", now + Duration::from_millis(2), cache_policy)
                .is_some()
        );
        assert!(cache.total_bytes <= 5);
    }

    #[test]
    fn source_cache_prunes_oldest_entry_to_fit_entry_limit() {
        let mut cache = SourceCacheState::default();
        let now = Instant::now();
        let cache_policy = policy(1024, 1);

        cache.insert("old", source(b"old"), now, cache_policy);
        cache.insert(
            "new",
            source(b"new"),
            now + Duration::from_millis(1),
            cache_policy,
        );

        assert!(
            cache
                .get("old", now + Duration::from_millis(2), cache_policy)
                .is_none()
        );
        assert!(
            cache
                .get("new", now + Duration::from_millis(2), cache_policy)
                .is_some()
        );
        assert_eq!(cache.entries.len(), 1);
    }

    #[tokio::test]
    async fn duplicate_source_loads_share_owner_result() {
        let key = test_key("duplicate");
        let first_key = key.clone();
        let second_key = key.clone();
        let loads = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let ready_wait = ready.notified();

        let first = tokio::spawn({
            let loads = loads.clone();
            let ready = ready.clone();
            let release = release.clone();
            async move {
                get_or_load(&first_key, || async move {
                    loads.fetch_add(1, Ordering::SeqCst);
                    ready.notify_waiters();
                    release.notified().await;
                    Ok(source(b"shared"))
                })
                .await
            }
        });

        ready_wait.await;
        let second = tokio::spawn({
            let loads = loads.clone();
            async move {
                get_or_load(&second_key, || async move {
                    loads.fetch_add(1, Ordering::SeqCst);
                    Ok(source(b"duplicate"))
                })
                .await
            }
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(loads.load(Ordering::SeqCst), 1);

        release.notify_waiters();
        let first = first.await.expect("first task").expect("first source");
        let second = second.await.expect("second task").expect("second source");

        assert_eq!(first.bytes(), b"shared");
        assert_eq!(second.bytes(), b"shared");
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }
}
