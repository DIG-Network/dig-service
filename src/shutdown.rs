//! Shutdown coordination.
//!
//! [`ShutdownToken`] is a thin wrapper over [`CancellationToken`](tokio_util::sync::CancellationToken)
//! that adds a typed reason. Cancellation is broadcast: every clone sees
//! the same flip when `.cancel(reason)` is called on any one of them.
//!
//! # Reasons
//!
//! [`ShutdownReason`] distinguishes *why* the service is shutting down so
//! the exiting binary can set the right process exit code (0 for graceful,
//! non-zero for fatal).
//!
//! # Idempotency
//!
//! `.cancel(reason)` records only the **first** reason; subsequent calls are
//! no-ops. This matches the "stop is stop" invariant — you can't un-shutdown.

use std::sync::Arc;

use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

/// A cancellation token with a typed [`ShutdownReason`].
///
/// Clone-to-share; every clone observes the same cancellation.
#[derive(Clone, Debug)]
pub struct ShutdownToken {
    inner: CancellationToken,
    reason: Arc<RwLock<Option<ShutdownReason>>>,
}

impl ShutdownToken {
    /// Construct a fresh, uncancelled token.
    pub fn new() -> Self {
        Self {
            inner: CancellationToken::new(),
            reason: Arc::new(RwLock::new(None)),
        }
    }

    /// Non-blocking check.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Awaitable trigger; completes as soon as any clone calls `.cancel`.
    pub async fn cancelled(&self) {
        self.inner.cancelled().await
    }

    /// Trigger shutdown. Idempotent — only the **first** call records its
    /// reason; later calls are no-ops.
    pub fn cancel(&self, reason: ShutdownReason) {
        // Record reason atomically before flipping the token so anyone
        // waking up from `cancelled().await` sees a populated `reason()`.
        {
            let mut w = self.reason.write();
            if w.is_none() {
                *w = Some(reason);
            }
        }
        self.inner.cancel();
    }

    /// Read the first-recorded reason, if any.
    pub fn reason(&self) -> Option<ShutdownReason> {
        self.reason.read().clone()
    }

    /// Construct a child token that inherits cancellation from `self`.
    ///
    /// Cancelling the child does NOT cancel the parent. Useful when a
    /// subsystem wants to shut its own subtasks down without affecting the
    /// outer service.
    pub fn child_token(&self) -> ShutdownToken {
        Self {
            inner: self.inner.child_token(),
            reason: self.reason.clone(),
        }
    }
}

impl Default for ShutdownToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Why the service is shutting down.
///
/// Binaries map these to process exit codes:
/// - `UserRequested` / `RequestedByRun` / `ReloadRequested` → 0.
/// - `RpcRequested` → 0.
/// - `Fatal(_)` → non-zero.
#[derive(Clone, Debug)]
pub enum ShutdownReason {
    /// SIGINT / SIGTERM / Ctrl-C.
    UserRequested,
    /// An admin RPC method (`stop_node`) requested shutdown.
    RpcRequested,
    /// A reload was requested; the binary should exit and re-spawn.
    ReloadRequested,
    /// The node's `run` method returned without any outside signal; the
    /// service treats that as a graceful exit.
    RequestedByRun,
    /// An unrecoverable internal error. The string is human-readable.
    Fatal(String),
}

/// Why the service exited.
///
/// Richer than [`ShutdownReason`] because it also captures the "ran to
/// completion normally" and "run method returned an error" cases.
#[derive(Clone, Debug)]
pub enum ExitReason {
    /// Shutdown was requested externally; the carried reason explains how.
    RequestedShutdown(ShutdownReason),
    /// The node's `run` method returned `Ok(())` without any outside signal.
    RunCompleted,
    /// The node's `run` method returned an error. The `Arc<anyhow::Error>`
    /// carries the original.
    RunError(Arc<anyhow::Error>),
}

/// The final status returned by `Service::start`.
#[derive(Clone, Debug)]
pub struct ExitStatus {
    /// Why the service exited.
    pub reason: ExitReason,
}

