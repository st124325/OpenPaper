//! Lock-free playback clock shared by the native WASAPI and D3D11 workers.
//!
//! WASAPI is the master because its device clock represents what the user
//! actually hears. Video falls back to its own monotonic clock when a file has
//! no native audio stream or the audio backend is replaced by libVLC.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Default)]
pub struct PlaybackClock {
    active: AtomicBool,
    position_100ns: AtomicU64,
}

impl PlaybackClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn activate(&self) {
        self.position_100ns.store(0, Ordering::Release);
        self.active.store(true, Ordering::Release);
    }

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub fn update_from_device(&self, device_position: u64, origin: u64, frequency: u64) {
        if frequency == 0 {
            return;
        }
        let elapsed = device_position.saturating_sub(origin);
        let whole_seconds = elapsed / frequency;
        let remainder = elapsed % frequency;
        let position_100ns = whole_seconds
            .saturating_mul(10_000_000)
            .saturating_add(remainder.saturating_mul(10_000_000) / frequency);
        self.position_100ns.store(position_100ns, Ordering::Release);
    }

    pub fn snapshot(&self) -> Option<u64> {
        self.active
            .load(Ordering::Acquire)
            .then(|| self.position_100ns.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_device_ticks_without_overflowing_the_multiplication() {
        let clock = PlaybackClock::new();
        clock.activate();
        clock.update_from_device(48_000 * 3 + 24_000, 0, 48_000);
        assert_eq!(clock.snapshot(), Some(35_000_000));
    }

    #[test]
    fn inactive_clock_has_no_snapshot() {
        let clock = PlaybackClock::new();
        clock.activate();
        clock.deactivate();
        assert_eq!(clock.snapshot(), None);
    }
}
