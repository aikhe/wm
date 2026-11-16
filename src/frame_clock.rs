//! Paces animation frames to the compositor, in the one loop. Nothing here
//! runs while the desktop is at rest.
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmFlush, DwmGetCompositionTimingInfo};

#[derive(Default)]
pub struct FrameClock {
    /// When the last frame was presented, while frames are being made.
    last: Option<Instant>,
    /// Seconds between two refreshes of the screen.
    period: f64,
}

impl FrameClock {
    /// Forgets the last frame, so the next run starts fresh.
    pub fn rest(&mut self) {
        self.last = None;
    }

    /// Blocks until the compositor has presented what was submitted, then
    /// returns the seconds the next frame should advance by. That is a whole
    /// number of refreshes, so the jitter in when this thread wakes never
    /// shows as uneven motion, and a missed refresh is caught up on.
    pub fn wait(&mut self) -> f64 {
        // It waits on the compositor, never on an app. Without one (a remote
        // session) there's nothing to wait for, so don't spin.
        if unsafe { DwmFlush() }.is_err() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let now = Instant::now();
        let Some(last) = self.last.replace(now) else {
            self.period = refresh_period();
            return self.period;
        };
        let refreshes = (now.duration_since(last).as_secs_f64() / self.period).round();
        refreshes.max(1.0) * self.period
    }
}

fn refresh_period() -> f64 {
    let mut timing = DWM_TIMING_INFO {
        cbSize: size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    let rate = match unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut timing) } {
        Ok(()) => timing.rateRefresh,
        Err(_) => Default::default(),
    };
    let (refreshes, seconds) = (rate.uiNumerator, rate.uiDenominator);
    if refreshes == 0 || seconds == 0 {
        return 1.0 / 60.0;
    }
    f64::from(seconds) / f64::from(refreshes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_advance_by_whole_refreshes_and_a_run_starts_with_one() {
        let mut clock = FrameClock::default();
        let first = clock.wait();
        assert!((1.0 / 500.0..=1.0 / 20.0).contains(&first), "{first}");
        std::thread::sleep(Duration::from_secs_f64(first * 2.6));
        let late = clock.wait();
        let refreshes = late / first;
        assert!((refreshes - refreshes.round()).abs() < 1e-9 && refreshes >= 3.0);
        clock.rest();
        assert_eq!(clock.wait(), first);
    }
}
