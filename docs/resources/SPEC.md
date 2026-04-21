---
title: dig-service — SPEC
status: design spec
last_updated: 2026-04-21
audience: crate implementers, reviewers, downstream binary authors
authoritative_sources:
  - docs/resources/03-appendices/10-crate-scope-refined.md (parent workspace)
  - apps/ARCHITECTURE.md (parent workspace) — §2, §4, §5
  - apps/fullnode/SPEC.md, apps/validator/SPEC.md — consumers
---

# dig-service — Specification

The generic orchestration scaffold every DIG binary shares. Provides the `Service<N, A, R>` composition, a `TaskRegistry` for tracked `tokio::spawn`, and a `ShutdownToken` broadcast. No config parsing, no logging setup, no Axum — `dig-service` is the smallest possible shared foundation.

## Scope

**In scope.**

- A generic `Service<N: NodeLifecycle, A: PeerApi, R: RpcApi>` struct binaries compose.
- Lifecycle traits: `NodeLifecycle`, `PeerApi`, `RpcApi`.
- `TaskRegistry`: bounded `tokio::spawn` tracking with graceful shutdown and abort-on-deadline fallback.
- `ShutdownToken`: a thin wrapper over `tokio_util::sync::CancellationToken` with typed shutdown reasons and a broadcast fan-out.
- Tiny startup/shutdown orchestrator with hook ordering: `pre_start` → `on_start` → `run` → `on_stop` → `post_stop`.
- `ServiceHandle` for out-of-process-lifetime commands (reload, stats).

**Out of scope (will NOT live here).**

- TOML / YAML config parsing. Each binary defines its own config struct with `serde`; a constructor for `Service` takes a pre-parsed config.
- `clap` subcommand definitions. Each binary builds its own CLI.
- `tracing_subscriber` installation. Each binary installs its subscriber in `main.rs` before `Service::start`.
- Metrics exporters. Each binary wires Prometheus (if any) in `main.rs`.
- Axum RPC server. Lives in `dig-rpc`.
- mTLS peer transport. Lives in `dig-gossip`.
- Any business logic.

## Placement in the Stack

```
              apps/fullnode   apps/validator   apps/introducer   apps/relay   apps/daemon
                     │               │                │              │             │
                     └───────────────┴────────┬───────┴──────────────┴─────────────┘
                                              ▼
                                        dig-service          ← this crate
                                              │
                                              ▼
                                          tokio + tokio-util
```

`dig-service` sits above `tokio` and below every DIG binary. It depends on nothing DIG-specific; it does not import `dig-block`, `dig-coinstore`, `dig-gossip`, `dig-rpc`, etc. Binaries compose those crates *through* `dig-service` via the `NodeLifecycle`, `PeerApi`, `RpcApi` traits.

## Public API

### Re-exports

```rust
pub use tokio_util::sync::CancellationToken;
pub use tokio::task::JoinHandle;
```

### Traits

