// SPDX-License-Identifier: GPL-3.0-only

//! Steady frame pacing out of the shared memory ring.
//!
//! Frames arrive irregularly (network jitter, a slow decode of a key
//! frame). The source shows one frame per frame period, keeping `target`
//! frames buffered: a late frame is covered by the buffer, a burst is
//! played out in order instead of overwriting frames.

use std::time::{Duration, Instant};

use vcam_shm::RING;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows_core::PCWSTR;

/// Which frame number to show at each tick.
#[derive(Debug, Default)]
pub struct Playout {
    next: Option<u64>,
}

impl Playout {
    /// `written`: frames published by the agent. `None`: show the previous
    /// frame again (buffering, or the agent is late).
    pub fn pick(&mut self, written: u64, target: u64) -> Option<u64> {
        let target = target.clamp(1, RING - 2);
        let mut next = match self.next {
            Some(n) => n,
            // Start once `target` frames are buffered.
            None if written >= target => written - target,
            None => return None,
        };
        // Fell out of the ring (the source stalled): back to `target`.
        if written > next + (RING - 1) {
            next = written - target;
        }
        // Clock drift between the client and us lets the buffer grow: drop
        // one frame to keep the latency bounded.
        if written > next + target + 1 {
            next += 1;
        }
        if next >= written {
            // Underrun: repeat, and show `next` once it arrives.
            self.next = Some(next);
            return None;
        }
        self.next = Some(next + 1);
        Some(next)
    }

    /// The frame was overwritten under us: pick again from the newest.
    pub fn resync(&mut self) {
        self.next = None;
    }
}

/// Sleeps to a deadline with a high resolution timer (the default timer
/// resolution, ~15 ms, would add its own jitter at 30 fps).
pub struct Ticker {
    timer: Option<HANDLE>,
    deadline: Option<Instant>,
}

unsafe impl Send for Ticker {}

impl Ticker {
    pub fn new() -> Self {
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .ok()
        };
        Ticker {
            timer,
            deadline: None,
        }
    }

    /// Waits until one `period` after the previous tick (now for the first
    /// one, or after a stall longer than a period).
    pub fn tick(&mut self, period: Duration) {
        let now = Instant::now();
        let deadline = match self.deadline {
            Some(d) if d + period * 2 > now => d + period,
            _ => now,
        };
        self.deadline = Some(deadline);
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return;
        }
        match self.timer {
            Some(timer) => unsafe {
                // Relative due time, 100 ns units.
                let due = -((wait.as_nanos() / 100) as i64).max(1);
                if SetWaitableTimer(timer, &due, 0, None, None, false).is_ok() {
                    WaitForSingleObject(timer, u32::MAX);
                } else {
                    std::thread::sleep(wait);
                }
            },
            None => std::thread::sleep(wait),
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        if let Some(t) = self.timer {
            unsafe {
                let _ = CloseHandle(t);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_stream_keeps_target() {
        let mut p = Playout::default();
        assert_eq!(p.pick(0, 3), None);
        assert_eq!(p.pick(2, 3), None);
        assert_eq!(p.pick(3, 3), Some(0));
        // One frame arrives per tick: frames come out in order.
        for n in 1..20 {
            assert_eq!(p.pick(n + 3, 3), Some(n));
        }
    }

    #[test]
    fn late_frame_and_burst_are_absorbed() {
        let mut p = Playout::default();
        assert_eq!(p.pick(3, 3), Some(0));
        assert_eq!(p.pick(4, 3), Some(1));
        // Two frames late: the buffer covers them.
        assert_eq!(p.pick(4, 3), Some(2));
        assert_eq!(p.pick(4, 3), Some(3));
        // Then they arrive together: nothing skipped.
        assert_eq!(p.pick(7, 3), Some(4));
        assert_eq!(p.pick(7, 3), Some(5));
        assert_eq!(p.pick(8, 3), Some(6));
    }

    #[test]
    fn underrun_repeats_then_resumes() {
        let mut p = Playout::default();
        assert_eq!(p.pick(1, 1), Some(0));
        assert_eq!(p.pick(1, 1), None);
        assert_eq!(p.pick(1, 1), None);
        assert_eq!(p.pick(2, 1), Some(1));
    }

    #[test]
    fn drift_and_stall_bound_the_latency() {
        let mut p = Playout::default();
        assert_eq!(p.pick(3, 3), Some(0));
        // The client runs slightly faster: 5 waiting > target + 1, drop one.
        assert_eq!(p.pick(6, 3), Some(2));
        // The source stalled, the agent went far ahead: back to target.
        assert_eq!(p.pick(30, 3), Some(27));
    }
}
