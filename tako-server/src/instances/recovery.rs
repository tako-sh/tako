//! Crash backoff and last-exit bookkeeping for one app.
//!
//! Pure state: callers pass the clock and the jitter, so the schedule can be
//! tested without sleeping.

use std::time::{Duration, Instant};
use tako_core::{InstanceExit, RecoveryStatus};

/// Delay before the second consecutive replacement; doubles from here.
pub(crate) const RESTART_BACKOFF_BASE: Duration = Duration::from_secs(1);
pub(crate) const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long the app must stay healthy before its backoff is forgotten.
pub(crate) const RESTART_STABLE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub(crate) struct Recovery {
    /// Instance losses since the last reset; drives the backoff.
    failures: u32,
    /// Replacement starts since the last reset.
    restarts: u32,
    last_exit: Option<InstanceExit>,
    retry_at: Option<Instant>,
    retry_at_unix_secs: Option<i64>,
    healthy_since: Option<Instant>,
}

/// Full delay after `failures` consecutive losses. The first loss is replaced
/// immediately; later ones wait 1s, 2s, 4s, ... up to the cap.
pub(crate) fn restart_backoff(failures: u32) -> Duration {
    let Some(doublings) = failures.checked_sub(2) else {
        return Duration::ZERO;
    };
    RESTART_BACKOFF_BASE
        .checked_mul(1u32.checked_shl(doublings).unwrap_or(u32::MAX))
        .unwrap_or(RESTART_BACKOFF_MAX)
        .min(RESTART_BACKOFF_MAX)
}

/// Spread the delay over its upper half so apps that fail together (a shared
/// database going away) do not all restart in the same instant.
/// `jitter` is a unit-interval sample.
pub(crate) fn jittered(delay: Duration, jitter: f64) -> Duration {
    delay.mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0))
}

impl Recovery {
    pub(crate) fn record_exit(
        &mut self,
        exit: InstanceExit,
        now: Instant,
        now_unix_secs: i64,
        jitter: f64,
    ) {
        self.failures = self.failures.saturating_add(1);
        self.healthy_since = None;
        let delay = jittered(restart_backoff(self.failures), jitter);
        if delay.is_zero() {
            self.retry_at = None;
            self.retry_at_unix_secs = None;
        } else {
            self.retry_at = Some(now + delay);
            self.retry_at_unix_secs =
                Some(now_unix_secs.saturating_add(delay.as_secs_f64().ceil() as i64));
        }
        self.last_exit = Some(exit);
    }

