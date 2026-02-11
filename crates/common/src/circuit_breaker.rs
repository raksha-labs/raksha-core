/// Circuit breaker for fault-tolerant external API calls
///
/// Implements the circuit breaker pattern to prevent cascading failures
/// when external services (RPC providers, APIs) become unavailable.
///
/// States:
/// - Closed: Normal operation, requests pass through
/// - Open: Too many failures, requests fail fast
/// - Half-Open: Testing if service recovered
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

const STATE_CLOSED: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_HALF_OPEN: u8 = 2;

/// Configuration for circuit breaker behavior
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of failures before opening the circuit
    pub failure_threshold: u64,
    /// Time window for counting failures (seconds)
    pub failure_window: Duration,
    /// How long to wait before attempting recovery (seconds)
    pub timeout: Duration,
    /// Number of successful calls needed to close circuit from half-open
    pub success_threshold: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            failure_window: Duration::from_secs(60),
            timeout: Duration::from_secs(30),
            success_threshold: 2,
        }
    }
}

/// Circuit breaker for protecting external service calls
#[derive(Clone)]
pub struct CircuitBreaker {
    name: Arc<String>,
    state: Arc<AtomicU8>,
    failure_count: Arc<AtomicU64>,
    success_count: Arc<AtomicU64>,
    last_failure_time: Arc<RwLock<Option<Instant>>>,
    last_transition: Arc<RwLock<Instant>>,
    config: Arc<CircuitBreakerConfig>,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given name and configuration
    pub fn new(name: impl Into<String>, config: CircuitBreakerConfig) -> Self {
        Self {
            name: Arc::new(name.into()),
            state: Arc::new(AtomicU8::new(STATE_CLOSED)),
            failure_count: Arc::new(AtomicU64::new(0)),
            success_count: Arc::new(AtomicU64::new(0)),
            last_failure_time: Arc::new(RwLock::new(None)),
            last_transition: Arc::new(RwLock::new(Instant::now())),
            config: Arc::new(config),
        }
    }

    /// Create a new circuit breaker with default configuration
    pub fn with_defaults(name: impl Into<String>) -> Self {
        Self::new(name, CircuitBreakerConfig::default())
    }

    /// Execute a fallible operation with circuit breaker protection
    ///
    /// # Example
    /// ```no_run
    /// # use common::CircuitBreaker;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let breaker = CircuitBreaker::with_defaults("eth-rpc");
    ///
    /// let result = breaker.call(|| async {
    ///     // Call external API
    ///     Ok::<_, anyhow::Error>("data")
    /// }).await;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn call<F, Fut, T, E>(&self, f: F) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        // Check if circuit is open
        if !self.can_attempt().await {
            return Err(CircuitBreakerError::Open {
                name: self.name.as_ref().clone(),
            });
        }

