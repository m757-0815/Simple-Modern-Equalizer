use crate::state::SharedState;
use resonance_dsp::true_peak::Meter;
use resonance_ipc::Response;
use std::{
    collections::VecDeque,
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};

const FRAMES: usize = 8192;
const QUEUE_FRAMES: usize = FRAMES * 4;
const FRESHNESS: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
struct Sample {
    rate: u32,
    gap: bool,
    frame: [f32; 4],
}

fn is_recent(time: Option<Instant>) -> bool {
    time.is_some_and(|time| time.elapsed() < FRESHNESS)
}

// #######################################################
// serve monitor readings to the app
// #######################################################

#[derive(Default)]
pub struct Capture {
    enabled: Arc<AtomicBool>,
    requested: Option<Instant>,
    updated: Option<Instant>,
    rate: u32,
    frames: VecDeque<[f32; 4]>,
    peak_requested: Option<Instant>,
    peak_generation: u64,
    peaks: Option<[f32; 2]>,
}

impl Capture {
    pub fn read(&mut self, requested_frames: u16) -> Response {
        self.requested = Some(Instant::now());
        self.enabled.store(true, Ordering::Relaxed);
        let frames = if is_recent(self.updated) {
            let start = self.frames.len().saturating_sub(usize::from(requested_frames));
            self.frames.iter().skip(start).copied().collect()
        } else {
            Vec::new()
        };
        Response::MonitorCapture { rate: self.rate, frames }
    }

    pub fn read_true_peak(&mut self, reset: bool) -> Response {
        // Start a fresh reading after a reset or when the monitor is opened again.
        if reset || !is_recent(self.peak_requested) {
            self.peak_generation = self.peak_generation.wrapping_add(1);
            self.peaks = None;
        }
        self.peak_requested = Some(Instant::now());
        self.enabled.store(true, Ordering::Relaxed);
        Response::TruePeak { peaks: self.peaks.take() }
    }
}

// #######################################################
// copy playback frames from PipeWire
// #######################################################

pub struct Tap {
    tx: rtrb::Producer<Sample>,
    enabled: Arc<AtomicBool>,
    gap: bool,
}

impl Tap {
    pub fn record(&mut self, rate: f64, frames: usize, sample: impl Fn(usize) -> [f32; 4]) {
        if !self.enabled.load(Ordering::Relaxed) {
            self.gap = true;
            return;
        }
        let rate = rate.round() as u32;
        let available = frames.min(self.tx.slots());
        for index in 0..available {
            let gap = std::mem::take(&mut self.gap);
            let _ = self.tx.push(Sample { rate, gap, frame: sample(index) });
        }
        // Mark the next queued frame if the ring ran out of space.
        self.gap |= available < frames;
    }

    pub fn gap(&mut self) {
        self.gap = true;
    }
}

// #######################################################
// measure peaks on the monitor worker
// #######################################################

pub fn spawn(state: SharedState) -> Tap {
    let (tx, mut rx) = rtrb::RingBuffer::<Sample>::new(QUEUE_FRAMES);
    let enabled = {
        let mut inner = state.0.lock().unwrap();
        inner.monitor.frames.reserve(FRAMES);
        inner.monitor.enabled.clone()
    };
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(8));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut batch = Vec::with_capacity(QUEUE_FRAMES);
        let mut meter = Meter::default();
        let mut meter_rate = 0;
        loop {
            interval.tick().await;
            let (waveform_active, peak_generation) = {
                let mut inner = state.0.lock().unwrap();
                let capture = &mut inner.monitor;
                let waveform_active = is_recent(capture.requested);
                let peak_active = is_recent(capture.peak_requested);
                if !waveform_active && !peak_active {
                    capture.enabled.store(false, Ordering::Relaxed);
                }
                if !peak_active {
                    capture.peaks = None;
                }
                (waveform_active, peak_active.then_some(capture.peak_generation))
            };
            if peak_generation.is_none() {
                meter.reset_history();
            }
            batch.clear();
            let count = rx.slots();
            // Analyze the queued audio outside the shared state lock.
            for _ in 0..count {
                let Ok(sample) = rx.pop() else { break };
                if peak_generation.is_some() {
                    if sample.gap || meter_rate != sample.rate {
                        meter.reset_history();
                        meter_rate = sample.rate;
                    }
                    // Each frame carries the input pair first and the post-EQ pair last.
                    meter.push([sample.frame[2], sample.frame[3]]);
                }
                if waveform_active {
                    batch.push(sample);
                }
            }
            let peaks = meter.take_peaks();
            let mut inner = state.0.lock().unwrap();
            let capture = &mut inner.monitor;
            // Drop these peak readings if the app reset the meter during analysis.
            if let Some(peaks) = peaks.filter(|_| peak_generation == Some(capture.peak_generation)) {
                let reading = capture.peaks.get_or_insert([0.0; 2]);
                for (peak, next) in reading.iter_mut().zip(peaks) {
                    *peak = peak.max(next);
                }
            }
            if waveform_active {
                if !is_recent(capture.updated) {
                    capture.frames.clear();
                }
                for sample in &batch {
                    if sample.gap || capture.rate != sample.rate {
                        capture.rate = sample.rate;
                        capture.frames.clear();
                    }
                    if capture.frames.len() == FRAMES {
                        capture.frames.pop_front();
                    }
                    capture.frames.push_back(sample.frame);
                }
                if !batch.is_empty() {
                    capture.updated = Some(Instant::now());
                }
            } else {
                capture.frames.clear();
                capture.updated = None;
            }
        }
    });
    Tap { tx, enabled, gap: true }
}
