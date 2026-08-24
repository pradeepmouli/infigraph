//! `Task<T>`: one cancellable-task primitive for both long-running producer
//! loops (spawned via [`Task::spawn`]) and one-shot blocking work (spawned
//! via [`Task::spawn_blocking`]) — see
//! docs/superpowers/specs/2026-08-21-daemon-watch-command-split-design.md
//! "`Task<T>` — one shared primitive, not two".

use std::future::Future;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub struct Task<T> {
    pub role: &'static str,
    pub token: CancellationToken,
    handle: JoinHandle<T>,
}

impl<T: Send + 'static> Task<T> {
    /// Spawn a long-running async task. `make_fut` receives this task's own
    /// child token, so the future can `.await` `token.cancelled()` to know
    /// when to stop.
    pub fn spawn<F>(
        parent: &CancellationToken,
        role: &'static str,
        make_fut: impl FnOnce(CancellationToken) -> F,
    ) -> Self
    where
        F: Future<Output = T> + Send + 'static,
    {
        let token = parent.child_token();
        let handle = tokio::task::spawn(make_fut(token.clone()));
        Task {
            role,
            token,
            handle,
        }
    }

    /// Spawn one-shot blocking work on tokio's blocking-thread pool.
    /// `make_f` receives this task's own child token for synchronous
    /// `token.is_cancelled()` checkpoint polling (the closure runs
    /// synchronously, so it cannot `.await` cancellation).
    pub fn spawn_blocking(
        parent: &CancellationToken,
        role: &'static str,
        make_f: impl FnOnce(CancellationToken) -> T + Send + 'static,
    ) -> Self {
        let token = parent.child_token();
        let token_for_closure = token.clone();
        let handle = tokio::task::spawn_blocking(move || make_f(token_for_closure));
        Task {
            role,
            token,
            handle,
        }
    }

    /// Cancel the token and await the handle, discarding the result.
    /// Tolerant of a panicked task (logs, doesn't propagate the panic) --
    /// mirrors how `doc_thread.join()` is already tolerant of this today.
    pub async fn stop(self) {
        self.token.cancel();
        if let Err(e) = self.handle.await {
            eprintln!("[task:{}] stop: task panicked: {e}", self.role);
        }
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub async fn join(self) -> Result<T, tokio::task::JoinError> {
        self.handle.await
    }
}

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

pub struct TaskRegistry<K> {
    active: Arc<Mutex<HashSet<K>>>,
}

impl<K> Default for TaskRegistry<K> {
    fn default() -> Self {
        TaskRegistry {
            active: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl<K: Eq + Hash + Clone + Send + 'static> TaskRegistry<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically claim `key`. Returns `None` if it's already claimed --
    /// the caller should treat that as "busy, decline to spawn a
    /// duplicate" (mirrors `try_start_full_reindex`'s
    /// `if drain_in_flight || full_reindex_in_flight { return None; }`,
    /// generalized).
    pub fn try_claim(&self, key: K) -> Option<Claim<K>> {
        let mut active = self.active.lock().unwrap();
        if active.contains(&key) {
            None
        } else {
            active.insert(key.clone());
            Some(Claim {
                key,
                active: Arc::clone(&self.active),
            })
        }
    }
}

pub struct Claim<K: Eq + Hash> {
    key: K,
    active: Arc<Mutex<HashSet<K>>>,
}

impl<K: Eq + Hash> Drop for Claim<K> {
    fn drop(&mut self) {
        self.active.lock().unwrap().remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn spawn_runs_until_cancelled() {
        let parent = CancellationToken::new();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = ran.clone();
        let task = Task::spawn(&parent, "test", move |token| async move {
            ran_clone.store(true, Ordering::SeqCst);
            token.cancelled().await;
        });
        tokio::task::yield_now().await;
        assert!(ran.load(Ordering::SeqCst));
        task.stop().await;
    }

    #[tokio::test]
    async fn stop_tolerates_a_panicking_task() {
        let parent = CancellationToken::new();
        let task: Task<()> = Task::spawn(&parent, "test", |_token| async {
            panic!("boom");
        });
        task.stop().await;
    }

    #[tokio::test]
    async fn spawn_blocking_join_returns_the_value() {
        let parent = CancellationToken::new();
        let task = Task::spawn_blocking(&parent, "test", |_token| 42);
        let result = task.join().await.unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn spawn_blocking_checkpoint_sees_cancellation() {
        let parent = CancellationToken::new();
        let task = Task::spawn_blocking(&parent, "test", |token| {
            // Simulates a long-running blocking body with a checkpoint.
            std::thread::sleep(std::time::Duration::from_millis(50));
            token.is_cancelled()
        });
        parent.cancel();
        let saw_cancelled = task.join().await.unwrap();
        assert!(
            saw_cancelled,
            "checkpoint should observe the parent's cancellation"
        );
    }

    #[tokio::test]
    async fn is_finished_reflects_completion() {
        let parent = CancellationToken::new();
        let task = Task::spawn_blocking(&parent, "test", |_token| ());
        for _ in 0..100 {
            if task.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(task.is_finished());
        task.join().await.unwrap();
    }

    #[test]
    fn try_claim_declines_a_second_claim_on_the_same_key() {
        let registry: TaskRegistry<&str> = TaskRegistry::new();
        let first = registry
            .try_claim("code")
            .expect("first claim should succeed");
        let second = registry.try_claim("code");
        assert!(
            second.is_none(),
            "a live claim on the same key must decline a second one"
        );
        drop(first);
    }

    #[test]
    fn try_claim_allows_a_fresh_claim_after_the_first_drops() {
        let registry: TaskRegistry<&str> = TaskRegistry::new();
        let first = registry.try_claim("code").unwrap();
        drop(first);
        let second = registry.try_claim("code");
        assert!(
            second.is_some(),
            "dropping the first claim should free the key for a fresh one"
        );
    }

    #[test]
    fn different_keys_do_not_contend() {
        let registry: TaskRegistry<&str> = TaskRegistry::new();
        let code = registry.try_claim("code").unwrap();
        let docs = registry.try_claim("docs");
        assert!(
            docs.is_some(),
            "distinct keys must not contend with each other"
        );
        drop(code);
    }

    #[test]
    fn claim_releases_even_if_the_holder_panics() {
        let registry: TaskRegistry<&str> = TaskRegistry::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _claim = registry.try_claim("code").unwrap();
            panic!("simulated panic while holding the claim");
        }));
        assert!(result.is_err());
        // The Claim's Drop impl runs during unwind, so a fresh claim must
        // now succeed -- this is what prevents an aborted/panicked task
        // from leaving a permanent phantom "still running" entry.
        let fresh = registry.try_claim("code");
        assert!(
            fresh.is_some(),
            "a panicking holder must not leak its claim"
        );
    }
}
