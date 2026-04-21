//! Testing helpers. Only compiled under the `testing` feature.
//!
//! Exports [`TestNode`] — a minimal `NodeLifecycle` impl whose `run` method
//! simply awaits shutdown. Useful in integration tests for dependent
//! crates that want to exercise the `Service` lifecycle without writing
//! their own node.

use async_trait::async_trait;

use crate::traits::{NodeLifecycle, RunContext, StartContext, StopContext};

/// A minimal [`NodeLifecycle`] whose `run` blocks until shutdown.
///
/// Construct with [`TestNode::default()`] or set individual failure
/// points via the builder methods.
pub struct TestNode {
    /// If `true`, `pre_start` returns an error.
    pub fail_pre_start: bool,
    /// If `true`, `on_start` returns an error.
    pub fail_on_start: bool,
    /// If `true`, `run` returns an error instead of waiting for shutdown.
    pub fail_run: bool,
    /// If `true`, `on_stop` returns an error.
    pub fail_on_stop: bool,
    /// Optional delay inside `run` before returning.
    pub run_delay: std::time::Duration,
}

impl Default for TestNode {
    fn default() -> Self {
        Self {
            fail_pre_start: false,
            fail_on_start: false,
            fail_run: false,
            fail_on_stop: false,
            run_delay: std::time::Duration::ZERO,
        }
    }
}

#[async_trait]
impl NodeLifecycle for TestNode {
    const NAME: Option<&'static str> = Some("test");

    async fn pre_start(&self, _ctx: &StartContext<'_>) -> anyhow::Result<()> {
        if self.fail_pre_start {
            anyhow::bail!("pre_start failure injected");
        }
        Ok(())
    }

    async fn on_start(&self, _ctx: &StartContext<'_>) -> anyhow::Result<()> {
        if self.fail_on_start {
            anyhow::bail!("on_start failure injected");
        }
        Ok(())
    }

    async fn run(&self, ctx: RunContext) -> anyhow::Result<()> {
        if self.fail_run {
            anyhow::bail!("run failure injected");
        }
        tokio::select! {
            _ = ctx.shutdown.cancelled() => Ok(()),
            _ = tokio::time::sleep(self.run_delay), if !self.run_delay.is_zero() => Ok(()),
        }
    }

    async fn on_stop(&self, _ctx: &StopContext<'_>) -> anyhow::Result<()> {
        if self.fail_on_stop {
            anyhow::bail!("on_stop failure injected");
        }
        Ok(())
    }

    async fn post_stop(&self, _ctx: &StopContext<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}