```rust
/// Business-logic core of a service. The `Node` is the owner of all in-memory
/// state (stores, pools, caches). It implements startup + shutdown hooks.
#[async_trait::async_trait]
pub trait NodeLifecycle: Send + Sync + 'static {
    /// Name used in logs / metrics labels.
    fn name(&self) -> &'static str;

    /// Called before any peer or RPC server binds.
    /// Typical work: open stores, replay journal, restore state.
    async fn pre_start(&self, ctx: &StartContext) -> Result<()>;

    /// Called after peer + RPC servers are bound but before the main run loop.
    /// Typical work: announce capabilities, load in-memory caches.
    async fn on_start(&self, ctx: &StartContext) -> Result<()>;

    /// Main run loop. Owns the service until `ctx.shutdown.cancelled()` fires.
    /// Should register all long-running background loops through `ctx.tasks`.
    async fn run(&self, ctx: RunContext) -> Result<()>;

    /// Called once `run` returns (graceful exit).
    /// Typical work: flush stores, write snapshot markers.
    async fn on_stop(&self, ctx: &StopContext) -> Result<()>;

    /// Called after peer + RPC servers are torn down.
    /// Typical work: close stores, release file locks.
    async fn post_stop(&self, ctx: &StopContext) -> Result<()>;
}

/// Peer-protocol dispatcher. Receives inbound opcodes from `dig-gossip`.
#[async_trait::async_trait]
pub trait PeerApi: Send + Sync + 'static {
    /// Called for every inbound wire message after handshake succeeds.
    async fn on_message(&self, peer: PeerId, msg: InboundMessage) -> Result<()>;

    /// Called when a peer connects (post-handshake).
    async fn on_peer_connected(&self, peer: PeerId, info: PeerInfo);

    /// Called when a peer disconnects.
    async fn on_peer_disconnected(&self, peer: PeerId, reason: DisconnectReason);
}

/// RPC dispatcher. Implementations route JSON-RPC methods to handlers.
/// The concrete server lives in `dig-rpc`; this trait is the hand-off point.
#[async_trait::async_trait]
pub trait RpcApi: Send + Sync + 'static {
    /// Called for every inbound RPC request. `method` is already parsed.
    async fn dispatch(&self, method: &str, params: serde_json::Value)
        -> Result<serde_json::Value, RpcError>;

    /// Optional health probe. Default: Ok(()).
    async fn healthz(&self) -> Result<(), RpcError> { Ok(()) }
}
```

Notes on the trait carve-up:

- `PeerApi` and `RpcApi` are **separate** traits because binaries like `dig-introducer` need only `PeerApi` (no RPC) and `dig-daemon` needs only `RpcApi` (no peer transport). Using two traits lets `Service<N, A, R>` be instantiated with `()` in either slot (see Composition below).
- `NodeLifecycle` is separate from both so the business-logic core can be authored once and re-used with different RPC or peer surfaces (e.g., a testnet vs mainnet variant).

### `Service<N, A, R>`

```rust
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
    // …
}

impl<N, A, R> Service<N, A, R>
where
    N: NodeLifecycle,
    A: PeerApi,
    R: RpcApi,
{
    pub fn new(node: N, peer_api: A, rpc_api: R) -> Self;

    /// Returns a handle that can be cloned and used to request shutdown or query status.
    pub fn handle(&self) -> ServiceHandle;

    /// Drives the full lifecycle: pre_start → on_start → run → on_stop → post_stop.
    /// Returns once `run` returns (graceful) or a shutdown is requested.
    pub async fn start(self) -> Result<ExitStatus>;

    /// Requests a graceful shutdown. Idempotent.
    pub fn request_shutdown(&self, reason: ShutdownReason);

    /// Blocks until the service has fully stopped. Callable from any task.
    pub async fn wait_for_exit(&self) -> ExitStatus;
}
```

Use `()` for unused slots:

```rust
// Fullnode: all three
let service = Service::new(fullnode_node, fullnode_peer_api, fullnode_rpc_api);

// Daemon: RPC only
let service: Service<DaemonNode, (), DaemonRpcApi> = Service::new(
    daemon_node, (), daemon_rpc_api,
);

// Introducer: peer-only
let service: Service<IntroducerNode, IntroducerPeerApi, ()> = Service::new(
    introducer_node, introducer_peer_api, (),
);
```

Blanket impls of `PeerApi` and `RpcApi` for `()` no-op everything.

### `StartContext`, `RunContext`, `StopContext`

```rust
pub struct StartContext<'a> {
    pub name: &'static str,
    pub shutdown: ShutdownToken,
    pub tasks: &'a TaskRegistry,
}

pub struct RunContext {
    pub shutdown: ShutdownToken,
    pub tasks: TaskRegistry,
}

pub struct StopContext<'a> {
    pub shutdown: ShutdownToken,
    pub tasks: &'a TaskRegistry,
    pub exit_reason: ExitReason,
}

pub enum ExitReason {
    RequestedShutdown(ShutdownReason),
    RunCompleted,
    RunError(Arc<anyhow::Error>),
}

pub enum ShutdownReason {
    UserRequested,  // SIGINT / SIGTERM / ctrl-c
    RpcRequested,   // admin called /v1/stop_node
    ReloadRequested,// SIGHUP or rpc reload
    Fatal(String),  // unrecoverable internal error
}
```

