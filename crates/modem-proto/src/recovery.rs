// SPDX-License-Identifier: Apache-2.0
//! Decisions shared by MODEMD's processor recovery loop and its host tests.
//! A watchdog is an observation failure, never permission to reuse DSP memory.

pub const STOP_WATCHDOG_MS: u64 = 35_000;
const TIMED_OUT: i32 = -33;
const GENERAL: i32 = -2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopProgress {
    /// The kernel has not acknowledged a terminal outcome. Keep polling, including after the watchdog fires.
    Pending(Option<i32>),
    /// Only phase 2 with no error authorizes protocol reset and firmware reload.
    Complete([u32; 2]),
    Failed(i32),
}

pub fn stop_progress(state: Result<[u32; 4], i32>, elapsed_ms: u64) -> StopProgress {
    match state {
        Ok([2, 0, mss, adsp]) => StopProgress::Complete([mss, adsp]),
        Ok([3, error, _, _]) => {
            StopProgress::Failed(if error == 0 { GENERAL } else { error as i32 })
        }
        Ok(_) => StopProgress::Pending((elapsed_ms >= STOP_WATCHDOG_MS).then_some(TIMED_OUT)),
        Err(error) => StopProgress::Pending((elapsed_ms >= STOP_WATCHDOG_MS).then_some(error)),
    }
}

/// True when `level` turns on: `latch` keeps the last level seen.
pub fn rising(latch: &mut bool, level: bool) -> bool {
    let edge = level && !*latch;
    *latch = level;
    edge
}

/// A lost state read or an incomplete ADSP/channel state cannot justify retaining that processor.
pub fn recovery_mask(adsp: Result<[u32; 4], i32>, apr_quiescent: bool) -> u32 {
    // READY (bit 1) is a positive remote signal; cached step/channels alone do not prove health.
    let healthy = matches!(adsp, Ok([3 | 4, 0, bits, 3]) if bits & 3 == 2);
    if healthy && apr_quiescent { 1 } else { 3 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_quarantines_busy_unknown_and_failed_reads_until_terminal() {
        assert_eq!(
            stop_progress(Ok([1, 0, 4, 7]), 34_999),
            StopProgress::Pending(None)
        );
        for state in [Ok([1, 0, 4, 7]), Ok([0, 0, 4, 7])] {
            assert_eq!(
                stop_progress(state, 35_000),
                StopProgress::Pending(Some(TIMED_OUT))
            );
        }
        assert_eq!(
            stop_progress(Err(-18), 36_000),
            StopProgress::Pending(Some(-18))
        );
        // A late acknowledged shutdown still permits progress, without issuing a second stop.
        assert_eq!(
            stop_progress(Ok([2, 0, 5, 8]), 40_000),
            StopProgress::Complete([5, 8])
        );
    }

    #[test]
    fn partial_stop_and_terminal_error_never_authorize_reload() {
        assert_eq!(
            stop_progress(Ok([1, 0, 5, 7]), 36_000),
            StopProgress::Pending(Some(TIMED_OUT))
        );
        assert_eq!(
            stop_progress(Ok([3, (-33i32) as u32, 5, 7]), 36_000),
            StopProgress::Failed(-33)
        );
        assert_eq!(
            stop_progress(Ok([2, (-33i32) as u32, 5, 7]), 36_000),
            StopProgress::Pending(Some(TIMED_OUT))
        );
        assert_eq!(
            stop_progress(Ok([3, 0, 5, 7]), 0),
            StopProgress::Failed(GENERAL)
        );
    }

    #[test]
    fn modem_only_restart_requires_known_healthy_adsp_and_quiescent_apr() {
        assert_eq!(recovery_mask(Ok([4, 0, 6, 3]), true), 1);
        for state in [
            Err(-18),
            Ok([4, 0, 7, 3]),
            Ok([4, 0, 0, 3]),
            Ok([4, 0, 4, 3]),
            Ok([4, 1, 6, 3]),
            Ok([2, 0, 6, 3]),
            Ok([4, 0, 6, 1]),
        ] {
            assert_eq!(recovery_mask(state, true), 3);
        }
        assert_eq!(recovery_mask(Ok([4, 0, 6, 3]), false), 3);
    }

    #[test]
    fn fatal_counts_once_until_it_clears() {
        let mut latch = false;
        assert!(rising(&mut latch, true));
        assert!(!rising(&mut latch, true)); // still set after the stop: not a new fault
        assert!(!rising(&mut latch, false));
        assert!(rising(&mut latch, true));
    }

    #[test]
    fn a_healthy_quiet_adsp_is_kept_for_a_modem_failure() {
        // the ADSP state the phone reported when the modem crashed after a call (2026-11-12)
        assert_eq!(recovery_mask(Ok([4, 0, 6, 3]), true), 1);
        assert_eq!(recovery_mask(Ok([4, 0, 6, 3]), false), 3); // audio still running: both restart
        assert_eq!(recovery_mask(Err(-5), true), 3);
    }
}
