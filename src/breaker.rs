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
    low_threshold: f64,
    high_threshold: f64,
    decay: f64,
    base_delay: f64,
    forgiveness: f64,
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
                if state.latency > config.high_threshold {
                    let tripped_at = now;

                    // The breaker tripped. Choose the length of the cooldown period.
                    let cooldown_factor = if let Some(range) = prev_open {
                        let prev_open_secs = range.end.duration_since(range.start).as_secs_f64();
                        let closed_secs = tripped_at.duration_since(range.end).as_secs_f64();

                        // Do exponential backoff with forgiveness. If the breaker tripped
                        // immediately after the last reset, double the cooldown period this time.
                        // But if it's been a while, use a smaller delay.
                        let new_delay = 2.0 * prev_open_secs * (2.0f64).powf(-closed_secs);
                        let new_factor = new_delay / config.base_delay;
                        (1.0f64).max(new_factor) // obey a minimum cooldown of 1x of the base
                    } else {
                        1.0
                    };
                    let jitter = rng.sample(Uniform::new(0.75, 1.25).unwrap());
                    state.position = Position::OpenCooldown {
                        tripped_at,
                        cooldown_factor: jitter * cooldown_factor,
                    };
                }
            }
            Position::OpenCooldown {
                tripped_at,
                cooldown_factor,
            } => {
                let cooldown = Duration::from_secs_f64(cooldown_factor * config.base_delay); // recompute in case config has changed
                let cooldown_ends_at = tripped_at.add(cooldown);
                if cooldown_ends_at <= now {
                    state.position = if state.latency < config.low_threshold {
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
                if state.latency < config.low_threshold {
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

#[cfg(test)]
mod tests {
    use rand::{SeedableRng, rngs::StdRng};

    use super::*;

    use crate::breaker::{CircuitBreaker, CircuitBreakerConfig};

    fn make_config(config: CircuitBreakerConfig) -> Arc<ArcSwap<CircuitBreakerConfig>> {
        Arc::new(ArcSwap::new(Arc::new(config)))
    }

    #[test]
    fn test_stays_closed() {
        let breaker = CircuitBreaker::new(make_config(CircuitBreakerConfig {
            low_threshold: 0.500,  // resets at 0.5 seconds
            high_threshold: 1.000, // trips at 1 second
            decay: 0.01,
            base_delay: 60.0,
            forgiveness: 1.0,
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
