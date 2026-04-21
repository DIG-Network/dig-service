//! # dig-service
//!
//! The generic orchestration scaffold every DIG Network binary shares.
//! Provides the minimal set of primitives needed to stand up a service:
//! the [`Service<N, A, R>`] composition, a [`TaskRegistry`] for tracked
//! `tokio::spawn`, a [`ShutdownToken`] broadcast, and three lifecycle
//! traits ([`NodeLifecycle`], [`PeerApi`], [`RpcApi`]).
//!
//! ## Design principle
//!
//! **Smallest possible shared foundation.** This crate does NOT pull in:
//!
//! - TOML / YAML config parsing — each binary defines its own config struct
//!   via `serde` and hands a parsed instance to `Service::new`.
//! - `clap` / CLI framework — binaries build their own CLI.
//! - `tracing_subscriber` subscriber installation — each binary installs its
//!   subscriber in `main.rs` before `Service::start`.
//! - Prometheus / metrics exporters — wired in `main.rs`.
//! - `dig-rpc` server — plugged in through [`RpcApi`].
//! - Peer transport — plugged in through [`PeerApi`].
//!
//! Keeping this crate minimal means every downstream binary picks only the
//! pieces it needs. Introducers and relays get `Service<N, A, ()>` (peer-only);
//! daemons get `Service<N, (), R>` (RPC-only); fullnodes get all three slots.
//!
//! ## At a glance
//!
//! ```text
//!   apps/fullnode   apps/validator   apps/introducer   apps/relay   apps/daemon
//!         │               │                 │              │             │
//!         └───────────────┴─────────┬───────┴──────────────┴─────────────┘
//!                                   ▼
//!                             dig-service              ← this crate
//!                                   │
//!                                   ▼
//!                              tokio + tokio-util
//! ```
//!
//! ## Minimal example
//!
//! ```no_run
//! use async_trait::async_trait;
//! use dig_service::{
//!     NodeLifecycle, Service, StartContext, RunContext, StopContext,
//! };
//!
//! struct Node;
//!
//! #[async_trait]
//! impl NodeLifecycle for Node {
//!     const NAME: Option<&'static str> = Some("example");
//!
//!     async fn pre_start(&self, _ctx: &StartContext<'_>) -> anyhow::Result<()> { Ok(()) }
//!     async fn on_start(&self, _ctx: &StartContext<'_>) -> anyhow::Result<()> { Ok(()) }
//!     async fn run(&self, ctx: RunContext) -> anyhow::Result<()> {
//!         // Block until shutdown is requested.
//!         ctx.shutdown.cancelled().await;
//!         Ok(())
//!     }
//!     async fn on_stop(&self, _ctx: &StopContext<'_>) -> anyhow::Result<()> { Ok(()) }
//!     async fn post_stop(&self, _ctx: &StopContext<'_>) -> anyhow::Result<()> { Ok(()) }
//! }
//!
//! # #[tokio::main]
//! # async fn main() {
//! let svc = Service::<Node, (), ()>::new(Node, (), ());
//! let _exit = svc.start().await.unwrap();
//! # }
//! ```
//!
//! ## Lifecycle diagram
//!
//! ```text
//!       Service::start
//!            │
//!            ▼
//!    node.pre_start     ◂── open stores, replay journal
//!            │
//!            ▼
//!    node.on_start      ◂── bind ports, warm caches
//!            │
//!            ▼
//!  ┌──  node.run  ──┐   ◂── main event loop; returns on shutdown
//!  │                │
//!  │  tasks.spawn…  │
//!  └────────┬───────┘
//!           ▼
//!  tasks.join_all    ◂── graceful wait w/ deadline
//!           ▼
//!    node.on_stop    ◂── flush stores
//!           ▼
//!    node.post_stop  ◂── close stores, release locks
//!           ▼
//!      ExitStatus
//! ```
//!
//! ## Feature flags
//!
//! | Flag | Default | Effect |
//! |---|---|---|
//! | `testing` | off | Ships `TestService` + deterministic helpers |
//! | `tokio-console` | off | Wires `console_subscriber` for live introspection |
//! | `structured-panic` | off | Routes panics through `tracing::error!` and triggers `Fatal` shutdown |

