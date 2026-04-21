//! Service-level error types.
//!
//! Every fallible `Service` operation returns [`Result<T>`]
//! = `Result<T, ServiceError>`. Errors are `Clone` (wrapping `anyhow::Error`
//! in `Arc`) so they can fan out through broadcast channels or be bubbled
//! through async traits without forcing the caller to stop.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

/// Convenience alias: `Result<T, ServiceError>`.
pub type Result<T> = std::result::Result<T, ServiceError>;

/// All failure modes of a running `Service`.
#[derive(Error, Debug, Clone)]
pub enum ServiceError {
    /// The node's `pre_start` hook returned an error.
    ///
    /// No other lifecycle hooks are invoked after this — typical causes
    /// are store open failures or journal replay errors.
    #[error("pre_start failed: {0}")]
    PreStartFailed(#[source] Arc<anyhow::Error>),

    /// The node's `on_start` hook returned an error.
    ///
    /// `post_stop` is still invoked so resources opened in `pre_start`
    /// have a chance to close.
    #[error("on_start failed: {0}")]
    OnStartFailed(#[source] Arc<anyhow::Error>),

    /// The node's `run` hook returned an error.
    ///
    /// `on_stop` + `post_stop` are still invoked. The exit status's
    /// `reason` is `ExitReason::RunError`.
    #[error("run exited with error: {0}")]
    RunFailed(#[source] Arc<anyhow::Error>),

    /// The node's `on_stop` OR `post_stop` hook returned an error.
    ///
    /// Reported after `run` has returned; usually indicates a flush /
    /// close failure.
    #[error("on_stop failed: {0}")]
    OnStopFailed(#[source] Arc<anyhow::Error>),

    /// One or more tracked tasks did not exit within the shutdown deadline.
    ///
    /// The service aborts the laggards; this error is reported so operators
    /// can see that shutdown was not fully graceful.
    #[error("task registry deadline exceeded ({deadline:?}); {pending} tasks aborted")]
    ShutdownDeadlineExceeded {
        /// The deadline that was exceeded.
        deadline: Duration,
        /// How many tasks were still alive when the deadline fired.
        pending: usize,
    },

    /// `start()` was called on a Service that has already run.
    ///
    /// A `Service` is single-shot — once started, build a new one.
    #[error("service is already running")]
    AlreadyRunning,

    /// `start()` was called on a Service that has already stopped.
    ///
    /// Reserved for future re-entry protections; currently unreachable.
    #[error("service has already stopped")]
    AlreadyStopped,
}