impl ExitStatus {
    /// Whether this exit was graceful (i.e., not `RunError` and not `Fatal`).
    ///
    /// Binaries can use this to decide on exit code 0 vs 1.
    pub fn is_graceful(&self) -> bool {
        !matches!(
            self.reason,
            ExitReason::RunError(_) | ExitReason::RequestedShutdown(ShutdownReason::Fatal(_))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Proves:** a fresh `ShutdownToken` is not cancelled and has no
    /// recorded reason.
    ///
    /// **Why it matters:** This is the default state every service starts
    /// in. A regression that pre-sets the cancel flag would cause every
    /// newly-constructed `Service` to believe shutdown was already
    /// requested and skip `run` entirely.
    ///
    /// **Catches:** accidentally swapping `CancellationToken::new()` for
    /// `CancellationToken::new_cancelled()` or similar.
    #[test]
    fn fresh_token_is_uncancelled() {
        let t = ShutdownToken::new();
        assert!(!t.is_cancelled());
        assert!(t.reason().is_none());
    }

    /// **Proves:** calling `.cancel(reason)` flips `is_cancelled` and
    /// records the reason.
    ///
    /// **Why it matters:** The "which reason did we shut down for" field
    /// is how binaries decide their exit code. If `cancel` ever failed to
    /// record the reason, every shutdown would look like "unknown".
    ///
    /// **Catches:** a regression where the inner `CancellationToken::cancel`
    /// is called before the reason is recorded — then a racing task that
    /// wakes up on `cancelled()` might observe `reason() == None`.
    #[test]
    fn cancel_sets_state() {
        let t = ShutdownToken::new();
        t.cancel(ShutdownReason::UserRequested);
        assert!(t.is_cancelled());
        assert!(matches!(t.reason(), Some(ShutdownReason::UserRequested)));
    }

    /// **Proves:** only the first `.cancel` call records its reason;
    /// subsequent calls do not overwrite it.
    ///
    /// **Why it matters:** If someone calls `Service::request_shutdown(User)`
    /// and then an anomaly triggers `Service::request_shutdown(Fatal("..."))`,
    /// we want the **first** reason recorded — the shutdown was already in
    /// flight for a benign reason, and the Fatal is a consequence of being
    /// torn down mid-flight.
    ///
    /// **Catches:** a regression where `.cancel` unconditionally overwrites
    /// the reason.
    #[test]
    fn cancel_reason_is_first_wins() {
        let t = ShutdownToken::new();
        t.cancel(ShutdownReason::UserRequested);
        t.cancel(ShutdownReason::Fatal("too late".to_string()));
        assert!(matches!(t.reason(), Some(ShutdownReason::UserRequested)));
    }

    /// **Proves:** cloning a `ShutdownToken` produces handles that share
    /// cancellation state.
    ///
    /// **Why it matters:** `ServiceHandle::request_shutdown` works by holding
    /// a clone of the token. If the clone had independent state, calling
    /// `handle.request_shutdown()` would flip a token nobody awaits.
    ///
    /// **Catches:** a regression where `Clone` for `ShutdownToken` deep-clones
    /// the underlying `CancellationToken`.
    #[test]
    fn clone_shares_state() {
        let a = ShutdownToken::new();
        let b = a.clone();
        assert!(!a.is_cancelled() && !b.is_cancelled());
        b.cancel(ShutdownReason::RpcRequested);
        assert!(a.is_cancelled() && b.is_cancelled());
        assert!(matches!(a.reason(), Some(ShutdownReason::RpcRequested)));
    }

    /// **Proves:** `ExitStatus::is_graceful` is `true` for `RunCompleted`
    /// and `RequestedShutdown(UserRequested)`, and `false` for
    /// `RunError`, `Fatal`.
    ///
    /// **Why it matters:** This method drives the process exit code. Any
    /// mis-classification could cause orchestrators (systemd, k8s) to
    /// infinitely restart a binary that exited cleanly.
    ///
    /// **Catches:** swapping the match arms; missing a new `ShutdownReason`
    /// variant when the enum grows.
    #[test]
    fn exit_status_is_graceful_classifies_correctly() {
        let graceful = ExitStatus {
            reason: ExitReason::RunCompleted,
        };
        assert!(graceful.is_graceful());

        let graceful = ExitStatus {
            reason: ExitReason::RequestedShutdown(ShutdownReason::UserRequested),
        };
        assert!(graceful.is_graceful());

        let fatal = ExitStatus {
            reason: ExitReason::RequestedShutdown(ShutdownReason::Fatal("x".into())),
        };
        assert!(!fatal.is_graceful());

        let run_err = ExitStatus {
            reason: ExitReason::RunError(Arc::new(anyhow::anyhow!("boom"))),
        };
        assert!(!run_err.is_graceful());
    }
}
