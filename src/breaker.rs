use std::{
    ops::{Add, Range},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use rand::{Rng, RngExt, distr::Uniform, rng};

/// Latency-based circuit breaker for determining when challenges are active.
///
/// Tracks an exponential moving average of the latency. When latency goes high, it enables
/// client-side challenges to shield the upstream service from excess load.
///
/// The breaker automatically resets itself after a certain amount of time. If latency spikes again,
/// it enables challenges again and waits for a longer amount of time, in exponential-backoff
/// fashion. This prevents oscillation between open/closed states when there is ongoing load.
pub struct CircuitBreaker {
    config: Arc<ArcSwap<CircuitBreakerConfig>>,
    tripped: AtomicBool,
    state: Mutex<State>,
}

pub struct CircuitBreakerConfig {
    pub reset_below: f64,
    pub trip_above: f64,
    min_open_duration: f64,
    backoff_half_life: f64,
    decay: f64,
}

struct State {
    position: Position,
    latency: f64,
}

enum Position {
    Closed {
        prev_open: Option<Range<Instant>>,
    },
    OpenCooldown {
        tripped_at: Instant,
        cooldown_factor: f64,
    },
    Open {
        tripped_at: Instant,
    },
}

impl CircuitBreaker {
    pub fn new(config: Arc<ArcSwap<CircuitBreakerConfig>>) -> Self {
        Self {
            config,
            tripped: AtomicBool::new(false),
            state: Mutex::new(State {
                position: Position::Closed { prev_open: None },
                latency: 0.0,
            }),
        }
    }

    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Relaxed)
    }

    pub fn update(&self, latency_sample: f64) {
        self.update_with(latency_sample, Instant::now(), &mut rng());
    }

    fn update_with<R: Rng>(&self, latency_sample: f64, now: Instant, rng: &mut R) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        let config = self.config.load();
        state.latency += config.decay * (latency_sample - state.latency);
        match &state.position {
            Position::Closed { prev_open } => {
                if state.latency > config.trip_above {
                    let tripped_at = now;

                    // Calculate base delay for exponential backoff
                    let base_delay = if let Some(range) = prev_open {
                        // If the breaker tripped immediately after reset, use the previous open
                        // duration as the base delay. But if the breaker stayed closed for a while,
                        // use a smaller value so the system gradually returns to normal.
                        let prev_open_secs = range.end.duration_since(range.start).as_secs_f64();
                        let secs_since_reset = tripped_at.duration_since(range.end).as_secs_f64();
                        let decay = (0.5f64).powf(secs_since_reset / config.backoff_half_life);
                        config.min_open_duration
                            + (prev_open_secs - config.min_open_duration) * decay
                    } else {
                        // First time? Use config for the initial value.
                        config.min_open_duration
                    };

                    // Compute the new delay. Backoff only increases and is 1.5x on average.
                    let jitter = rng.sample(Uniform::new(1.0, 2.0).unwrap());
                    let new_delay = base_delay * jitter;
                    let new_delay =
                        new_delay.clamp(config.min_open_duration, 7.0 * 24.0 * 60.0 * 60.0);

                    // Express the delay as a multiple of the configuration base value
                    let cooldown_factor = new_delay / config.min_open_duration;
                    state.position = Position::OpenCooldown {
                        tripped_at,
                        cooldown_factor,
                    };
                }
            }
            Position::OpenCooldown {
                tripped_at,
                cooldown_factor,
            } => {
                let cooldown = Duration::from_secs_f64(cooldown_factor * config.min_open_duration); // recompute in case config has changed
                let cooldown_ends_at = tripped_at.add(cooldown);
                if cooldown_ends_at <= now {
                    state.position = if state.latency < config.reset_below {
                        // Optimization: If latency has already dropped, we can jump directly to the
                        // `Closed` state, without needing an additional request to advance the state.
                        Position::Closed {
                            prev_open: Some(*tripped_at..now),
                        }
                    } else {
                        // Latency is still high. We can still advance from `OpenCooldown` to `Open`
                        // but we can't close the breaker yet.
                        Position::Open {
                            tripped_at: *tripped_at,
                        }
                    };
                }
            }
            Position::Open { tripped_at } => {
                if state.latency < config.reset_below {
                    state.position = Position::Closed {
                        prev_open: Some(*tripped_at..now),
                    }
                }
            }
        }

        // Cache a copy of the open/closed state in an atomic, for quick access
        self.tripped.store(
            match state.position {
                Position::Closed { .. } => false,
                Position::OpenCooldown { .. } | Position::Open { .. } => true,
            },
            Relaxed,
        );
    }
}

