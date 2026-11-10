// SPDX-License-Identifier: Apache-2.0
//! Polling deadlines independent of how often IPC wakes the daemon.

/// A fixed deadline: unrelated IPC must not restart the device polling period.
pub struct PumpDeadline {
    next: u64,
    period_ms: u64,
}

impl PumpDeadline {
    pub fn new(now_ms: u64, period_ms: u64) -> Self {
        Self {
            next: now_ms,
            period_ms: period_ms.max(1),
        }
    }

    pub fn due(&self, now_ms: u64) -> bool {
        now_ms >= self.next
    }

    /// Schedule from completion, avoiding a burst of catch-up work after a slow HAL operation.
    pub fn completed(&mut self, now_ms: u64) {
        self.next = now_ms.saturating_add(self.period_ms);
    }

    pub fn wait_us(&self, now_ms: u64) -> i32 {
        self.next
            .saturating_sub(now_ms)
            .saturating_mul(1000)
            .min(i32::MAX as u64) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_flood_cannot_postpone_device_work() {
        let mut d = PumpDeadline::new(0, 10);
        d.completed(0);
        for now in 1..10 {
            assert!(!d.due(now));
            assert_eq!(d.wait_us(now), ((10 - now) * 1000) as i32);
        }
        assert!(d.due(10));
        assert_eq!(d.wait_us(11), 0);
        d.completed(11);
        assert!(!d.due(20));
        assert!(d.due(21));
    }

    #[test]
    fn delayed_work_does_not_trigger_catchup_storm() {
        let mut d = PumpDeadline::new(0, 10);
        assert!(d.due(1000));
        d.completed(1000);
        assert!(!d.due(1000));
        assert_eq!(d.wait_us(1000), 10000);
    }
}
