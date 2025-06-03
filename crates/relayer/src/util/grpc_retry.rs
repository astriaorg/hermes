use core::time::Duration;
use std::future::Future;

use tracing::{debug, warn};

use crate::error::Error;

/// Default retry configuration for gRPC calls
#[derive(Debug, Clone)]
pub struct GrpcRetryConfig {
    pub max_attempts: usize,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for GrpcRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
        }
    }
}

/// Retry a gRPC call with fixed delay
pub async fn retry_grpc_call<F, T, Fut>(
    config: &GrpcRetryConfig,
    operation_name: &str,
    operation: F,
) -> Result<T, Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    let result = tryhard::retry_fn(|| async {
        let result = operation().await;

        match &result {
            Ok(_) => {
                debug!(operation = operation_name, "gRPC call succeeded");
            }
            Err(e) => {
                warn!(
                    operation = operation_name,
                    error = %e,
                    "gRPC call failed, will retry"
                );
            }
        }

        result
    })
    .retries(config.max_attempts.saturating_sub(1) as u32)
    .await;

    result.map_err(|e| {
        Error::grpc_status(
            tonic::Status::unavailable(format!("gRPC retries exhausted: {}", e)),
            operation_name.to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_retry_success_on_first_attempt() {
        let config = GrpcRetryConfig::default();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        let result = retry_grpc_call(&config, "test_operation", || async {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            Ok::<i32, Error>(42)
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_success_after_failure() {
        let config = GrpcRetryConfig {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        };
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        let result = retry_grpc_call(&config, "test_operation", || async {
            let count = counter_clone.fetch_add(1, Ordering::SeqCst);
            if count < 2 {
                Err(Error::grpc_status(
                    tonic::Status::unavailable("connection timeout"),
                    "test".to_string(),
                ))
            } else {
                Ok::<i32, Error>(42)
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }
}