        // Execute the operation
        match f().await {
            Ok(result) => {
                self.on_success().await;
                Ok(result)
            }
            Err(e) => {
                self.on_failure().await;
                Err(CircuitBreakerError::CallFailed(e))
            }
        }
    }

    /// Check if a call attempt is allowed
    async fn can_attempt(&self) -> bool {
        let state = self.state.load(Ordering::Relaxed);

        match state {
            STATE_CLOSED => true,
            STATE_OPEN => {
                // Check if timeout has elapsed
                let last_transition = self.last_transition.read().await;
                if last_transition.elapsed() >= self.config.timeout {
                    drop(last_transition);
                    self.transition_to_half_open().await;
                    true
                } else {
                    false
                }
            }
            STATE_HALF_OPEN => true,
            _ => false,
        }
    }

    /// Record a successful call
    async fn on_success(&self) {
        let state = self.state.load(Ordering::Relaxed);

        match state {
            STATE_HALF_OPEN => {
                let count = self.success_count.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= self.config.success_threshold {
                    self.transition_to_closed().await;
                }
            }
            STATE_CLOSED => {
                // Reset failure count on success in closed state
                self.failure_count.store(0, Ordering::Relaxed);
                *self.last_failure_time.write().await = None;
            }
            _ => {}
        }
    }

    /// Record a failed call
    async fn on_failure(&self) {
        *self.last_failure_time.write().await = Some(Instant::now());

        let state = self.state.load(Ordering::Relaxed);

        match state {
            STATE_CLOSED => {
                let count = self.failure_count.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= self.config.failure_threshold {
                    self.transition_to_open().await;
                }
            }
            STATE_HALF_OPEN => {
                // Any failure in half-open immediately reopens the circuit
                self.transition_to_open().await;
            }
            _ => {}
        }
    }

    async fn transition_to_closed(&self) {
        self.state.store(STATE_CLOSED, Ordering::Relaxed);
        self.failure_count.store(0, Ordering::Relaxed);
        self.success_count.store(0, Ordering::Relaxed);
        *self.last_transition.write().await = Instant::now();
        info!(
            circuit_breaker = %self.name,
            "circuit breaker closed (service recovered)"
        );
    }

    async fn transition_to_open(&self) {
        self.state.store(STATE_OPEN, Ordering::Relaxed);
        self.success_count.store(0, Ordering::Relaxed);
        *self.last_transition.write().await = Instant::now();
        warn!(
            circuit_breaker = %self.name,
            failure_threshold = self.config.failure_threshold,
            timeout_secs = self.config.timeout.as_secs(),
            "circuit breaker opened (service unavailable)"
        );
    }

    async fn transition_to_half_open(&self) {
        self.state.store(STATE_HALF_OPEN, Ordering::Relaxed);
        self.failure_count.store(0, Ordering::Relaxed);
        self.success_count.store(0, Ordering::Relaxed);
        *self.last_transition.write().await = Instant::now();
        info!(
            circuit_breaker = %self.name,
            "circuit breaker half-open (testing recovery)"
        );
    }

    /// Get current circuit breaker state (for monitoring)
    pub fn get_state(&self) -> CircuitBreakerState {
        match self.state.load(Ordering::Relaxed) {
            STATE_CLOSED => CircuitBreakerState::Closed,
            STATE_OPEN => CircuitBreakerState::Open,
            STATE_HALF_OPEN => CircuitBreakerState::HalfOpen,
            _ => CircuitBreakerState::Closed,
        }
    }

    /// Get metrics for monitoring
    pub fn get_metrics(&self) -> CircuitBreakerMetrics {
        CircuitBreakerMetrics {
            name: self.name.as_ref().clone(),
            state: self.get_state(),
            failure_count: self.failure_count.load(Ordering::Relaxed),
            success_count: self.success_count.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitBreakerState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone)]
pub struct CircuitBreakerMetrics {
    pub name: String,
    pub state: CircuitBreakerState,
    pub failure_count: u64,
    pub success_count: u64,
}

#[derive(Debug)]
pub enum CircuitBreakerError<E> {
    Open { name: String },
    CallFailed(E),
}

impl<E: std::fmt::Display> std::fmt::Display for CircuitBreakerError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open { name } => write!(f, "circuit breaker '{}' is open", name),
            Self::CallFailed(e) => write!(f, "call failed: {}", e),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for CircuitBreakerError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CallFailed(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_breaker_opens_after_threshold() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            failure_window: Duration::from_secs(60),
            timeout: Duration::from_secs(1),
            success_threshold: 2,
        };
        let breaker = CircuitBreaker::new("test", config);

        // Simulate failures
        for _ in 0..3 {
            let _ = breaker
                .call(|| async { Err::<(), _>("error") })
                .await;
        }

        // Circuit should be open
        assert_eq!(breaker.get_state(), CircuitBreakerState::Open);
    }

    #[tokio::test]
    async fn test_circuit_breaker_recovers() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            failure_window: Duration::from_secs(60),
            timeout: Duration::from_millis(100),
            success_threshold: 2,
        };
        let breaker = CircuitBreaker::new("test", config);

        // Open the circuit
        for _ in 0..2 {
            let _ = breaker
                .call(|| async { Err::<(), _>("error") })
                .await;
        }

        assert_eq!(breaker.get_state(), CircuitBreakerState::Open);

        // Wait for timeout
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Successful calls should close the circuit
        for _ in 0..2 {
            let _ = breaker
                .call(|| async { Ok::<_, ()>("success") })
                .await;
        }

        assert_eq!(breaker.get_state(), CircuitBreakerState::Closed);
    }
}
