//! Time-to-idle policy shared by expensive in-process read caches.
//!
//! A live client transport does not imply its cached project data is in use.
//! Owners periodically evict idle values; returned Arcs/owned results remain
//! valid, and the next request rebuilds the cache through its normal miss path.

use std::sync::Mutex;
use std::time::{Duration, Instant};

crate::settings! {
    cache {
        // Release unused search/vector data after this many seconds.
        // Zero disables idle eviction.
        idle_secs: u64 = 300,
    }
}

/// Resolve once when a long-lived process starts its cache cleanup loop.
pub fn idle_timeout() -> Duration {
    let settings =
        Cache::resolve_or_default(RawCache::default(), crate::settings_file::ConfigScope::User);
    Duration::from_secs(settings.idle_secs)
}

/// A cache entry whose idle clock advances only on successful use or insert.
pub struct IdleCache<T> {
    value: Option<T>,
    last_used: Instant,
}

impl<T> Default for IdleCache<T> {
    fn default() -> Self {
        Self {
            value: None,
            last_used: Instant::now(),
        }
    }
}

impl<T> IdleCache<T> {
    /// A rejected key does not prolong the lifetime of the old cached value.
    pub fn get_if(&mut self, matches: impl FnOnce(&T) -> bool) -> Option<&T> {
        if !self.value.as_ref().is_some_and(matches) {
            return None;
        }
        self.last_used = Instant::now();
        self.value.as_ref()
    }

    pub fn insert(&mut self, value: T) {
        self.value = Some(value);
        self.last_used = Instant::now();
    }

    /// Return the entry so a caller can drop large values outside its mutex.
    pub fn take(&mut self) -> Option<T> {
        self.value.take()
    }

    fn take_if_idle(&mut self, now: Instant, timeout: Duration) -> Option<T> {
        if timeout.is_zero() || now.saturating_duration_since(self.last_used) < timeout {
            return None;
        }
        self.take()
    }
}

/// Nonblocking idle eviction. A cache currently being accessed is left alone.
/// Destruction happens after releasing the mutex, so the next cache miss does
/// not wait for all of an old project's strings and vectors to be freed.
pub fn evict_idle<T>(cache: &Mutex<IdleCache<T>>, timeout: Duration) -> bool {
    release(cache, |guard| guard.take_if_idle(Instant::now(), timeout))
}

/// Release a cache when its last client left, without waiting on a reader.
pub fn clear<T>(cache: &Mutex<IdleCache<T>>) -> bool {
    release(cache, IdleCache::take)
}

fn release<T>(
    cache: &Mutex<IdleCache<T>>,
    take: impl FnOnce(&mut IdleCache<T>) -> Option<T>,
) -> bool {
    let expired = match cache.try_lock() {
        Ok(mut guard) => take(&mut guard),
        Err(std::sync::TryLockError::WouldBlock) => None,
        Err(std::sync::TryLockError::Poisoned(poison)) => {
            // A tainted cache is never trusted or retained indefinitely.
            let mut guard = poison.into_inner();
            let removed = guard.take();
            cache.clear_poison();
            removed
        }
    };
    expired.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn expires_at_the_idle_boundary_and_repopulates() {
        let mut cache = IdleCache::default();
        cache.insert("first");
        let used = cache.last_used;
        let timeout = Duration::from_secs(5);
        assert!(cache
            .take_if_idle(used + timeout - Duration::from_nanos(1), timeout)
            .is_none());
        assert_eq!(cache.take_if_idle(used + timeout, timeout), Some("first"));
        cache.insert("rebuilt");
        assert_eq!(cache.get_if(|_| true), Some(&"rebuilt"));
    }

    #[test]
    fn only_a_matching_use_refreshes_the_idle_clock() {
        let mut cache = IdleCache::default();
        cache.insert(7);
        let old = Instant::now() - Duration::from_secs(10);
        cache.last_used = old;
        assert_eq!(cache.get_if(|value| *value == 8), None);
        assert_eq!(cache.last_used, old);
        assert_eq!(cache.get_if(|value| *value == 7), Some(&7));
        assert!(cache.last_used > old);
        assert!(cache
            .take_if_idle(Instant::now(), Duration::from_secs(5))
            .is_none());
    }

    #[test]
    fn zero_timeout_disables_eviction() {
        let mut cache = IdleCache::default();
        cache.insert(7);
        assert!(cache
            .take_if_idle(cache.last_used + Duration::from_secs(3600), Duration::ZERO)
            .is_none());
        assert_eq!(cache.get_if(|_| true), Some(&7));
    }

    #[test]
    fn eviction_does_not_invalidate_an_in_flight_owned_result() {
        let value = Arc::new(vec![1, 2, 3]);
        let mut cache = IdleCache::default();
        cache.insert(Arc::clone(&value));
        cache.last_used = Instant::now() - Duration::from_secs(10);
        let cache = Mutex::new(cache);
        assert!(evict_idle(&cache, Duration::from_secs(1)));
        assert_eq!(*value, vec![1, 2, 3]);
        assert_eq!(Arc::strong_count(&value), 1);
    }

    #[test]
    fn does_not_wait_for_a_cache_in_use() {
        let cache = Mutex::new(IdleCache::<u8>::default());
        let _guard = cache.lock().unwrap();
        assert!(!evict_idle(&cache, Duration::from_secs(1)));
    }

    #[test]
    fn a_poisoned_cache_is_discarded_and_can_rebuild() {
        let cache = Arc::new(Mutex::new(IdleCache::default()));
        let other = cache.clone();
        assert!(std::thread::spawn(move || {
            let mut guard = other.lock().unwrap();
            guard.insert("tainted");
            panic!("a cache user panicked");
        })
        .join()
        .is_err());
        assert!(cache.is_poisoned());
        assert!(evict_idle(&cache, Duration::from_secs(3600)));
        assert!(!cache.is_poisoned());
        let mut guard = cache.lock().unwrap();
        assert!(guard.get_if(|_| true).is_none());
        guard.insert("rebuilt");
        assert_eq!(guard.get_if(|_| true), Some(&"rebuilt"));
    }
}
