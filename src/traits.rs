//! The three lifecycle / API traits DIG binaries implement to plug into
//! [`crate::Service`].
//!
//! - [`NodeLifecycle`]: the business-logic core. Required for every service.
//! - [`PeerApi`]: the peer-protocol dispatcher. Optional — `()` impls no-op.
//! - [`RpcApi`]: the JSON-RPC dispatcher. Optional — `()` impls no-op.
//!
//! All three are trait objects — consumers can plug arbitrary concrete
//! types. We use `async_trait::async_trait` for v0.1 so the returned
//! futures are `Box<dyn Future>`; this is tolerable at the lifecycle layer
//! (one call per hook) but will migrate to `async fn` in traits when MSRV
//! reaches 1.75+.

use async_trait::async_trait;

use crate::shutdown::{ExitReason, ShutdownToken};
use crate::tasks::TaskRegistry;

/// A stable peer identifier — typically `SHA256(remote cert pubkey)`.
pub type PeerId = [u8; 32];

/// Peer metadata populated at connect time.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// Peer identifier.
    pub peer_id: PeerId,
    /// Remote `"ip:port"` string.
    pub remote_addr: String,
    /// Peer-declared node type (for debug / logs).
    pub node_type: String,
}

/// Why a peer disconnected.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum DisconnectReason {
    /// Peer closed gracefully.
    Graceful,
    /// Local side closed (shutdown, eviction).
    LocalClose,
    /// Transport error.
    Transport(String),
    /// Protocol violation — peer was banned.
    ProtocolViolation(String),
}

/// A raw inbound peer-protocol message.
///
/// The shape is intentionally opaque at this layer; concrete binaries
/// decode opcodes via `dig-protocol` and route to typed handlers.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// The opcode byte / u8.
    pub opcode: u8,
    /// Raw payload bytes.
    pub payload: Vec<u8>,
}

/// Context passed to `pre_start` / `on_start`.
pub struct StartContext<'a> {
    /// Service name (equal to `N::NAME`).
    pub name: &'static str,
    /// Shutdown token. Capture `.clone()` into background tasks.
    pub shutdown: ShutdownToken,
    /// Task registry. Spawn background loops here so they join cleanly.
    pub tasks: &'a TaskRegistry,
}

/// Context passed to `run`. Differs from `StartContext` only in that it
/// owns a `TaskRegistry` (spawning from inside `run` is the common case).
pub struct RunContext {
    /// Shutdown token. Await `.cancelled()` to notice shutdown.
    pub shutdown: ShutdownToken,
    /// Task registry.
    pub tasks: TaskRegistry,
}

/// Context passed to `on_stop` / `post_stop`.
pub struct StopContext<'a> {
    /// Shutdown token.
    pub shutdown: ShutdownToken,
    /// Task registry (for inspection).
    pub tasks: &'a TaskRegistry,
    /// The reason the service is exiting.
    pub exit_reason: ExitReason,
}

/// The business-logic core of a service.
///
/// Implementors own all in-memory state (stores, pools, caches) and expose
/// it through the five lifecycle hooks. See crate-level docs for ordering.
///
/// # Associated constants
///
/// - `NAME` — an optional static identifier surfaced via `ServiceHandle::name`
///   and used in tracing spans. Set to `Some("validator")` etc.
#[async_trait]
pub trait NodeLifecycle: Send + Sync + 'static {
    /// Optional static name. Used in logs; may be `None`.
    const NAME: Option<&'static str> = None;

    /// Called before peer / RPC servers bind. Typical work:
    /// open stores, replay journal, restore state.
    async fn pre_start(&self, ctx: &StartContext<'_>) -> anyhow::Result<()>;

    /// Called after peer / RPC servers bind but before `run` begins. Typical
    /// work: announce capabilities, warm caches.
    async fn on_start(&self, ctx: &StartContext<'_>) -> anyhow::Result<()>;

    /// Main run loop. Returns on:
    /// - `ctx.shutdown.cancelled()` firing (graceful), OR
    /// - a fatal internal error (`Err`).
    async fn run(&self, ctx: RunContext) -> anyhow::Result<()>;

    /// Called after `run` returns. Typical work: flush stores,
    /// write snapshots. Errors here are reported but do not stop `post_stop`
    /// from also running.
    async fn on_stop(&self, ctx: &StopContext<'_>) -> anyhow::Result<()>;

    /// Final hook. Typical work: close stores, release file locks.
    async fn post_stop(&self, ctx: &StopContext<'_>) -> anyhow::Result<()>;
}

/// Peer-protocol dispatcher.
///
/// `()` is blanket-implemented to no-op every method; binaries that don't
/// serve a peer surface (daemon, wallet) instantiate
/// `Service<N, (), R>`.
#[async_trait]
pub trait PeerApi: Send + Sync + 'static {
    /// Called for every inbound message after handshake succeeds.
    async fn on_message(&self, _peer: PeerId, _msg: InboundMessage) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called when a peer connects (post-handshake).
    async fn on_peer_connected(&self, _peer: PeerId, _info: PeerInfo) {}

    /// Called when a peer disconnects.
    async fn on_peer_disconnected(&self, _peer: PeerId, _reason: DisconnectReason) {}
}

#[async_trait]
impl PeerApi for () {}

/// JSON-RPC dispatcher.
///
/// `()` is blanket-implemented to route every method to `MethodNotFound`;
/// binaries that don't serve RPC (introducer, relay) instantiate
/// `Service<N, A, ()>`.
#[async_trait]
pub trait RpcApi: Send + Sync + 'static {
    /// Called for every inbound RPC request. `method` is already parsed;
    /// `params` is the method-specific JSON payload.
    async fn dispatch(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, dig_rpc_types::envelope::JsonRpcError>;

    /// Optional health probe. Default: `Ok(())`.
    async fn healthz(&self) -> std::result::Result<(), dig_rpc_types::envelope::JsonRpcError> {
        Ok(())
    }
}

#[async_trait]
impl RpcApi for () {
    async fn dispatch(
        &self,
        method: &str,
        _params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, dig_rpc_types::envelope::JsonRpcError> {
        Err(dig_rpc_types::envelope::JsonRpcError {
            code: dig_rpc_types::errors::ErrorCode::MethodNotFound,
            message: format!("RpcApi not configured; method {method:?} rejected"),
            data: None,
        })
    }
}
