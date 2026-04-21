//! Out-of-lifetime service handle.
//!
//! A [`ServiceHandle`] is a clone-safe, shareable reference into a running
//! `Service`. Intended for:
//!
//! - OS signal handlers that need to call `request_shutdown`.
//! - The RPC layer invoking admin operations like `stop_node`.
//! - Tests that need a way to tear down the service from the outside.
//!
//! Handles remain valid after the owning `Service` has dropped — their
//! operations become no-ops rather than panicking.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::shutdown::{ShutdownReason, ShutdownToken};
use crate::tasks::TaskRegistry;
pub use crate::tasks::TaskSummary;

/// Shared state between a `Service` and its handles.
///
/// Not part of the public surface — but the `pub(crate)` fields are used
/// by `Service::new` / `Service::handle`.
#[derive(Debug)]
pub(crate) struct HandleState {
    pub(crate) name: &'static str,
}

impl HandleState {
    pub(crate) fn new(name: &'static str) -> Self {
        Self { name }
    }
}

/// A cheap-clone handle into a running `Service`.
#[derive(Clone)]
pub struct ServiceHandle {
    shutdown: ShutdownToken,
    tasks: TaskRegistry,
    state: Arc<RwLock<HandleState>>,
}

impl ServiceHandle {
    /// Construct from pre-owned inner components. Only called by
    /// `Service::handle`; not public.
    pub(crate) fn new(
        shutdown: ShutdownToken,
        tasks: TaskRegistry,
        state: Arc<RwLock<HandleState>>,
    ) -> Self {
        Self {
            shutdown,
            tasks,
            state,
        }
    }

    /// The service's static name (from `NodeLifecycle::NAME`). Empty string
    /// if the node chose not to declare one.
    pub fn name(&self) -> &'static str {
        self.state.read().name
    }

    /// Request a graceful shutdown. Idempotent.
    pub fn request_shutdown(&self, reason: ShutdownReason) {
        self.shutdown.cancel(reason);
    }

    /// Whether the underlying service is still running (i.e., shutdown has
    /// not been cancelled).
    pub fn is_running(&self) -> bool {
        !self.shutdown.is_cancelled()
    }

    /// Borrow the shutdown token. Mostly useful in tests.
    pub fn shutdown_token(&self) -> &ShutdownToken {
        &self.shutdown
    }

    /// Snapshot of all currently-tracked tasks.
    pub fn tasks(&self) -> Vec<TaskSummary> {
        self.tasks.snapshot()
    }
}

impl std::fmt::Debug for ServiceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceHandle")
            .field("name", &self.state.read().name)
            .field("is_running", &self.is_running())
            .field("tasks", &self.tasks.len())
            .finish()
    }
}
