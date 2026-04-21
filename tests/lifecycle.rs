//! Lifecycle tests — verifying hook order, error propagation, and the
//! shutdown-reason contract.

#![cfg(feature = "testing")]

use std::sync::Arc;

use async_trait::async_trait;
use dig_service::{
    testing::TestNode, ExitReason, NodeLifecycle, RunContext, Service, ServiceError,
    ShutdownReason, StartContext, StopContext,
};

/// A node that records which hooks ran, in order.
struct RecordingNode {
    log: Arc<parking_lot::Mutex<Vec<&'static str>>>,
    fail_at: Option<&'static str>,
}

#[async_trait]
impl NodeLifecycle for RecordingNode {
    const NAME: Option<&'static str> = Some("recording");

    async fn pre_start(&self, _ctx: &StartContext<'_>) -> anyhow::Result<()> {
        self.log.lock().push("pre_start");
        if self.fail_at == Some("pre_start") {
            anyhow::bail!("inject");
        }
        Ok(())
    }
    async fn on_start(&self, _ctx: &StartContext<'_>) -> anyhow::Result<()> {
        self.log.lock().push("on_start");
        if self.fail_at == Some("on_start") {
            anyhow::bail!("inject");
        }
        Ok(())
    }
    async fn run(&self, ctx: RunContext) -> anyhow::Result<()> {
        self.log.lock().push("run_enter");
        if self.fail_at == Some("run") {
            anyhow::bail!("inject");
        }
        ctx.shutdown.cancelled().await;
        self.log.lock().push("run_exit");
        Ok(())
    }
    async fn on_stop(&self, _ctx: &StopContext<'_>) -> anyhow::Result<()> {
        self.log.lock().push("on_stop");
        Ok(())
    }
    async fn post_stop(&self, _ctx: &StopContext<'_>) -> anyhow::Result<()> {
        self.log.lock().push("post_stop");
        Ok(())
    }
}

/// **Proves:** on the happy path, hooks run in the exact order
/// `pre_start → on_start → run_enter → run_exit → on_stop → post_stop`.
///
/// **Why it matters:** The whole point of `dig-service` is this ordering.
/// If hooks ever ran out of order (e.g., `on_start` before `pre_start`),
/// every downstream binary that relies on "stores opened in pre_start are
/// available in on_start" would blow up.
///
/// **Catches:** a refactor of `Service::start` that accidentally reorders
/// the hooks, or that skips a hook on the happy path.
#[tokio::test]
async fn happy_path_hook_order() {
    let log: Arc<parking_lot::Mutex<Vec<&'static str>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));

    let node = RecordingNode {
        log: log.clone(),
        fail_at: None,
    };
    let svc = Service::<_, (), ()>::new(node, (), ());
    let handle = svc.handle();

    // Fire shutdown after a tick so run exits cleanly.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.request_shutdown(ShutdownReason::UserRequested);
    });

    let status = svc.start().await.expect("start");
    let log = log.lock().clone();
    assert_eq!(
        log,
        vec![
            "pre_start",
            "on_start",
            "run_enter",
            "run_exit",
            "on_stop",
            "post_stop"
        ]
    );
    assert!(matches!(
        status.reason,
        ExitReason::RequestedShutdown(ShutdownReason::UserRequested)
    ));
}

/// **Proves:** a failure in `pre_start` surfaces as
/// [`ServiceError::PreStartFailed`] and **does not** call any later hook.
///
/// **Why it matters:** `pre_start` opens stores. If it fails (disk full,
/// permission denied), calling `on_start` next would try to bind ports
/// against half-opened stores and do strictly worse.
///
/// **Catches:** a regression where `Service::start` ignores the `pre_start`
/// error and runs `on_start` anyway.
#[tokio::test]
async fn pre_start_failure_short_circuits() {
    let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let node = RecordingNode {
        log: log.clone(),
        fail_at: Some("pre_start"),
    };
    let err = Service::<_, (), ()>::new(node, (), ())
        .start()
        .await
        .unwrap_err();

    assert!(matches!(err, ServiceError::PreStartFailed(_)));
    assert_eq!(log.lock().as_slice(), &["pre_start"]);
}

/// **Proves:** a failure in `run` still calls `on_stop` + `post_stop`.
///
/// **Why it matters:** `run` can fail anywhere — mid-event, panic-adjacent.
/// Stores opened in `pre_start` must still be flushed, which is what the
/// later hooks are for. Skipping them on `run`-error would corrupt state.
///
/// **Catches:** a regression that early-returns on `run` failure without
/// the cleanup tail.
#[tokio::test]
async fn run_failure_still_runs_cleanup() {
    let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let node = RecordingNode {
        log: log.clone(),
        fail_at: Some("run"),
    };
    let err = Service::<_, (), ()>::new(node, (), ())
        .start()
        .await
        .unwrap_err();

    assert!(matches!(err, ServiceError::RunFailed(_)));
    // All three early hooks ran (pre_start, on_start, run_enter),
    // and both cleanup hooks (on_stop, post_stop) ran.
    let log = log.lock().clone();
    assert!(log.contains(&"on_stop"), "on_stop missing: {log:?}");
    assert!(log.contains(&"post_stop"), "post_stop missing: {log:?}");
}

/// **Proves:** a `TestNode` with no failure injection exits gracefully when
/// the shutdown token fires.
///
/// **Why it matters:** Smoke test the `testing` feature's helper — dependent
/// crates use it extensively.
///
/// **Catches:** a regression in `TestNode::run` that ignores the shutdown
/// token.
#[tokio::test]
async fn test_node_graceful_exit() {
    let svc = Service::<_, (), ()>::new(TestNode::default(), (), ());
    let handle = svc.handle();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        handle.request_shutdown(ShutdownReason::UserRequested);
    });
    let status = svc.start().await.unwrap();
    assert!(status.is_graceful());
}
