//! Receiver side of the link speed test (PROTOCOL.md §6.9). Pure state; the
//! caller supplies timestamps taken when each message was fully read.

use std::time::{Duration, Instant};

use crate::protocol::{SPEED_TEST_ABORTED, SPEED_TEST_OK, SpeedTestResult, SpeedTestStart};

/// Grace period after `duration_ms` before an unfinished test is aborted.
pub const TIMEOUT_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct ActiveTest {
    test_id: u32,
    deadline: Instant,
    started: Instant,
    bytes: u64,
}

impl ActiveTest {
    fn result(&self, status: u32, now: Instant) -> SpeedTestResult {
        SpeedTestResult {
            test_id: self.test_id,
            status,
            bytes_received: self.bytes,
            elapsed_us: now.saturating_duration_since(self.started).as_micros() as u64,
        }
    }
}

#[derive(Debug, Default)]
pub struct SpeedTestReceiver {
    active: Option<ActiveTest>,
}

impl SpeedTestReceiver {
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Starts a test (the caller has already validated `start`). Returns the
    /// ABORTED result for a test that was still running, if any.
    pub fn start(&mut self, start: SpeedTestStart, now: Instant) -> Option<SpeedTestResult> {
        let aborted = self.active.take().map(|old| {
            log::warn!(
                "[speed] test {} aborted by new test {}",
                old.test_id,
                start.test_id
            );
            old.result(SPEED_TEST_ABORTED, now)
        });
        log::info!(
            "[speed] test {} started ({} ms)",
            start.test_id,
            start.duration_ms
        );
        self.active = Some(ActiveTest {
            test_id: start.test_id,
            deadline: now + Duration::from_millis(start.duration_ms.into()) + TIMEOUT_GRACE,
            started: now,
            bytes: 0,
        });
        aborted
    }

    /// Counts the filler of one SPEED_TEST_DATA. Returns false if ignored.
    pub fn data(&mut self, test_id: u32, filler_len: u64) -> bool {
        match &mut self.active {
            Some(t) if t.test_id == test_id => {
                t.bytes += filler_len;
                true
            }
            _ => {
                log::debug!("[speed] ignoring SPEED_TEST_DATA for inactive test {test_id}");
                false
            }
        }
    }

    /// Finishes the matching test. `None` if the END does not match.
    pub fn end(&mut self, test_id: u32, now: Instant) -> Option<SpeedTestResult> {
        match self.active.take() {
            Some(t) if t.test_id == test_id => {
                let result = t.result(SPEED_TEST_OK, now);
                log::info!(
                    "[speed] test {} done: {} bytes in {} us = {} Mbit/s",
                    test_id,
                    result.bytes_received,
                    result.elapsed_us,
                    result.kbit_per_s().unwrap_or(0) / 1000
                );
                Some(result)
            }
            other => {
                log::warn!("[speed] ignoring SPEED_TEST_END for inactive test {test_id}");
                self.active = other;
                None
            }
        }
    }

    /// Aborts the active test once `duration_ms` + 5 s have passed.
    pub fn poll_timeout(&mut self, now: Instant) -> Option<SpeedTestResult> {
        if self.active.as_ref().is_some_and(|t| now >= t.deadline) {
            let t = self.active.take().expect("checked above");
            log::warn!(
                "[speed] test {} timed out without SPEED_TEST_END",
                t.test_id
            );
            return Some(t.result(SPEED_TEST_ABORTED, now));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(test_id: u32, duration_ms: u32) -> SpeedTestStart {
        SpeedTestStart {
            test_id,
            duration_ms,
        }
    }

    #[test]
    fn counts_matching_data_and_reports_elapsed() {
        let t0 = Instant::now();
        let mut rx = SpeedTestReceiver::default();
        assert!(rx.start(start(1, 1000), t0).is_none());
        assert!(rx.data(1, 1000));
        assert!(rx.data(1, 24));
        assert!(!rx.data(2, 999));
        let r = rx.end(1, t0 + Duration::from_millis(250)).unwrap();
        assert_eq!(r.test_id, 1);
        assert_eq!(r.status, SPEED_TEST_OK);
        assert_eq!(r.bytes_received, 1024);
        assert_eq!(r.elapsed_us, 250_000);
        assert!(!rx.is_active());
    }

    #[test]
    fn data_and_end_without_active_test_are_ignored() {
        let mut rx = SpeedTestReceiver::default();
        assert!(!rx.data(1, 10));
        assert!(rx.end(1, Instant::now()).is_none());
    }

    #[test]
    fn end_with_other_id_keeps_the_active_test() {
        let t0 = Instant::now();
        let mut rx = SpeedTestReceiver::default();
        rx.start(start(3, 500), t0);
        assert!(rx.end(4, t0).is_none());
        assert!(rx.is_active());
        assert_eq!(rx.end(3, t0).unwrap().test_id, 3);
    }

    #[test]
    fn new_start_aborts_running_test() {
        let t0 = Instant::now();
        let mut rx = SpeedTestReceiver::default();
        rx.start(start(1, 1000), t0);
        rx.data(1, 77);
        let aborted = rx
            .start(start(2, 1000), t0 + Duration::from_millis(10))
            .unwrap();
        assert_eq!(aborted.test_id, 1);
        assert_eq!(aborted.status, SPEED_TEST_ABORTED);
        assert_eq!(aborted.bytes_received, 77);
        assert!(!rx.data(1, 5), "old test id no longer counts");
        assert!(rx.data(2, 5));
    }

    #[test]
    fn times_out_after_duration_plus_grace() {
        let t0 = Instant::now();
        let mut rx = SpeedTestReceiver::default();
        rx.start(start(9, 100), t0);
        assert!(rx.poll_timeout(t0 + Duration::from_millis(5099)).is_none());
        let r = rx.poll_timeout(t0 + Duration::from_millis(5100)).unwrap();
        assert_eq!((r.test_id, r.status), (9, SPEED_TEST_ABORTED));
        assert!(!rx.is_active());
        assert!(rx.poll_timeout(t0 + Duration::from_secs(60)).is_none());
    }
}
