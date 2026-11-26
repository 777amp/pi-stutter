use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const PROBE_INTERVAL_MS: u64 = 10;
const HISTORY_CAP: usize = 300; // ~3 seconds at 10ms per sample

#[derive(Debug, Clone, Default)]
pub struct JitterSnapshot {
    /// Latest wakeup overshoot in milliseconds
    pub last_ms: f64,
    /// Maximum wakeup overshoot observed since the last snapshot drain (ms)
    pub window_max_ms: f64,
    /// 95th percentile over recent samples (ms)
    pub p95_ms: f64,
    /// 99th percentile over recent samples (ms)
    pub p99_ms: f64,
    /// All-time peak since start/reset (ms)
    pub peak_ms: f64,
    /// Total samples collected
    pub sample_count: u64,
}

struct JitterState {
    samples_us: VecDeque<u64>,
    window_max_us: u64,
    peak_us: u64,
    last_us: u64,
    sample_count: u64,
}

impl Default for JitterState {
    fn default() -> Self {
        Self {
            samples_us: VecDeque::with_capacity(HISTORY_CAP),
            window_max_us: 0,
            peak_us: 0,
            last_us: 0,
            sample_count: 0,
        }
    }
}

pub struct JitterProbe {
    state: Arc<Mutex<JitterState>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl JitterProbe {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(JitterState::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let state_bg = Arc::clone(&state);
        let stop_bg = Arc::clone(&stop);

        let handle = thread::Builder::new()
            .name("jitter-probe".into())
            .spawn(move || {
                let target = Duration::from_millis(PROBE_INTERVAL_MS);
                let target_us = target.as_micros() as u64;

                while !stop_bg.load(Ordering::Relaxed) {
                    let before = Instant::now();
                    thread::sleep(target);
                    let elapsed_us = before.elapsed().as_micros() as u64;
                    let overshoot_us = elapsed_us.saturating_sub(target_us);

                    if let Ok(mut st) = state_bg.lock() {
                        st.last_us = overshoot_us;
                        if overshoot_us > st.window_max_us {
                            st.window_max_us = overshoot_us;
                        }
                        if overshoot_us > st.peak_us {
                            st.peak_us = overshoot_us;
                        }
                        if st.samples_us.len() >= HISTORY_CAP {
                            st.samples_us.pop_front();
                        }
                        st.samples_us.push_back(overshoot_us);
                        st.sample_count += 1;
                    }
                }
            })
            .ok();

        Self {
            state,
            stop,
            handle,
        }
    }

    /// Take a snapshot and reset the per-tick `window_max_ms`.
    pub fn take_snapshot(&self) -> JitterSnapshot {
        let Ok(mut st) = self.state.lock() else {
            return JitterSnapshot::default();
        };

        let mut sorted: Vec<u64> = st.samples_us.iter().copied().collect();
        sorted.sort_unstable();

        let p95_us = percentile(&sorted, 0.95);
        let p99_us = percentile(&sorted, 0.99);

        let snap = JitterSnapshot {
            last_ms: st.last_us as f64 / 1000.0,
            window_max_ms: st.window_max_us as f64 / 1000.0,
            p95_ms: p95_us as f64 / 1000.0,
            p99_ms: p99_us as f64 / 1000.0,
            peak_ms: st.peak_us as f64 / 1000.0,
            sample_count: st.sample_count,
        };

        st.window_max_us = 0;
        snap
    }

    pub fn reset(&self) {
        if let Ok(mut st) = self.state.lock() {
            st.samples_us.clear();
            st.window_max_us = 0;
            st.peak_us = 0;
            st.last_us = 0;
            st.sample_count = 0;
        }
    }
}

impl Drop for JitterProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}
