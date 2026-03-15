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
        let wait = match retry_after {
            Some(wait) => {
                state.next_backoff = self.base_backoff;
                wait
            }
            None => {
                let wait = state.next_backoff;
                state.next_backoff = state.next_backoff.checked_mul(2).unwrap_or(Duration::MAX);
                wait
            }
        };
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
        if let Some(blocked_until) = state.blocked_until
            && blocked_until <= tokio::time::Instant::now()
        {
            state.blocked_until = None;
        }
    }
}