    /// Time left before another replacement may start.
    pub(crate) fn backoff_remaining(&self, now: Instant) -> Option<Duration> {
        self.retry_at
            .and_then(|at| at.checked_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
    }

    pub(crate) fn record_restart(&mut self) {
        self.restarts = self.restarts.saturating_add(1);
        self.retry_at = None;
        self.retry_at_unix_secs = None;
    }

    /// Feed one observation of whether the app is at its minimum and fully
    /// healthy. Forgets the backoff once that has held for the stable window.
    pub(crate) fn observe_health(&mut self, healthy: bool, now: Instant) {
        if !self.has_history() {
            return;
        }
        if !healthy {
            self.healthy_since = None;
            return;
        }
        let since = *self.healthy_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= RESTART_STABLE_WINDOW {
            self.reset();
        }
    }

    /// Whether a loss or a replacement was recorded since the last reset.
    pub(crate) fn has_history(&self) -> bool {
        self.failures != 0 || self.restarts != 0
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn status(&self, now: Instant) -> Option<RecoveryStatus> {
        if !self.has_history() {
            return None;
        }
        Some(RecoveryStatus {
            restart_count: self.restarts,
            last_exit: self.last_exit.clone(),
            next_restart_at_unix_secs: self.backoff_remaining(now).and(self.retry_at_unix_secs),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tako_core::InstanceExitReason;

    fn exit(reason: InstanceExitReason) -> InstanceExit {
        InstanceExit {
            reason,
            exit_code: Some(1),
            signal: None,
            at_unix_secs: 1_000,
        }
    }

    #[test]
    fn backoff_starts_after_the_first_loss_doubles_and_caps() {
        let delays: Vec<u64> = (0..=9).map(|n| restart_backoff(n).as_secs()).collect();
        assert_eq!(delays, [0, 0, 1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(restart_backoff(u32::MAX), RESTART_BACKOFF_MAX);
    }

    #[test]
    fn jitter_keeps_the_delay_within_its_upper_half() {
        let full = Duration::from_secs(8);
        assert_eq!(jittered(full, 0.0), Duration::from_secs(4));
        assert_eq!(jittered(full, 1.0), full);
        assert_eq!(jittered(full, 0.5), Duration::from_secs(6));
        assert_eq!(jittered(full, 7.0), full);
        assert_eq!(jittered(Duration::ZERO, 0.9), Duration::ZERO);
    }

    #[test]
    fn first_loss_is_replaceable_at_once_and_later_ones_wait() {
        let start = Instant::now();
        let mut recovery = Recovery::default();
        assert!(recovery.status(start).is_none());

        recovery.record_exit(exit(InstanceExitReason::Crashed), start, 1_000, 1.0);
        assert_eq!(recovery.backoff_remaining(start), None);
        let status = recovery.status(start).unwrap();
        assert_eq!(status.restart_count, 0);
        assert_eq!(status.next_restart_at_unix_secs, None);

        recovery.record_restart();
        recovery.record_exit(exit(InstanceExitReason::FailedToStart), start, 1_000, 1.0);
        assert_eq!(
            recovery.backoff_remaining(start),
            Some(Duration::from_secs(1))
        );
        let status = recovery.status(start).unwrap();
        assert_eq!(status.restart_count, 1);
        assert_eq!(status.next_restart_at_unix_secs, Some(1_001));
        assert_eq!(
            status.last_exit.unwrap().reason,
            InstanceExitReason::FailedToStart
        );

        // Due: nothing left to wait for, and status stops advertising a retry.
        let due = start + Duration::from_secs(1);
        assert_eq!(recovery.backoff_remaining(due), None);
        assert_eq!(
            recovery.status(due).unwrap().next_restart_at_unix_secs,
            None
        );

        for _ in 0..10 {
            recovery.record_restart();
            recovery.record_exit(exit(InstanceExitReason::Crashed), start, 1_000, 1.0);
        }
        assert_eq!(recovery.backoff_remaining(start), Some(RESTART_BACKOFF_MAX));
        assert_eq!(recovery.status(start).unwrap().restart_count, 11);
    }

    #[test]
    fn starting_a_replacement_clears_the_pending_retry() {
        let start = Instant::now();
        let mut recovery = Recovery::default();
        recovery.record_exit(exit(InstanceExitReason::Crashed), start, 1_000, 0.0);
        recovery.record_exit(exit(InstanceExitReason::Crashed), start, 1_000, 0.0);
        assert!(recovery.backoff_remaining(start).is_some());
        recovery.record_restart();
        assert_eq!(recovery.backoff_remaining(start), None);
    }

    #[test]
    fn backoff_resets_only_after_a_continuous_stable_window() {
        let start = Instant::now();
        let mut recovery = Recovery::default();
        for _ in 0..4 {
            recovery.record_exit(exit(InstanceExitReason::Crashed), start, 1_000, 1.0);
            recovery.record_restart();
        }

        recovery.observe_health(true, start);
        recovery.observe_health(true, start + RESTART_STABLE_WINDOW / 2);
        // A blip restarts the window.
        recovery.observe_health(false, start + RESTART_STABLE_WINDOW / 2);
        recovery.observe_health(true, start + RESTART_STABLE_WINDOW);
        assert!(recovery.status(start).is_some());
        // A new loss inside the window continues the schedule.
        recovery.record_exit(
            exit(InstanceExitReason::Crashed),
            start + RESTART_STABLE_WINDOW,
            1_060,
            1.0,
        );
        assert_eq!(
            recovery.backoff_remaining(start + RESTART_STABLE_WINDOW),
            Some(Duration::from_secs(8))
        );

        let healthy_from = start + RESTART_STABLE_WINDOW * 2;
        recovery.observe_health(true, healthy_from);
        recovery.observe_health(
            true,
            healthy_from + RESTART_STABLE_WINDOW - Duration::from_secs(1),
        );
        assert!(recovery.status(healthy_from).is_some());
        recovery.observe_health(true, healthy_from + RESTART_STABLE_WINDOW);
        assert!(recovery.status(healthy_from).is_none());

        // After the reset the next loss is a first loss again.
        let later = healthy_from + RESTART_STABLE_WINDOW * 2;
        recovery.record_exit(exit(InstanceExitReason::Crashed), later, 2_000, 1.0);
        assert_eq!(recovery.backoff_remaining(later), None);
    }
}
