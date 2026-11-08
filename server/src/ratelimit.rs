//! In-memory sliding-window rate limits: failed logins, sign-ups and
//! enrollment attempts. They reset when the server restarts, which is fine
//! for limits measured in minutes.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    window: Duration,
    events: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    pub fn new(window: Duration) -> Self {
        RateLimiter {
            window,
            events: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, VecDeque<Instant>>> {
        self.events.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `Err(retry_after)` if `key` already has `limit` events in the window.
    pub fn check(&self, key: &str, limit: usize) -> Result<(), Duration> {
        self.check_at(key, limit, Instant::now())
    }

    pub fn record(&self, key: &str) {
        self.record_at(key, Instant::now());
    }

    /// Counts this attempt whether or not it's allowed: for limits on
    /// attempts rather than failures.
    pub fn attempt(&self, key: &str, limit: usize) -> Result<(), Duration> {
        let now = Instant::now();
        self.check_at(key, limit, now)?;
        self.record_at(key, now);
        Ok(())
    }

    pub fn clear(&self, key: &str) {
        self.lock().remove(key);
    }

    fn check_at(&self, key: &str, limit: usize, now: Instant) -> Result<(), Duration> {
        let mut events = self.lock();
        let Some(queue) = events.get_mut(key) else {
            return Ok(());
        };
        trim(queue, now, self.window);
        if queue.len() < limit {
            return Ok(());
        }
        // Allowed again once the oldest event that counts leaves the window.
        let oldest = queue[queue.len() - limit];
        Err((oldest + self.window)
            .saturating_duration_since(now)
            .max(Duration::from_secs(1)))
    }

    fn record_at(&self, key: &str, now: Instant) {
        let mut events = self.lock();
        let queue = events.entry(key.to_string()).or_default();
        trim(queue, now, self.window);
        queue.push_back(now);
    }

    /// Drops keys with nothing left in the window, so the map doesn't grow
    /// with every address that ever failed once.
    pub fn prune(&self) {
        let now = Instant::now();
        self.lock().retain(|_, queue| {
            trim(queue, now, self.window);
            !queue.is_empty()
        });
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().len()
    }
}

fn trim(queue: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while queue
        .front()
        .is_some_and(|&t| now.saturating_duration_since(t) >= window)
    {
        queue.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_the_limit_then_reports_when_to_retry() {
        let limiter = RateLimiter::new(Duration::from_secs(900));
        let t0 = Instant::now();
        for i in 0..5 {
            assert!(limiter.check_at("k", 5, t0 + Duration::from_secs(i)).is_ok());
            limiter.record_at("k", t0 + Duration::from_secs(i));
        }
        let wait = limiter.check_at("k", 5, t0 + Duration::from_secs(10)).unwrap_err();
        assert_eq!(wait, Duration::from_secs(890));
        assert!(limiter.check_at("other", 5, t0).is_ok(), "keys are independent");
        // The first event leaves the window.
        assert!(limiter.check_at("k", 5, t0 + Duration::from_secs(900)).is_ok());
        limiter.clear("k");
        assert!(limiter.check_at("k", 1, t0).is_ok());
    }

    #[test]
    fn prune_forgets_quiet_keys() {
        let limiter = RateLimiter::new(Duration::from_millis(1));
        limiter.record("a");
        std::thread::sleep(Duration::from_millis(5));
        limiter.prune();
        assert_eq!(limiter.len(), 0);
    }
}
