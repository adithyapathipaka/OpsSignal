//! Counts fire-and-forget deliveries that have not finished, so a
//! client can wait for them before the process exits.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct InFlight {
    count: Mutex<usize>,
    idle: Condvar,
}

/// Held by one delivery task. Dropping it, including when the task
/// panics or the runtime cancels it, marks the delivery finished.
pub(crate) struct Guard(Arc<InFlight>);

impl InFlight {
    pub(crate) fn start(self: &Arc<Self>) -> Guard {
        *self.count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        Guard(Arc::clone(self))
    }

    /// Blocks until nothing is in flight or `timeout` passes, and
    /// returns how many deliveries are still unfinished.
    pub(crate) fn wait(&self, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        while *count > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            count = self
                .idle
                .wait_timeout(count, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *count
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut count = self.0.count.lock().unwrap_or_else(|e| e.into_inner());
        *count -= 1;
        if *count == 0 {
            self.0.idle.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_returns_at_once_when_idle() {
        let in_flight = Arc::new(InFlight::default());
        assert_eq!(in_flight.wait(Duration::from_secs(5)), 0);
    }

    #[test]
    fn wait_times_out_with_the_unfinished_count() {
        let in_flight = Arc::new(InFlight::default());
        let _a = in_flight.start();
        let _b = in_flight.start();
        assert_eq!(in_flight.wait(Duration::from_millis(20)), 2);
    }

    #[test]
    fn wait_wakes_when_the_last_guard_drops() {
        let in_flight = Arc::new(InFlight::default());
        let guard = in_flight.start();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            drop(guard);
        });
        assert_eq!(in_flight.wait(Duration::from_secs(5)), 0);
        worker.join().expect("worker");
    }
}
