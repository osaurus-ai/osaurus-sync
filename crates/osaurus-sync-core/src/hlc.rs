//! Hybrid logical clock: physical milliseconds in the high 48 bits, a logical counter in the low 16.

pub const LOGICAL_BITS: u32 = 16;

/// Remote clocks further ahead than this are not adopted, so a peer with a broken or forged
/// clock cannot drag every device's clock into the future.
pub const MAX_FORWARD_DRIFT_MS: u64 = 60 * 60 * 1000;

pub fn pack(physical_ms: u64, logical: u16) -> u64 {
    (physical_ms << LOGICAL_BITS) | logical as u64
}

pub fn physical_ms(hlc: u64) -> u64 {
    hlc >> LOGICAL_BITS
}

#[derive(Debug, Clone, Default)]
pub struct HlcClock {
    last: u64,
}

impl HlcClock {
    pub fn new(last: u64) -> Self {
        Self { last }
    }

    pub fn last(&self) -> u64 {
        self.last
    }

    /// Next timestamp for a local event. Strictly increasing on this device.
    pub fn tick(&mut self, now_ms: u64) -> u64 {
        let phys = pack(now_ms, 0);
        self.last = phys.max(self.last + 1);
        self.last
    }

    /// Advance past a received timestamp.
    pub fn observe(&mut self, remote: u64, now_ms: u64) {
        if physical_ms(remote) <= now_ms.saturating_add(MAX_FORWARD_DRIFT_MS) {
            self.last = self.last.max(remote);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_when_wall_clock_goes_back() {
        let mut c = HlcClock::default();
        let a = c.tick(1_000);
        let b = c.tick(900);
        let d = c.tick(900);
        assert!(a < b && b < d);
        assert_eq!(physical_ms(b), 1_000);
    }

    #[test]
    fn observe_advances_past_remote() {
        let mut c = HlcClock::default();
        c.tick(1_000);
        c.observe(pack(2_000, 5), 1_000);
        assert!(c.tick(1_000) > pack(2_000, 5));
    }

    #[test]
    fn observe_ignores_far_future() {
        let mut c = HlcClock::default();
        let before = c.tick(1_000);
        c.observe(pack(1_000 + MAX_FORWARD_DRIFT_MS + 1, 0), 1_000);
        assert_eq!(c.last(), before);
    }
}
