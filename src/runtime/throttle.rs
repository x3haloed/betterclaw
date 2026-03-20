use std::time::Duration;

#[derive(Debug)]
pub(crate) struct ProviderThrottle {
    pub(crate) base_backoff: Duration,
    pub(crate) state: tokio::sync::Mutex<ProviderThrottleState>,
}

#[derive(Debug)]
pub(crate) struct ProviderThrottleState {
    pub(crate) blocked_until: Option<tokio::time::Instant>,
    pub(crate) next_backoff: Duration,
}

impl ProviderThrottle {
    pub(crate) fn new(base_backoff: Duration) -> Self {
        Self {
            base_backoff,
            state: tokio::sync::Mutex::new(ProviderThrottleState {
                blocked_until: None,
                next_backoff: base_backoff,
            }),
        }
    }

    pub(crate) async fn current_wait(&self) -> Option<Duration> {
        let mut state = self.state.lock().await;
        let Some(blocked_until) = state.blocked_until else {
            return None;
        };
        let now = tokio::time::Instant::now();
        if blocked_until <= now {
            state.blocked_until = None;
            return None;
        }
        Some(blocked_until.duration_since(now))
    }

    pub(crate) async fn arm(&self, retry_after: Option<Duration>) -> Duration {
        let mut state = self.state.lock().await;
        let wait = retry_after
            .unwrap_or(state.next_backoff)
            .max(state.next_backoff);
        state.next_backoff = wait.checked_mul(2).unwrap_or(Duration::MAX);
        let now = tokio::time::Instant::now();
        let candidate = now + wait;
        state.blocked_until = Some(match state.blocked_until {
            Some(existing) if existing > candidate => existing,
            _ => candidate,
        });
        state
            .blocked_until
            .map(|deadline| deadline.duration_since(now))
            .unwrap_or(wait)
    }

    pub(crate) async fn note_success(&self) {
        let mut state = self.state.lock().await;
        state.next_backoff = self.base_backoff;
        state.blocked_until = None;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::ProviderThrottle;

    #[tokio::test]
    async fn repeated_retry_after_windows_escalate_backoff_floor() {
        let throttle = ProviderThrottle::new(Duration::from_secs(1));

        assert_eq!(
            throttle.arm(Some(Duration::from_secs(1))).await,
            Duration::from_secs(1)
        );
        {
            let mut state = throttle.state.lock().await;
            assert_eq!(state.next_backoff, Duration::from_secs(2));
            state.blocked_until = None;
        }

        assert_eq!(
            throttle.arm(Some(Duration::from_secs(1))).await,
            Duration::from_secs(2)
        );
        {
            let mut state = throttle.state.lock().await;
            assert_eq!(state.next_backoff, Duration::from_secs(4));
            state.blocked_until = None;
        }

        assert_eq!(
            throttle.arm(Some(Duration::from_secs(1))).await,
            Duration::from_secs(4)
        );
    }

    #[tokio::test]
    async fn success_resets_backoff_floor() {
        let throttle = ProviderThrottle::new(Duration::from_secs(1));

        assert_eq!(
            throttle.arm(Some(Duration::from_secs(2))).await,
            Duration::from_secs(2)
        );
        throttle.note_success().await;

        assert_eq!(
            throttle.arm(Some(Duration::from_secs(1))).await,
            Duration::from_secs(1)
        );
    }
}