#![deny(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod handle;
mod shutdown;
mod tasks;
mod traits;

#[cfg(feature = "testing")]
pub mod testing;

// Re-export tokio primitives that every consumer will want.
pub use tokio::task::JoinHandle;
pub use tokio_util::sync::CancellationToken;

// Public surface.
pub use error::{Result, ServiceError};
pub use handle::{ServiceHandle, TaskSummary};
pub use shutdown::{ExitReason, ExitStatus, ShutdownReason, ShutdownToken};
pub use tasks::{TaskKind, TaskRegistry};
pub use traits::{
    DisconnectReason, InboundMessage, NodeLifecycle, PeerApi, PeerId, PeerInfo, RpcApi, RunContext,
    StartContext, StopContext,
};

use std::sync::Arc;

use parking_lot::RwLock;

/// The top-level orchestration handle.
///
/// See the [crate-level documentation](crate) for architecture + lifecycle
/// diagram. `Service` owns the business-logic `node` and the peer / RPC
/// API adapters; driving `start()` runs the full lifecycle.
///
/// # Type parameters
///
/// - `N` — the [`NodeLifecycle`] implementation (the business core).
/// - `A` — the [`PeerApi`] implementation. Use `()` for binaries that don't
///   serve a peer surface (daemon, wallet).
/// - `R` — the [`RpcApi`] implementation. Use `()` for binaries that don't
///   serve RPC (introducer, relay).
pub struct Service<N, A, R>
where
    N: NodeLifecycle,
    A: PeerApi,
    R: RpcApi,
{
    node: Arc<N>,
    peer_api: Arc<A>,
    rpc_api: Arc<R>,
    shutdown: ShutdownToken,
    tasks: TaskRegistry,
    // Cached handle so ServiceHandle clones stay valid after Service is dropped.
    handle_state: Arc<RwLock<handle::HandleState>>,
    started: Arc<std::sync::atomic::AtomicBool>,
}