### `TaskRegistry`

Tracks every long-running task spawned during the lifecycle. On shutdown, cancellation propagates via the shared `ShutdownToken`; tasks that don't exit within the configured deadline are force-aborted.

```rust
pub struct TaskRegistry { /* internals */ }

impl TaskRegistry {
    pub fn new(shutdown: ShutdownToken) -> Self;

    /// Spawn a tracked task. The closure receives a clone of the shutdown token.
    /// Task is aborted on shutdown_deadline if it hasn't exited by then.
    pub fn spawn<F>(&self, name: &'static str, fut: F) -> JoinHandle<()>
    where
        F: Future<Output = Result<()>> + Send + 'static;

    /// Await all tracked tasks. Returns the first error if any.
    /// Blocks up to `shutdown_deadline` after `ShutdownToken` fires.
    pub async fn join_all(&self, deadline: Duration) -> Result<()>;

    /// Snapshot of currently-tracked tasks (for debug / RPC).
    pub fn snapshot(&self) -> Vec<TaskSummary>;
}

pub struct TaskSummary {
    pub name: &'static str,
    pub spawned_at: Instant,
    pub kind: TaskKind,
}

pub enum TaskKind {
    BackgroundLoop,
    PeerConnection,
    RpcHandler,
    Maintenance,
}
```

### `ShutdownToken`

```rust
#[derive(Clone)]
pub struct ShutdownToken {
    inner: CancellationToken,
    reason: Arc<RwLock<Option<ShutdownReason>>>,
}

impl ShutdownToken {
    pub fn new() -> Self;

    /// Non-blocking check.
    pub fn is_cancelled(&self) -> bool;

    /// Awaitable trigger.
    pub async fn cancelled(&self);

    /// Triggers shutdown. Records the first reason; subsequent calls are no-ops.
    pub fn cancel(&self, reason: ShutdownReason);

    /// Reads the recorded reason.
    pub fn reason(&self) -> Option<ShutdownReason>;

    /// Creates a child token that inherits cancellation.
    pub fn child_token(&self) -> ShutdownToken;
}
```

### `ServiceHandle`

A `Clone + Send + Sync` reference to a running service. Intended for:

- OS signal handlers calling `request_shutdown`.
- The RPC layer invoking admin operations.
- Tests that need to tear down a service.

```rust
#[derive(Clone)]
pub struct ServiceHandle { /* internals */ }

impl ServiceHandle {
    pub fn request_shutdown(&self, reason: ShutdownReason);
    pub fn is_running(&self) -> bool;
    pub fn tasks(&self) -> Vec<TaskSummary>;
}
```

## Lifecycle Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│ main.rs                                                         │
│  1. install tracing subscriber                                  │
│  2. parse CLI + config                                          │
│  3. build node, peer_api, rpc_api                               │
│  4. let svc = Service::new(node, peer_api, rpc_api);            │
│  5. let handle = svc.handle();                                  │
│  6. install signal handler → handle.request_shutdown(UserReq)   │
│  7. svc.start().await  ◀─── drives the whole lifecycle          │
└─────────────────────────────────────────────────────────────────┘
                       │
                       ▼
        ┌──────────────────────────────────┐
        │ Service::start                   │
        │                                  │
        │  node.pre_start(ctx).await       │ ← open stores, replay journal
        │  node.on_start(ctx).await        │ ← bind ports, warm caches
        │                                  │
        │  ┌────────────────────────────┐  │
        │  │ node.run(ctx).await        │  │ ← main event loop
        │  │   ↑ spawns background      │  │
        │  │     loops via tasks        │  │
        │  │   ↑ exits on shutdown      │  │
        │  └────────────────────────────┘  │
        │                                  │
        │  tasks.join_all(deadline).await  │ ← graceful task wind-down
        │  node.on_stop(ctx).await         │ ← flush stores
        │  node.post_stop(ctx).await       │ ← close stores
        │                                  │
        │  return ExitStatus               │
        └──────────────────────────────────┘
