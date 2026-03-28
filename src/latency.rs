use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct LatencyMonitor {
    // No AtomicF64 in std, so we store the f64 as raw bits in a u64
    avg_as_u64: AtomicU64,
    high_load: AtomicBool,
    weight: f64,
    high_ms: f64,
    low_ms: f64,
}

impl LatencyMonitor {
    pub fn new(weight: f64, high_ms: f64, low_ms: f64) -> Self {
        Self {
            avg_as_u64: AtomicU64::new(0.0_f64.to_bits()),
            high_load: AtomicBool::new(false),
            weight,
            high_ms,
            low_ms,
        }
    }

    /// Update moving average with a new latency sample, returning true if `high_load` changed.
    pub fn update(&self, latency_ms: f64) -> bool {
        // CAS loop to atomically update the moving average. All atomics in this function use
        // Relaxed ordering because these values are approximate statistics; so long as
        // `self.high_load` eventually reflects the correct value, we should be good.
        let new_avg = loop {
            let current_bits = self.avg_as_u64.load(Ordering::Relaxed);
            let current = f64::from_bits(current_bits);
            let new = (1.0 - self.weight) * current + self.weight * latency_ms;
            if self
                .avg_as_u64
                .compare_exchange_weak(
                    current_bits,
                    new.to_bits(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                break new;
            }
        };

        // Use hysteresis thresholds to optionally update `self.high_load`
        let old_high_load = self.high_load.load(Ordering::Relaxed);
        let threshold = if old_high_load {
            self.low_ms
        } else {
            self.high_ms
        };
        let new_high_load = new_avg > threshold;
        if new_high_load != old_high_load {
            self.high_load.store(new_high_load, Ordering::Relaxed);
            return true;
        }
        false
    }

    pub fn average(&self) -> f64 {
        f64::from_bits(self.avg_as_u64.load(Ordering::Relaxed))
    }

    pub fn is_high_load(&self) -> bool {
        self.high_load.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_load_transitions() {
        const HIGH_MS: f64 = 100.0;
        const LOW_MS: f64 = 50.0;

        let monitor = LatencyMonitor::new(0.1, HIGH_MS, LOW_MS);
        let update_until_flip = |sample_ms: f64| {
            let mut flag = false;
            for _ in 0..1000 {
                if monitor.update(sample_ms) {
                    flag = true;
                }
            }
            assert!(flag, "monitor.update() never returned true");
        };

        // Initialize with low latency (10 ms), low load
        for _ in 0..100 {
            monitor.update(10.0);
        }
        assert!(!monitor.is_high_load());
        assert!((monitor.average() - 10.0).abs() < 0.1);

        // Feed high-latency samples (500 ms) until we enter high load
        update_until_flip(500.0);
        assert!(monitor.is_high_load());
        assert!(monitor.average() > HIGH_MS);

        // Feed low-latency samples (5 ms) until we leave high load
        update_until_flip(5.0);
        assert!(!monitor.is_high_load());
        assert!(monitor.average() < LOW_MS);
    }
}