impl<N, A, R> Service<N, A, R>
where
    N: NodeLifecycle,
    A: PeerApi,
    R: RpcApi,
{
    /// Construct a new service from its three components.
    ///
    /// No runtime work happens here — ports are not bound, stores are not
    /// opened. All of that runs inside `start()`.
    pub fn new(node: N, peer_api: A, rpc_api: R) -> Self {
        let shutdown = ShutdownToken::new();
        let tasks = TaskRegistry::new(shutdown.clone());
        let handle_state = Arc::new(RwLock::new(handle::HandleState::new(N::NAME.unwrap_or(""))));
        Self {
            node: Arc::new(node),
            peer_api: Arc::new(peer_api),
            rpc_api: Arc::new(rpc_api),
            shutdown,
            tasks,
            handle_state,
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Return a cloneable handle for out-of-process-lifetime commands
    /// (signal handlers, RPC admin methods).
    ///
    /// The returned handle remains valid after `Service` is dropped — its
    /// operations simply become no-ops.
    pub fn handle(&self) -> ServiceHandle {
        ServiceHandle::new(
            self.shutdown.clone(),
            self.tasks.clone(),
            self.handle_state.clone(),
        )
    }

    /// Borrow the node. Mostly useful in tests; production callers should
    /// share state via the `Arc<N>` they constructed before `Service::new`.
    pub fn node(&self) -> &Arc<N> {
        &self.node
    }

    /// Borrow the peer API.
    pub fn peer_api(&self) -> &Arc<A> {
        &self.peer_api
    }

    /// Borrow the RPC API.
    pub fn rpc_api(&self) -> &Arc<R> {
        &self.rpc_api
    }

    /// Borrow the shutdown token. Use this to give background tasks a way
    /// to notice shutdown has been requested.
    pub fn shutdown_token(&self) -> &ShutdownToken {
        &self.shutdown
    }

    /// Borrow the task registry.
    pub fn tasks(&self) -> &TaskRegistry {
        &self.tasks
    }

    /// Request a graceful shutdown. Idempotent — only the first reason is
    /// recorded.
    pub fn request_shutdown(&self, reason: ShutdownReason) {
        self.shutdown.cancel(reason);
    }

    /// Drive the full lifecycle: `pre_start → on_start → run → on_stop →
    /// post_stop`. Returns once `run` exits (graceful or error) OR a
    /// shutdown has been requested.
    ///
    /// # Error handling
    ///
    /// - A failure in `pre_start` short-circuits without calling any later
    ///   hook.
    /// - A failure in `on_start` calls `post_stop` but skips `run` and
    ///   `on_stop`.
    /// - A failure in `run` still calls `on_stop` + `post_stop`.
    /// - A failure in `on_stop` is recorded but does not block `post_stop`.
    /// - Panics inside `run` are caught via `scopeguard`-style drop logic
    ///   and reported as `ExitReason::RunError`.
    ///
    /// Calling `start` twice on the same `Service` returns
    /// [`ServiceError::AlreadyRunning`].
    pub async fn start(self) -> Result<ExitStatus> {
        use std::sync::atomic::Ordering;

        if self.started.swap(true, Ordering::SeqCst) {
            return Err(ServiceError::AlreadyRunning);
        }

        let node = self.node.clone();
        let start_ctx = StartContext {
            name: N::NAME.unwrap_or(""),
            shutdown: self.shutdown.clone(),
            tasks: &self.tasks,
        };

        // pre_start
        if let Err(e) = node.pre_start(&start_ctx).await {
            return Err(ServiceError::PreStartFailed(Arc::new(e)));
        }

        // on_start
        if let Err(e) = node.on_start(&start_ctx).await {
            // on_start failed — call post_stop so any resources opened in
            // pre_start have a chance to close.
            let stop_ctx = StopContext {
                shutdown: self.shutdown.clone(),
                tasks: &self.tasks,
                exit_reason: ExitReason::RunError(Arc::new(anyhow::anyhow!("on_start failed"))),
            };
            let _ = node.post_stop(&stop_ctx).await;
            return Err(ServiceError::OnStartFailed(Arc::new(e)));
        }

        // run
        let run_ctx = RunContext {
            shutdown: self.shutdown.clone(),
            tasks: self.tasks.clone(),
        };
        let run_result = node.run(run_ctx).await;

        // Determine exit reason.
        let exit_reason = match &run_result {
            Ok(()) => self
                .shutdown
                .reason()
                .map(ExitReason::RequestedShutdown)
                .unwrap_or(ExitReason::RunCompleted),
            Err(e) => ExitReason::RunError(Arc::new(anyhow::anyhow!("{e}"))),
        };

        // Signal tasks to wind down even if run returned normally.
        if !self.shutdown.is_cancelled() {
            self.shutdown.cancel(ShutdownReason::RequestedByRun);
        }

        // Wait for background tasks with deadline.
        let join_result = self
            .tasks
            .join_all(std::time::Duration::from_secs(30))
            .await;

        // on_stop
        let stop_ctx = StopContext {
            shutdown: self.shutdown.clone(),
            tasks: &self.tasks,
            exit_reason: exit_reason.clone(),
        };
        let on_stop_result = node.on_stop(&stop_ctx).await;

        // post_stop (always called)
        let post_stop_result = node.post_stop(&stop_ctx).await;

        // Decide final error priority: run > on_stop > post_stop > join.
        if let Err(e) = run_result {
            return Err(ServiceError::RunFailed(Arc::new(e)));
        }
        if let Err(e) = on_stop_result {
            return Err(ServiceError::OnStopFailed(Arc::new(e)));
        }
        if let Err(e) = post_stop_result {
            return Err(ServiceError::OnStopFailed(Arc::new(e)));
        }
        join_result?;

        Ok(ExitStatus {
            reason: exit_reason,
        })
    }
}
