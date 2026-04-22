# dig-service

Generic `Service<N, A, R>` orchestration scaffold for every DIG Network binary. Lifecycle traits, task registry with shutdown-deadline enforcement, typed shutdown token, out-of-lifetime service handle. Nothing else — no config parsing, no CLI, no logging setup.

See [`docs/resources/SPEC.md`](docs/resources/SPEC.md) for the design doc.

---

## Table of contents

1. [Install](#install)
2. [Architecture](#architecture)
3. [Quick reference](#quick-reference)
4. [`Service<N, A, R>`](#servicen-a-r)
5. [`NodeLifecycle` trait](#nodelifecycle-trait)
6. [`PeerApi` trait](#peerapi-trait)
7. [`RpcApi` trait](#rpcapi-trait)
8. [Contexts](#contexts)
9. [`ShutdownToken`](#shutdowntoken)
10. [`ShutdownReason` / `ExitReason` / `ExitStatus`](#shutdownreason--exitreason--exitstatus)
11. [`TaskRegistry`](#taskregistry)
12. [`ServiceHandle`](#servicehandle)
13. [Errors](#errors)
14. [`testing` module](#testing-module-feature-testing)
15. [Feature flags](#feature-flags)
16. [License](#license)

---

## Install

```toml
[dependencies]
dig-service = "0.1"
```

Minimum Rust: 1.70.

---

## Architecture

```
  apps/fullnode   apps/validator   apps/introducer   apps/relay   apps/daemon
        │               │                 │              │             │
        └───────────────┴─────────┬───────┴──────────────┴─────────────┘
                                  ▼
                            dig-service
                                  │
                                  ▼
                           tokio + tokio-util
```

### Lifecycle

```
     Service::start
          │
          ▼
   node.pre_start    ◂── open stores, replay journal
          │
          ▼
   node.on_start     ◂── bind ports, warm caches
          │
          ▼
 ┌── node.run ──┐    ◂── main event loop; returns on shutdown
 │              │
 │ tasks.spawn  │
 └──────┬───────┘
        ▼
 tasks.join_all     ◂── graceful wait w/ deadline
        ▼
   node.on_stop    ◂── flush stores
        ▼
   node.post_stop  ◂── close stores, release locks
        ▼
     ExitStatus
```

---

## Quick reference

```rust,no_run
use async_trait::async_trait;
use dig_service::{NodeLifecycle, Service, StartContext, RunContext, StopContext};

struct Node;

#[async_trait]
impl NodeLifecycle for Node {
    const NAME: Option<&'static str> = Some("example");

    async fn pre_start(&self, _ctx: &StartContext<'_>) -> anyhow::Result<()> { Ok(()) }
    async fn on_start(&self, _ctx: &StartContext<'_>) -> anyhow::Result<()> { Ok(()) }
    async fn run(&self, ctx: RunContext) -> anyhow::Result<()> {
        ctx.shutdown.cancelled().await;
        Ok(())
    }
    async fn on_stop(&self, _ctx: &StopContext<'_>) -> anyhow::Result<()> { Ok(()) }
    async fn post_stop(&self, _ctx: &StopContext<'_>) -> anyhow::Result<()> { Ok(()) }
}

#[tokio::main]
async fn main() {
    let svc = Service::<Node, (), ()>::new(Node, (), ());
    let _exit = svc.start().await.unwrap();
}
```

---

## `Service<N, A, R>`

```rust,ignore
pub struct Service<N, A, R>
where
    N: NodeLifecycle,
    A: PeerApi,
    R: RpcApi,
{ /* … */ }
```

### Type parameters

| Param | Role | Use `()` when… |
|---|---|---|
| `N` | [`NodeLifecycle`](#nodelifecycle-trait) — business logic core | always required |
| `A` | [`PeerApi`](#peerapi-trait) — peer-protocol dispatcher | binary has no peer surface (daemon, wallet) |
| `R` | [`RpcApi`](#rpcapi-trait) — JSON-RPC dispatcher | binary has no RPC (introducer, relay) |

### Methods

| Method | Signature | Inputs / Output / Side effects |
|---|---|---|
| `new` | `(N, A, R) -> Self` | Build a service. **No I/O.** Ports are not bound, stores are not opened. |
| `start` | `self -> Result<ExitStatus>` | Drive the full lifecycle. **Consumes `self`** — single-shot. |
| `handle` | `&self -> ServiceHandle` | Clone-safe handle for signal handlers / RPC admin. Remains valid after `Service` drops. |
| `node` | `&self -> &Arc<N>` | Borrow the business core. |
| `peer_api` | `&self -> &Arc<A>` | Borrow the peer API adapter. |
| `rpc_api` | `&self -> &Arc<R>` | Borrow the RPC API adapter. |
| `shutdown_token` | `&self -> &ShutdownToken` | Borrow the broadcast cancellation token. |
| `tasks` | `&self -> &TaskRegistry` | Borrow the tracked-spawn registry. |
| `request_shutdown` | `&self, ShutdownReason` | Fire the shutdown token. |

### `start` contract

| Situation | Result |
|---|---|
| All hooks succeed, shutdown requested externally | `Ok(ExitStatus { reason: ExitReason::RequestedShutdown(...) })` |
| All hooks succeed, `run` returned `Ok(())` | `Ok(ExitStatus { reason: ExitReason::RunCompleted })` |
| `pre_start` errors | `Err(ServiceError::PreStartFailed)`; **no** later hook runs |
| `on_start` errors | `Err(ServiceError::OnStartFailed)`; `post_stop` runs (closes stores) |
| `run` errors | `Err(ServiceError::RunFailed)`; `on_stop` + `post_stop` still run |
| `on_stop` / `post_stop` errors | `Err(ServiceError::OnStopFailed)`; subsequent hooks still run |
| Task deadline exceeded | `Err(ServiceError::ShutdownDeadlineExceeded)` |
| `start` called twice | `Err(ServiceError::AlreadyRunning)` |

---

## `NodeLifecycle` trait

```rust,ignore
#[async_trait]
pub trait NodeLifecycle: Send + Sync + 'static {
    const NAME: Option<&'static str> = None;

    async fn pre_start(&self, ctx: &StartContext<'_>) -> anyhow::Result<()>;
    async fn on_start (&self, ctx: &StartContext<'_>) -> anyhow::Result<()>;
    async fn run      (&self, ctx: RunContext)        -> anyhow::Result<()>;
    async fn on_stop  (&self, ctx: &StopContext<'_>)  -> anyhow::Result<()>;
    async fn post_stop(&self, ctx: &StopContext<'_>)  -> anyhow::Result<()>;
}
```

| Hook | Invocation order | Typical work |
|---|---|---|
| `pre_start` | 1st | Open stores, replay journal, restore in-memory state |
| `on_start` | 2nd | Bind ports, warm caches, announce capabilities |
| `run` | 3rd (main) | Main event loop; returns on `ctx.shutdown.cancelled()` |
| `on_stop` | 4th | Flush stores, write snapshots |
| `post_stop` | 5th (always) | Close stores, release file locks |

**Associated constant** `NAME: Option<&'static str>` — optional static identifier surfaced via `ServiceHandle::name` and available in tracing spans. Default: `None`.

---

## `PeerApi` trait

```rust,ignore
#[async_trait]
pub trait PeerApi: Send + Sync + 'static {
    async fn on_message(&self, peer: PeerId, msg: InboundMessage) -> anyhow::Result<()> { Ok(()) }
    async fn on_peer_connected(&self, peer: PeerId, info: PeerInfo) {}
    async fn on_peer_disconnected(&self, peer: PeerId, reason: DisconnectReason) {}
}
```

Default methods are no-ops. `impl PeerApi for () {}` is provided — binaries with no peer surface use `Service<N, (), R>`.

### Wire types

| Type | Definition | Role |
|---|---|---|
| `PeerId` | `[u8; 32]` | Typically `SHA256(remote_cert_pubkey)` |
| `PeerInfo` | `{peer_id, remote_addr: String, node_type: String}` | Populated at connect |
| `InboundMessage` | `{opcode: u8, payload: Vec<u8>}` | Raw inbound bytes; binaries route via `dig-protocol` |
| `DisconnectReason` | `Graceful \| LocalClose \| Transport(String) \| ProtocolViolation(String)` | `#[non_exhaustive]` |

---

## `RpcApi` trait

```rust,ignore
#[async_trait]
pub trait RpcApi: Send + Sync + 'static {
    async fn dispatch(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, dig_rpc_types::envelope::JsonRpcError>;

    async fn healthz(&self) -> Result<(), dig_rpc_types::envelope::JsonRpcError> { Ok(()) }
}
```

Default `healthz` returns `Ok(())`. `impl RpcApi for ()` returns `MethodNotFound` for every method — binaries with no RPC use `Service<N, A, ()>`.

Uses `dig_rpc_types::envelope::JsonRpcError` as the error type so servers and clients share the wire shape.

---

## Contexts

| Type | Fields |
|---|---|
| `StartContext<'a>` | `name: &'static str`, `shutdown: ShutdownToken`, `tasks: &'a TaskRegistry` |
| `RunContext` | `shutdown: ShutdownToken`, `tasks: TaskRegistry` (owned — so `run` can spawn) |
| `StopContext<'a>` | `shutdown: ShutdownToken`, `tasks: &'a TaskRegistry`, `exit_reason: ExitReason` |

---

## `ShutdownToken`

Thin wrapper over `tokio_util::sync::CancellationToken` with a typed reason. Cancellation is broadcast: every clone sees the same flip.

| Method | Signature | Behaviour |
|---|---|---|
| `new` | `() -> Self` | Fresh, uncancelled |
| `is_cancelled` | `&self -> bool` | Non-blocking check |
| `cancelled` | `&self -> impl Future<Output = ()>` | Await the cancellation |
| `cancel` | `&self, ShutdownReason` | Fire the token; **first-wins** for reason |
| `reason` | `&self -> Option<ShutdownReason>` | The first reason recorded |
| `child_token` | `&self -> ShutdownToken` | Child that inherits cancellation (cancelling child doesn't affect parent) |

---

## `ShutdownReason` / `ExitReason` / `ExitStatus`

### `ShutdownReason`

| Variant | When | Typical process exit code |
|---|---|---|
| `UserRequested` | SIGINT / SIGTERM / Ctrl-C | 0 |
| `RpcRequested` | Admin RPC called `stop_node` | 0 |
| `ReloadRequested` | Reload signalled — binary should exit and re-spawn | 0 |
| `RequestedByRun` | `run` returned `Ok(())` without outside signal | 0 |
| `Fatal(String)` | Unrecoverable internal error | ≠0 |

### `ExitReason`

```rust,ignore
pub enum ExitReason {
    RequestedShutdown(ShutdownReason),
    RunCompleted,
    RunError(Arc<anyhow::Error>),
}
```

### `ExitStatus`

```rust,ignore
pub struct ExitStatus { pub reason: ExitReason }

impl ExitStatus {
    pub fn is_graceful(&self) -> bool;   // false iff RunError OR Fatal(_)
}
```

Binaries map `is_graceful()` → process exit code `0` vs `1`.

---

## `TaskRegistry`

Tracked-task spawning with shutdown-deadline enforcement. `Clone + Send + Sync`; all clones share state via `Arc`.

| Method | Signature | Purpose |
|---|---|---|
| `new` | `ShutdownToken -> Self` | Empty registry bound to a token |
| `spawn` | `&self, name: &'static str, kind: TaskKind, fut) -> AbortHandle` | Spawn a tracked task; returns `tokio::task::AbortHandle` |
| `shutdown` | `&self -> &ShutdownToken` | Borrow the linked token |
| `join_all` | `&self, deadline: Duration -> Result<()>` | Wait for tasks with deadline; abort laggards |
| `snapshot` | `&self -> Vec<TaskSummary>` | Inspect currently-registered tasks |
| `len` / `is_empty` | — | Registered task count |

### `TaskKind`

| Variant | Use for |
|---|---|
| `BackgroundLoop` | sync tick, heartbeat |
| `PeerConnection` | per-peer handler |
| `RpcHandler` | per-request handler |
| `Maintenance` | compaction, log rotation |

### `TaskSummary`

```rust,ignore
pub struct TaskSummary {
    pub name: &'static str,
    pub spawned_at: Instant,
    pub kind: TaskKind,
}
```

---

## `ServiceHandle`

Clone-safe reference into a running `Service`. Remains valid after the owning `Service` drops — operations become no-ops rather than panicking.

| Method | Signature | Purpose |
|---|---|---|
| `name` | `&self -> &'static str` | `NodeLifecycle::NAME` or `""` |
| `request_shutdown` | `&self, ShutdownReason` | Fire the shutdown token |
| `is_running` | `&self -> bool` | Whether shutdown has been cancelled |
| `shutdown_token` | `&self -> &ShutdownToken` | Borrow the token |
| `tasks` | `&self -> Vec<TaskSummary>` | Snapshot live tasks |

---

## Errors

```rust,ignore
pub enum ServiceError {
    PreStartFailed(Arc<anyhow::Error>),
    OnStartFailed(Arc<anyhow::Error>),
    RunFailed(Arc<anyhow::Error>),
    OnStopFailed(Arc<anyhow::Error>),
    ShutdownDeadlineExceeded { deadline: Duration, pending: usize },
    AlreadyRunning,
    AlreadyStopped,
}

pub type Result<T> = std::result::Result<T, ServiceError>;
```

All variants are `Clone` (errors wrapped in `Arc`) so they fan out through broadcast channels or async trait boundaries without forcing `!Sync` bounds.

---

## `testing` module (feature `testing`)

```rust,ignore
pub struct TestNode {
    pub fail_pre_start: bool,
    pub fail_on_start:  bool,
    pub fail_run:       bool,
    pub fail_on_stop:   bool,
    pub run_delay:      Duration,
}

impl Default for TestNode { /* all-false, zero-delay */ }
impl NodeLifecycle for TestNode { /* blocks in run until shutdown, unless fail_run is set */ }
```

Useful for dependent crates to exercise `Service` without writing their own node.

---

## Feature flags

| Flag | Default | Effect |
|---|---|---|
| `testing` | off | Ships `testing::TestNode` helper |
| `tokio-console` | off | (Planned) `console_subscriber` wiring |
| `structured-panic` | off | (Planned) Panic hook → `tracing::error!` → `Fatal` shutdown |

---

## License

Licensed under either of Apache-2.0 or MIT at your option.
