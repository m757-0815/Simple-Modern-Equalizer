use crate::state::SharedState;
use resonance_ipc::Response;
use std::{
    collections::VecDeque,
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};

const FRAMES: usize = 8192;
const FRESHNESS: Duration = Duration::from_millis(250);
type Sample = (u32, [f32; 4]);

#[derive(Default)]
pub struct Capture {
    enabled: Arc<AtomicBool>,
    requested: Option<Instant>,
    updated: Option<Instant>,
    rate: u32,
    frames: VecDeque<[f32; 4]>,
}

impl Capture {
    pub fn read(&mut self, requested_frames: u16) -> Response {
        self.requested = Some(Instant::now());
        self.enabled.store(true, Ordering::Relaxed);
        let frames = if self.updated.is_some_and(|last| last.elapsed() < FRESHNESS) {
            let start = self.frames.len().saturating_sub(usize::from(requested_frames));
            self.frames.iter().skip(start).copied().collect()
        } else {
            Vec::new()
        };
        Response::MonitorCapture { rate: self.rate, frames }
    }
}

pub struct Tap {
    tx: rtrb::Producer<Sample>,
    enabled: Arc<AtomicBool>,
}

impl Tap {
    pub fn record(&mut self, rate: f64, frames: usize, sample: impl Fn(usize) -> [f32; 4]) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let rate = rate.round() as u32;
        for index in 0..frames.min(self.tx.slots()) {
            let _ = self.tx.push((rate, sample(index)));
        }
    }
}

pub fn spawn(state: SharedState) -> Tap {
    let (tx, mut rx) = rtrb::RingBuffer::new(FRAMES);
    let enabled = {
        let mut inner = state.0.lock().unwrap();
        inner.monitor.frames.reserve(FRAMES);
        inner.monitor.enabled.clone()
    };
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(8));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let mut inner = state.0.lock().unwrap();
            let capture = &mut inner.monitor;
            let count = rx.slots();
            if capture.requested.is_some_and(|time| time.elapsed() < FRESHNESS) {
                if capture.updated.is_none_or(|time| time.elapsed() >= FRESHNESS) {
                    capture.frames.clear();
                }
                for _ in 0..count {
                    let Ok((rate, frame)) = rx.pop() else { break };
                    if capture.rate != rate {
                        capture.rate = rate;
                        capture.frames.clear();
                    }
                    if capture.frames.len() == FRAMES {
                        capture.frames.pop_front();
                    }
                    capture.frames.push_back(frame);
                }
                if count > 0 {
                    capture.updated = Some(Instant::now());
                }
            } else {
                capture.enabled.store(false, Ordering::Relaxed);
                for _ in 0..count {
                    let _ = rx.pop();
                }
                capture.frames.clear();
                capture.updated = None;
            }
        }
    });
    Tap { tx, enabled }
}
