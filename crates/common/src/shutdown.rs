/// Graceful shutdown support for long-running workers
///
/// Provides signal handling for SIGTERM, SIGINT, and SIGHUP to enable
/// graceful shutdowns during deployments and scaling operations.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::info;

/// Shared shutdown flag that can be checked in worker loops
#[derive(Clone)]
pub struct ShutdownSignal {
    flag: Arc<AtomicBool>,
}

impl ShutdownSignal {
    /// Create a new shutdown signal
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Check if shutdown has been requested
    pub fn is_shutdown_requested(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Trigger shutdown (used internally by signal handler)
    fn trigger(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Install signal handlers and return a ShutdownSignal
    ///
    /// This spawns a background task that listens for OS signals (SIGTERM, SIGINT)
    /// and sets the shutdown flag when received.
    ///
    /// # Example
    /// ```no_run
    /// # use common::ShutdownSignal;
    /// # #[tokio::main]
    /// # async fn main() {
    /// let shutdown = ShutdownSignal::install();
    ///
    /// loop {
    ///     if shutdown.is_shutdown_requested() {
    ///         println!("Shutting down gracefully...");
    ///         break;
    ///     }
    ///     // Do work...
    /// }
    /// # }
    /// ```
    pub fn install() -> Self {
        let signal = Self::new();
        let signal_clone = signal.clone();

        tokio::spawn(async move {
            if let Err(e) = wait_for_shutdown_signal().await {
                crate::log_error!(warn, e, "failed to install signal handler");
            } else {
                info!("shutdown signal received");
                signal_clone.trigger();
            }
        });

        signal
    }
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Wait for a shutdown signal (SIGTERM or SIGINT)
async fn wait_for_shutdown_signal() -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;

        tokio::select! {
            _ = sigterm.recv() => {
                info!("received SIGTERM");
            }
            _ = sigint.recv() => {
                info!("received SIGINT");
            }
        }
    }

    #[cfg(not(unix))]
    {
        // Windows: only CTRL+C is supported
        tokio::signal::ctrl_c().await?;
        info!("received CTRL+C");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shutdown_signal_default_not_signaled() {
        let signal = ShutdownSignal::new();
        assert!(!signal.is_shutdown_requested());
    }

    #[test]
    fn test_shutdown_signal_trigger() {
        let signal = ShutdownSignal::new();
        signal.trigger();
        assert!(signal.is_shutdown_requested());
    }

    #[test]
    fn test_shutdown_signal_clone() {
        let signal = ShutdownSignal::new();
        let cloned = signal.clone();

        signal.trigger();
        assert!(cloned.is_shutdown_requested());
    }
}
