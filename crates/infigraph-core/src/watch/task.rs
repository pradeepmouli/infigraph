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
}