```

## Error Variants

```rust
#[derive(thiserror::Error, Debug)]
pub enum ServiceError {
    #[error("pre_start failed: {0}")]
    PreStartFailed(#[source] Arc<anyhow::Error>),

    #[error("on_start failed: {0}")]
    OnStartFailed(#[source] Arc<anyhow::Error>),

    #[error("run exited with error: {0}")]
    RunFailed(#[source] Arc<anyhow::Error>),

    #[error("on_stop failed: {0}")]
    OnStopFailed(#[source] Arc<anyhow::Error>),

    #[error("task registry deadline exceeded ({deadline:?}); {pending} tasks aborted")]
    ShutdownDeadlineExceeded { deadline: Duration, pending: usize },

    #[error("service is already running")]
    AlreadyRunning,

    #[error("service has already stopped")]
    AlreadyStopped,
}

pub type Result<T> = std::result::Result<T, ServiceError>;
```

`RpcError` lives in `dig-rpc-types` (shared wire contract) and is re-exported by `dig-rpc`; `dig-service` imports it only via the `RpcApi::dispatch` return type.

## Dependencies

```toml
[dependencies]
tokio = { version = "1", features = ["rt", "rt-multi-thread", "sync", "time", "macros"] }
tokio-util = { version = "0.7", features = ["rt"] }
async-trait = "0.1"
tracing = "0.1"            # facade only — no subscriber setup
thiserror = "1"
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"           # for RpcApi::dispatch signature
futures = "0.3"
parking_lot = "0.12"

# workspace re-used types
dig-rpc-types = { path = "../dig-rpc-types" }   # for RpcError (cyclic? see below)
```

**Cycle concern.** `dig-rpc-types` is leaf-level (no dependencies on dig crates). `dig-service` depends on it for `RpcError`. `dig-rpc` depends on `dig-service` (for `RpcApi`) and `dig-rpc-types`. No cycle.

## Consumers

| Consumer | Trait impls |
|---|---|
| `apps/fullnode` | `NodeLifecycle` (FullNode), `PeerApi` (FullNodeApi), `RpcApi` (FullNodeRpcApi) |
| `apps/validator` | `NodeLifecycle` (Validator), `PeerApi` (ValidatorApi — minimal, fullnode-only), `RpcApi` (ValidatorRpcApi) |
| `apps/introducer` | `NodeLifecycle` (Introducer), `PeerApi` (IntroducerApi), `()` for RPC |
| `apps/relay` | `NodeLifecycle` (Relay), `PeerApi` (RelayApi), `()` for RPC |
| `apps/daemon` | `NodeLifecycle` (Daemon), `()` for Peer, `RpcApi` (DaemonRpcApi) |
| `apps/wallet` (future) | `NodeLifecycle` (Wallet), `()` for Peer, `RpcApi` (WalletRpcApi) |

## Invariants

| ID | Invariant | Enforcer |
|---|---|---|
| SVC-001 | `on_start` is called exactly once after `pre_start` succeeds | `Service::start` control flow |
| SVC-002 | `on_stop` is always called if `on_start` returned Ok (even on panic/error in `run`) | `Service::start` uses `scopeguard`-like pattern |
| SVC-003 | All `TaskRegistry::spawn`'d tasks receive shutdown within the deadline; remainders are aborted | `TaskRegistry::join_all` |
| SVC-004 | `ShutdownToken::cancel` is idempotent — only the first reason is recorded | `RwLock` on reason |
| SVC-005 | `ServiceHandle` clones stay valid after the owning `Service` is dropped (they become no-op) | weak-refs pattern |
| SVC-006 | No lifecycle hook blocks longer than the crate's own timeout config before shutdown is forced | deadline tracking |

## Feature Flags

| Flag | Default | Effect |
|---|---|---|
| `tokio-console` | off | Adds console-subscriber wiring for `tokio-console` introspection |
| `structured-panic` | on | Installs a panic hook that routes panics through `tracing::error!` and triggers `Fatal` shutdown |

## Testing Strategy

- **Unit tests.** `Service::start` happy path, `pre_start` failure path, `on_start` failure path, `run` panic path, shutdown-deadline timeout path.
- **Property tests.** `proptest` over shutdown-ordering: any order of `request_shutdown` calls vs task completion produces a consistent `ExitStatus`.
- **Test harness.** A `TestService` implementing all three traits with configurable failure points, exposed at `dig-service::testing::TestService` under the `testing` feature.
- **Integration with tokio-console.** CI job runs the example binary under `tokio-console` for a brief window and asserts no leaked tasks.

## File Layout

```
dig-service/
├── Cargo.toml
├── README.md
├── docs/
│   └── resources/
│       └── SPEC.md          ← this document
├── src/
│   ├── lib.rs
│   ├── service.rs           ← Service<N, A, R> + start/stop orchestration
│   ├── traits.rs            ← NodeLifecycle, PeerApi, RpcApi
│   ├── tasks.rs             ← TaskRegistry, TaskSummary, TaskKind
│   ├── shutdown.rs          ← ShutdownToken, ShutdownReason, ExitReason
│   ├── handle.rs            ← ServiceHandle
│   ├── error.rs             ← ServiceError
│   └── testing.rs           ← TestService (feature = "testing")
└── tests/
    ├── happy_path.rs
    ├── shutdown_ordering.rs
    └── deadline_abort.rs
```

## Risks & Open Questions

1. **Async-trait overhead.** `async_trait::async_trait` introduces `Box<dyn Future>` per method call. Fine for lifecycle hooks (called once) but problematic for `PeerApi::on_message` and `RpcApi::dispatch` (hot paths). Decision: tolerate for v1; revisit with `async fn` in traits when MSRV hits 1.75+.
2. **ExitStatus granularity.** Should `ExitStatus` distinguish "run completed normally" from "shutdown requested"? Proposed: yes, and include the `ShutdownReason` so binaries can set process exit code accordingly.
3. **Reload semantics.** `ShutdownReason::ReloadRequested` is specified but this crate doesn't implement reload itself — it only signals. The implementing binary must handle it by exiting and re-spawning. Future: support in-place reload via `Service::reload(new_config)` with a `Reloadable` subtrait.
4. **TaskRegistry backpressure.** If a binary spawns 10k tracked tasks, the registry's book-keeping cost may become noticeable. Mitigation: keep only a `Weak<TaskInfo>` in the registry; the owning future holds the `Arc`. Snapshot takes a pass and drops the dead weak-refs.
5. **Signal handling placement.** The spec leaves signal handlers in `main.rs`. An alternative is a `dig-service::signals::install_default_handlers(handle)` helper. Defer to v1.1.

## Authoritative Sources

- [`apps/ARCHITECTURE.md`](../../../dig-network/apps/ARCHITECTURE.md) — §2 "Service composition", §4 "Task registry", §5 "Shutdown"
- [`apps/fullnode/SPEC.md`](../../../dig-network/apps/fullnode/SPEC.md) — `FullNode` node struct
- [`apps/validator/SPEC.md`](../../../dig-network/apps/validator/SPEC.md) — `Validator` node struct
- [`docs/resources/02-subsystems/08-binaries/supplement/03-startup-sequences.md`](../../../dig-network/docs/resources/02-subsystems/08-binaries/supplement/03-startup-sequences.md)
- [`docs/resources/03-appendices/10-crate-scope-refined.md`](../../../dig-network/docs/resources/03-appendices/10-crate-scope-refined.md) — rationale for this crate's scope
- [tokio-util CancellationToken](https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html) — upstream primitive
- [Chia `chia/server/start_service.py`](https://github.com/Chia-Network/chia-blockchain/blob/main/chia/server/start_service.py) — parallel of the composition pattern
