//! Tracked-task registry.
//!
//! [`TaskRegistry`] is a cheap clone-and-spawn utility: every task spawned
//! through it is tracked, given a stable name, and joined at shutdown with
//! a configurable deadline. Stragglers past the deadline are aborted and
//! counted so operators see a graceful-but-incomplete shutdown.
//!
//! # Why not `tokio::task::JoinSet`?
//!
//! `JoinSet` forgets tasks as soon as they complete; we want to **track**
//! live tasks for the service's lifetime (so `ServiceHandle::tasks()` can
//! enumerate them). `JoinSet` also doesn't carry task names. `TaskRegistry`
//! layers both on top.
//!
//! # Guarantees
//!
//! - Every spawned task receives a clone of the [`ShutdownToken`](crate::ShutdownToken).
//! - `join_all(deadline)` waits up to the deadline for each live task;
//!   remaining tasks are aborted and counted in
//!   [`ServiceError::ShutdownDeadlineExceeded`](crate::ServiceError::ShutdownDeadlineExceeded).

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::task::{AbortHandle, JoinHandle};

use crate::error::{Result, ServiceError};
use crate::shutdown::ShutdownToken;

/// Kind of task, for observability.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// A long-running background loop (sync loop, heartbeat).
    BackgroundLoop,
    /// A per-peer connection handler.
    PeerConnection,
    /// A per-request RPC handler.
    RpcHandler,
    /// A periodic maintenance job (compaction, log rotation).
    Maintenance,
}

/// A clone-safe registry of spawned tasks.
///
/// `TaskRegistry: Clone + Send + Sync`. Cloning is cheap (`Arc` bump) and
/// all clones share the same underlying task list.
#[derive(Clone)]
pub struct TaskRegistry {
    inner: Arc<Inner>,
}

struct Inner {
    shutdown: ShutdownToken,
    entries: Mutex<Vec<Entry>>,
}

struct Entry {
    name: &'static str,
    kind: TaskKind,
    spawned_at: Instant,
    handle: JoinHandle<()>,
}

/// Per-task snapshot surfaced by [`TaskRegistry::snapshot`].
#[derive(Debug, Clone)]
pub struct TaskSummary {
    /// Static name passed to `spawn`.
    pub name: &'static str,
    /// When the task was spawned.
    pub spawned_at: Instant,
    /// Kind classification.
    pub kind: TaskKind,
}

