use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::oneshot;

use crate::ml::features::RequestFeatures;

/// Number of recent requests retained before old entries roll off.
pub const CAPACITY: usize = 256;

/// Collects features of recently received requests.
///
/// This struct serves two main purposes. One is to make it easy to collect samples on demand for ML
/// training purposes, during times when the system is healthy and we want to train on that. The
/// other purpose is to constantly keep track of the most recent requests; these help existing
/// models like shift forests come online faster when the system goes unhealthy.
pub struct Recorder {
    state: Mutex<State>,
}

struct State {
    /// Ring buffer of the most recent requests.
    recent: VecDeque<RequestFeatures>,
    /// The sample currently being collected, if any.
    sample: Option<Sample>,
}

/// An in-progress sample of requests being gathered for training.
struct Sample {
    /// Features collected so far.
    collected: Vec<RequestFeatures>,
    /// Size at which the sample is complete.
    target: usize,
    /// Where to send the collected features.
    done: oneshot::Sender<Vec<RequestFeatures>>,
}

impl Recorder {
    pub fn new() -> Self {
        Recorder {
            state: Mutex::new(State {
                recent: VecDeque::with_capacity(CAPACITY),
                sample: None,
            }),
        }
    }

    /// Records a single request's features.
    ///
    /// This is intended to be called on every new request, after it has been received but before it
    /// has been proxied or challenged.
    pub fn record(&self, features: RequestFeatures) {
        let mut state = self.state.lock().unwrap();

        // Add to the ring buffer
        if state.recent.len() == CAPACITY {
            state.recent.pop_front();
        }
        state.recent.push_back(features);

        // If a sample is in progress, add it there as well
        let complete = match state.sample.as_mut() {
            Some(sample) => {
                sample.collected.push(features);
                sample.collected.len() >= sample.target
            }
            None => false,
        };
        if complete {
            let sample = state.sample.take().unwrap();
            // Ignore dropped receivers
            let _ = sample.done.send(sample.collected);
        }
    }

    /// Starts collecting a contiguous sample of features for ML training.
    ///
    /// Samples are returned asynchronously along a oneshot channel. Only one sample can be in
    /// progress at a time; starting a new sample while one is in progress will flush the incomplete
    /// sample to the old channel. This means the sample length is not guaranteed to be `size`.
    pub fn start_sample(&self, size: usize) -> oneshot::Receiver<Vec<RequestFeatures>> {
        let (done, receiver) = oneshot::channel();
        let mut state = self.state.lock().unwrap();

        // Flush any in-progress sample
        if let Some(previous) = state.sample.take() {
            let _ = previous.done.send(previous.collected);
        }

        // Create a blank Sample struct
        state.sample = Some(Sample {
            collected: Vec::with_capacity(size),
            target: size,
            done,
        });
        receiver
    }
}