impl CircuitBreakerConfig {
    pub fn new(
        trip_above: f64,
        reset_below: f64,
        smoothing: f64,
        min_open_duration: f64,
        backoff_half_life: f64,
    ) -> Self {
        Self {
            trip_above,
            reset_below,
            min_open_duration,
            backoff_half_life,
            // `smoothing` acts like a window size; the N most recent samples should make up about
            // 95% of the moving average.
            decay: 1.0 - (0.05f64).powf(1.0 / smoothing),
        }
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_debug_snapshot;
    use rand::{SeedableRng, rngs::StdRng};

    use super::*;

    use crate::breaker::{CircuitBreaker, CircuitBreakerConfig};

    fn make_config(config: CircuitBreakerConfig) -> Arc<ArcSwap<CircuitBreakerConfig>> {
        Arc::new(ArcSwap::new(Arc::new(config)))
    }

    #[test]
    fn test_latency_smoothing() {
        let now = Instant::now();
        let mut rng = StdRng::seed_from_u64(20260424);

        // Test that `CircuitBreakerConfig::new` does the correct calculation when computing the
        // moving average's decay value. Requests that fall within this window should account for
        // 95% of the moving average.
        let window = 50;
        let breaker = CircuitBreaker::new(make_config(CircuitBreakerConfig::new(
            1000.0, // exact numbers don't matter; this is just something high that we won't hit
            900.0,
            window.into(), // smoothing factor represents this 95% window size
            1.0,
            1.0,
        )));

        // Fill the window with requests and assert that the moving average went 95% of the way.
        let input = 789.0;
        let expected = 0.95 * input;
        for _ in 0..window {
            breaker.update_with(input, now, &mut rng);
        }
        let average = breaker.state.lock().unwrap().latency;
        assert!(
            (average - expected).abs() < 1.0,
            "expected {expected} but got {average}"
        );

        // Do it again, but this time driving the moving average back down.
        let input = 123.0;
        let expected = 0.95 * input + 0.05 * average;
        for _ in 0..window {
            breaker.update_with(input, now, &mut rng);
        }
        let average = breaker.state.lock().unwrap().latency;
        assert!(
            (average - expected).abs() < 1.0,
            "expected {expected} but got {average}"
        );
    }

    #[test]
    fn test_exponential_backoff() {
        let mut now = Instant::now();
        let mut rng = StdRng::seed_from_u64(20260516);

        // Each time the breaker trips under sustained load, resets, and immediately re-trips, the
        // cooldown should roughly double (modulo jitter).
        let breaker = CircuitBreaker::new(make_config(CircuitBreakerConfig::new(
            1.0,         // trip when latency > 1 second
            0.5,         // reset when latency < 0.5 seconds
            10.0,        // average latency over ~10 requests
            60.0,        // when breaker trips, stay open for at least a minute
            1_000_000.0, // ignore backoff half life for now
        )));

        // Simulate sustained load until the breaker opens and closes 10 times
        let mut open_durations = vec![];
        while open_durations.len() < 10 {
            // Immediately trip the breaker with high-latency requests
            while !breaker.is_tripped() {
                breaker.update_with(2.0, now, &mut rng);
            }
            let tripped_at = now;

            // Simulate passing time and low-latency requests until the breaker resets
            while breaker.is_tripped() {
                now = now.add(Duration::from_millis(100));
                breaker.update_with(0.0, now, &mut rng);
            }
            open_durations.push(now.duration_since(tripped_at).as_secs_f32().round() as u32);
        }

        // This should 1x-2x each time the breaker opens
        assert_debug_snapshot!(open_durations, @"
        [
            63,
            108,
            179,
            232,
            304,
            314,
            607,
            1132,
            1525,
            2731,
        ]
        ");
    }

    #[test]
    fn test_stays_closed() {
        let breaker = CircuitBreaker::new(make_config(CircuitBreakerConfig {
            reset_below: 0.500,
            trip_above: 1.000,
            decay: 0.01,
            min_open_duration: 60.0,
            backoff_half_life: 1.0,
        }));
        let mut now = Instant::now();
        let mut rng = StdRng::seed_from_u64(20260424);

        for _ in 0..500 {
            now = now.add(Duration::from_millis(1));

            // Individual latencies go above both thresholds, but the moving average should stay
            // close to 2.7s/3 ~ 0.9s. This sits between the two thresholds and is just shy of the
            // upper one, so it shouldn't trip the breaker.
            breaker.update_with(0.001, now, &mut rng);
            breaker.update_with(0.7, now, &mut rng);
            breaker.update_with(2.0, now, &mut rng);
            assert!(!breaker.is_tripped());
        }
        let latency_avg = breaker.state.lock().unwrap().latency;
        assert!(0.5 < latency_avg && latency_avg < 1.0);
    }
}