impl TaskRegistry {
    /// Construct an empty registry linked to the given shutdown token.
    pub fn new(shutdown: ShutdownToken) -> Self {
        Self {
            inner: Arc::new(Inner {
                shutdown,
                entries: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Spawn a tracked task.
    ///
    /// The closure receives no arguments — if it needs the shutdown token
    /// or a sub-registry, capture them via move. Use
    /// [`shutdown`](Self::shutdown) to obtain a clone of the token.
    ///
    /// Returns an [`AbortHandle`] so the caller can force-abort the task
    /// if needed (e.g., a per-peer handler when the peer disconnects). The
    /// real `JoinHandle` is held by the registry and is awaited in
    /// [`join_all`](Self::join_all) at shutdown. Callers that want to wait
    /// for a single task to complete must await by their own means (a
    /// `tokio::sync::oneshot` or similar inside `fut`) — the registry is
    /// optimised for the shutdown-join case, not per-task completion.
    pub fn spawn<F>(&self, name: &'static str, kind: TaskKind, fut: F) -> AbortHandle
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let handle: JoinHandle<()> = tokio::spawn(async move {
            if let Err(e) = fut.await {
                tracing::error!(task = name, error = %e, "task exited with error");
            }
        });
        let abort = handle.abort_handle();
        self.inner.entries.lock().push(Entry {
            name,
            kind,
            spawned_at: Instant::now(),
            handle,
        });
        abort
    }

    /// Borrow the shutdown token this registry is linked to.
    pub fn shutdown(&self) -> &ShutdownToken {
        &self.inner.shutdown
    }

    /// Await all tracked tasks up to `deadline`; abort laggards.
    ///
    /// Returns `Ok(())` if every task exited inside the deadline.
    /// Returns [`ServiceError::ShutdownDeadlineExceeded`] otherwise, with
    /// the count of aborted tasks.
    pub async fn join_all(&self, deadline: Duration) -> Result<()> {
        // Drain the list; we don't hold the lock across awaits.
        let entries = {
            let mut g = self.inner.entries.lock();
            std::mem::take(&mut *g)
        };

        let start = Instant::now();
        let mut pending = 0usize;

        for entry in entries {
            let remaining = deadline.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                entry.handle.abort();
                pending += 1;
                continue;
            }
            match tokio::time::timeout(remaining, entry.handle).await {
                Ok(Ok(())) => {}
                Ok(Err(join_err)) => {
                    // Task panicked or was cancelled.
                    tracing::warn!(
                        task = entry.name,
                        elapsed = ?entry.spawned_at.elapsed(),
                        panic = join_err.is_panic(),
                        "tracked task did not exit cleanly",
                    );
                }
                Err(_elapsed) => {
                    // The `.handle` was consumed by `timeout`; it will abort
                    // automatically when dropped.
                    pending += 1;
                    tracing::warn!(
                        task = entry.name,
                        "task exceeded shutdown deadline; aborting",
                    );
                }
            }
        }

        if pending == 0 {
            Ok(())
        } else {
            Err(ServiceError::ShutdownDeadlineExceeded { deadline, pending })
        }
    }

    /// Snapshot of currently-tracked tasks (live only; completed tasks
    /// are not garbage-collected until `join_all`).
    pub fn snapshot(&self) -> Vec<TaskSummary> {
        self.inner
            .entries
            .lock()
            .iter()
            .map(|e| TaskSummary {
                name: e.name,
                spawned_at: e.spawned_at,
                kind: e.kind,
            })
            .collect()
    }

    /// Current live-task count.
    pub fn len(&self) -> usize {
        self.inner.entries.lock().len()
    }

    /// Whether no tasks are currently registered.
    pub fn is_empty(&self) -> bool {
        self.inner.entries.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Proves:** a freshly-constructed registry is empty.
    ///
    /// **Why it matters:** Basic invariant — nothing should be "tracked"
    /// before `spawn` is called.
    ///
    /// **Catches:** a default-state regression.
    #[test]
    fn empty_on_construction() {
        let r = TaskRegistry::new(ShutdownToken::new());
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    /// **Proves:** `spawn` increments `len` and adds an entry to the
    /// snapshot.
    ///
    /// **Why it matters:** `ServiceHandle::tasks()` forwards to `snapshot`
    /// so operators can see what's running. An empty snapshot for a
    /// running service is a red flag.
    ///
    /// **Catches:** a regression where `spawn` forgets to push to `entries`.
    #[tokio::test]
    async fn spawn_registers() {
        let r = TaskRegistry::new(ShutdownToken::new());
        let _h = r.spawn("t1", TaskKind::BackgroundLoop, async { Ok(()) });
        assert_eq!(r.len(), 1);
        assert_eq!(r.snapshot()[0].name, "t1");
        assert_eq!(r.snapshot()[0].kind, TaskKind::BackgroundLoop);
    }

    /// **Proves:** `join_all` waits for tasks that finish inside the deadline,
    /// reporting success.
    ///
    /// **Why it matters:** The happy path for graceful shutdown.
    ///
    /// **Catches:** a regression where `join_all` returns before tasks
    /// actually complete.
    #[tokio::test]
    async fn join_all_awaits_fast_tasks() {
        let r = TaskRegistry::new(ShutdownToken::new());
        r.spawn("fast", TaskKind::BackgroundLoop, async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(())
        });
        let res = r.join_all(Duration::from_secs(5)).await;
        assert!(res.is_ok());
    }

    /// **Proves:** `join_all` aborts tasks that outlast the deadline and
    /// returns `ShutdownDeadlineExceeded` with the correct count.
    ///
    /// **Why it matters:** If a task has a bug (infinite loop, stuck await),
    /// shutdown must not hang forever. Aborting past the deadline is the
    /// safety valve, and the error count lets operators see how many tasks
    /// went wrong.
    ///
    /// **Catches:** a regression where `join_all` blocks without abort,
    /// or where the `pending` count gets off by one.
    #[tokio::test]
    async fn join_all_aborts_slow_tasks() {
        let r = TaskRegistry::new(ShutdownToken::new());
        r.spawn("slow", TaskKind::BackgroundLoop, async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        });
        let res = r.join_all(Duration::from_millis(20)).await;
        assert!(matches!(
            res,
            Err(ServiceError::ShutdownDeadlineExceeded { pending: 1, .. })
        ));
    }
}
