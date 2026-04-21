//! Shutdown-token + task-registry integration tests.

#![cfg(feature = "testing")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dig_service::{
    NodeLifecycle, RunContext, Service, ServiceError, ShutdownReason, ShutdownToken, StartContext,
    StopContext, TaskKind,
};

/// A node that spawns `N` background loops and waits for shutdown.
struct NoisyNode {
    loops: usize,
    started_count: Arc<AtomicUsize>,
}

#[async_trait]
impl NodeLifecycle for NoisyNode {
    const NAME: Option<&'static str> = Some("noisy");

    async fn pre_start(&self, _ctx: &StartContext<'_>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_start(&self, _ctx: &StartContext<'_>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run(&self, ctx: RunContext) -> anyhow::Result<()> {
        for i in 0..self.loops {
            let shutdown = ctx.shutdown.clone();
            let started = self.started_count.clone();
            ctx.tasks.spawn("bg", TaskKind::BackgroundLoop, async move {
                started.fetch_add(1, Ordering::SeqCst);
                shutdown.cancelled().await;
                // Simulate small per-task tear-down work.
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(())
            });
            // Let each spawned task actually start before moving on so the
            // `started_count` observation is meaningful.
            tokio::task::yield_now().await;
            let _ = i;
        }
        ctx.shutdown.cancelled().await;
        Ok(())
    }
    async fn on_stop(&self, _ctx: &StopContext<'_>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn post_stop(&self, _ctx: &StopContext<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}

/// **Proves:** a service with N background loops, when shut down, joins
/// them all within the deadline and exits gracefully.
///
/// **Why it matters:** Real binaries spawn dozens of background loops
/// (peer heartbeats, sync tick, metrics scrape). If `join_all` couldn't
/// wait for them cleanly, every orderly shutdown would surface as
/// `ShutdownDeadlineExceeded`.
///
/// **Catches:** a regression where `join_all` fails to wait for **some**
/// tasks (perhaps due to a draining-lock bug).
#[tokio::test]
async fn graceful_shutdown_joins_all_background_tasks() {
    let started = Arc::new(AtomicUsize::new(0));
    let node = NoisyNode {
        loops: 3,
        started_count: started.clone(),
    };
    let svc = Service::<_, (), ()>::new(node, (), ());
    let handle = svc.handle();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.request_shutdown(ShutdownReason::UserRequested);
    });

    let status = svc.start().await.expect("graceful exit");
    assert!(status.is_graceful());
    assert_eq!(started.load(Ordering::SeqCst), 3);
}

/// **Proves:** `ShutdownToken::reason` is readable from multiple clones
/// and reflects the first-call reason.
///
/// **Why it matters:** Monitoring subsystems (metrics, tracing) may capture
/// the shutdown reason after the fact. Both readers must see the same
/// value — clone-semantics for `ShutdownToken` share reason state.
///
/// **Catches:** a regression where `.clone()` creates independent reason
/// storage.
#[tokio::test]
async fn shutdown_reason_shared_across_clones() {
    let t = ShutdownToken::new();
    let a = t.clone();
    let b = t.clone();
    a.cancel(ShutdownReason::RpcRequested);
    assert!(matches!(b.reason(), Some(ShutdownReason::RpcRequested)));
    // Subsequent cancel on a different clone doesn't overwrite.
    b.cancel(ShutdownReason::UserRequested);
    assert!(matches!(t.reason(), Some(ShutdownReason::RpcRequested)));
}

/// **Proves:** a task that ignores the shutdown token past the deadline
/// produces [`ServiceError::ShutdownDeadlineExceeded`].
///
/// **Why it matters:** A stuck / buggy task must not hang shutdown
/// forever. The deadline + abort-on-deadline behaviour is what keeps
/// `Service::start` bounded.
///
/// **Catches:** a regression where `join_all`'s timeout is bypassed or
/// where aborted tasks aren't counted in the `pending` field.
#[tokio::test]
async fn stuck_task_surfaces_deadline_exceeded() {
    use dig_service::TaskRegistry;

    let token = ShutdownToken::new();
    let r = TaskRegistry::new(token.clone());
    r.spawn("stuck", TaskKind::BackgroundLoop, async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(())
    });

    // Fire shutdown but `stuck` ignores it.
    token.cancel(ShutdownReason::UserRequested);

    let err = r.join_all(Duration::from_millis(30)).await.unwrap_err();
    assert!(matches!(
        err,
        ServiceError::ShutdownDeadlineExceeded { pending: 1, .. }
    ));
}
