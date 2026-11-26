use crate::{
    live::{self, BEFORE_AFTER_MODE, LOUDNESS_HISTORY_MODE, STEREO_FIELD_MODE},
    plot_canvas::{Canvas, RasterSize},
    theme::{PLOT_GRID, PLOT_ZERO, ThemeColors},
};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::{collections::VecDeque, sync::Arc, time::{Duration, Instant}};
use tiny_skia::PathBuilder;

const WIDTH: f32 = 960.0;
const HEIGHT: f32 = 120.0;
const HISTORY_SECS: f32 = 30.0;
const HISTORY_INTERVAL: Duration = Duration::from_millis(100);
pub const FIELD_FRAMES: usize = 1024;

// #######################################################
// prepare audio for the comparison and stereo views
// #######################################################

pub enum Signal {
    Comparison { before: Vec<f32>, after: Vec<f32> },
    Stereo { samples: Vec<[f32; 2]>, correlation: Option<f32> },
}

impl Signal {
    pub fn stereo(samples: Vec<[f32; 2]>) -> Self {
        let [left, right, product] = samples.iter().fold([0.0_f64; 3], |[ll, rr, lr], &[l, r]| {
            if !l.is_finite() || !r.is_finite() {
                return [ll, rr, lr];
            }
            let (l, r) = (f64::from(l), f64::from(r));
            [ll + l * l, rr + r * r, lr + l * r]
        });
        let scale = (left * right).sqrt();
        let correlation = (scale > 1e-8).then(|| (product / scale).clamp(-1.0, 1.0) as f32);
        Self::Stereo { samples, correlation }
    }

    #[cfg(target_os = "linux")]
    pub fn comparison(frames: &[[f32; 4]], rate: u32, analyzer: &mut crate::spectrum::Analyzer) -> Option<Self> {
        let mut levels = |offset: usize| {
            analyzer.stereo_levels(frames.iter().map(|frame| [frame[offset], frame[offset + 1]]),
                f64::from(rate), crate::spectrum::Resolution::Standard).map(|bands| bands.as_slice().to_vec())
        };
        Some(Self::Comparison { before: levels(0)?, after: levels(2)? })
    }

    pub fn has_mode(&self, mode: i32) -> bool {
        matches!((mode, self),
            (BEFORE_AFTER_MODE, Self::Comparison { .. }) | (STEREO_FIELD_MODE, Self::Stereo { .. }))
    }
}

// #######################################################
// keep a rolling history of output level
// #######################################################

#[derive(Default)]
pub struct History {
    points: VecDeque<(Instant, Option<f32>)>,
    energy: Option<f32>,
    revision: u64,
}

impl History {
    pub fn clear(&mut self) {
        self.points.clear();
        self.energy = None;
        self.revision += 1;
    }

    pub fn update(&mut self, now: Instant, rms: Option<f32>) {
        if self.points.back().is_some_and(|(last, _)| now.duration_since(*last) < HISTORY_INTERVAL) {
            return;
        }
        while self.points.front().is_some_and(|(time, _)| now.duration_since(*time).as_secs_f32() > HISTORY_SECS) {
            self.points.pop_front();
        }
        self.energy = rms.filter(|rms| rms.is_finite()).map(|rms| {
            let power = rms * rms;
            self.energy.map_or(power, |previous| previous * 0.75 + power * 0.25)
        });
        self.points.push_back((now, self.energy.map(|power| live::amplitude_dbfs(power.sqrt()))));
        self.revision += 1;
    }
}

// #######################################################
// render the monitor only when its inputs change
// #######################################################

#[derive(Default)]
pub struct Plots {
    key: Option<(i32, RasterSize, ThemeColors, u64, u32)>,
    signal: Option<Arc<Signal>>,
    before: Vec<f32>,
    after: Vec<f32>,
}

impl Plots {
    pub fn frame(
        &mut self,
        mode: i32,
        size: RasterSize,
        colors: ThemeColors,
        signal: Option<&Arc<Signal>>,
        history: &History,
        now: Instant,
        elapsed: Duration,
    ) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        let revision = if mode == LOUDNESS_HISTORY_MODE { history.revision } else { 0 };
        let offset = if mode == LOUDNESS_HISTORY_MODE && history.points.len() > 1 {
            let last = history.points.back().unwrap().0;
            (now.saturating_duration_since(last).as_secs_f32() * size.width as f32 / HISTORY_SECS).floor() as u32
        } else { 0 };
        let key = (mode, size, colors, revision, offset);
        let same_signal = match (&self.signal, signal) {
            (Some(previous), Some(current)) => Arc::ptr_eq(previous, current),
            (None, None) => true,
            _ => false,
        };
        let changed = if mode == BEFORE_AFTER_MODE {
            if let Some(Signal::Comparison { before, after }) = signal.map(Arc::as_ref) {
                let before_changed = live::smooth_spectrum(&mut self.before, before, elapsed);
                live::smooth_spectrum(&mut self.after, after, elapsed) || before_changed
            } else if !self.before.is_empty() {
                let silence = [0.0; crate::spectrum::STANDARD_BINS];
                let before_changed = live::smooth_spectrum(&mut self.before, &silence, elapsed);
                live::smooth_spectrum(&mut self.after, &silence, elapsed) || before_changed
            } else {
                false
            }
        } else {
            self.before.clear();
            self.after.clear();
            !same_signal
        };
        if self.key == Some(key) && !changed {
            return None;
        }
        self.key = Some(key);
        self.signal = signal.cloned();
        let mut canvas = Canvas::new(WIDTH as u32, HEIGHT as u32, size);
        match mode {
            BEFORE_AFTER_MODE => {
                for (_, x) in live::frequency_ticks() {
                    canvas.rect(x * (WIDTH - 1.0), 0.0, 1.0, HEIGHT, PLOT_GRID);
                }
                level_grid(&mut canvas);
                draw_levels(&mut canvas, &self.before, colors.graph_tint(0.60), 2.7);
                draw_levels(&mut canvas, &self.after, colors.accent, 1.35);
            }
            LOUDNESS_HISTORY_MODE => {
                for division in 0..6 {
                    canvas.rect(WIDTH * division as f32 / 6.0, 0.0, 1.0, HEIGHT, PLOT_GRID);
                }
                level_grid(&mut canvas);
                draw_history(&mut canvas, history, colors.accent, offset as f32 * WIDTH / size.width as f32);
            }
            STEREO_FIELD_MODE => {
                let samples = match signal.map(Arc::as_ref) {
                    Some(Signal::Stereo { samples, .. }) => samples.as_slice(),
                    _ => &[],
                };
                draw_field(&mut canvas, samples, size, colors.accent);
            }
            _ => return None,
        }
        Some(canvas.finish())
    }
}

fn level_grid(canvas: &mut Canvas) {
    for y in [8.0, 34.0, 60.0, 86.0, 112.0] {
        canvas.rect(0.0, y, WIDTH, 1.0, PLOT_GRID);
    }
}

fn draw_levels(canvas: &mut Canvas, levels: &[f32], color: [u8; 3], width: f32) {
    if levels.len() < 2 || levels.iter().all(|level| *level == 0.0) {
        return;
    }
    let mut trace = PathBuilder::new();
    for (index, &level) in levels.iter().enumerate() {
        let (x, y) = (WIDTH * index as f32 / (levels.len() - 1) as f32, 112.0 - 104.0 * level);
        if index == 0 {
            trace.move_to(x, y);
        } else {
            trace.line_to(x, y);
        }
    }
    if let Some(trace) = trace.finish() {
        canvas.stroke(&trace, color, width);
    }
}

fn draw_history(canvas: &mut Canvas, history: &History, color: [u8; 3], offset: f32) {
    let Some((now, _)) = history.points.back() else { return; };
    let mut trace = PathBuilder::new();
    let mut previous: Option<Instant> = None;
    for &(time, db) in &history.points {
        let Some(db) = db else {
            previous = None;
            continue;
        };
        let x = (1.0 - now.duration_since(time).as_secs_f32() / HISTORY_SECS) * WIDTH - offset;
        let y = 112.0 - 104.0 * ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        if previous.is_some_and(|last| time.duration_since(last) <= 2 * HISTORY_INTERVAL) {
            trace.line_to(x, y);
        } else {
            trace.move_to(x, y);
        }
        previous = Some(time);
    }
    if let Some(trace) = trace.finish() {
        canvas.stroke(&trace, color, 1.45);
    }
}

fn draw_field(canvas: &mut Canvas, samples: &[[f32; 2]], size: RasterSize, color: [u8; 3]) {
    let (cx, cy) = (WIDTH / 2.0, HEIGHT / 2.0);
    let dy = 0.44 * HEIGHT;
    let dx = (0.44 * size.height as f32 / size.width as f32 * WIDTH).min(WIDTH * 0.44);
    let mut grid = PathBuilder::new();
    for scale in [0.5, 1.0] {
        grid.move_to(cx - dx * scale, cy);
        grid.line_to(cx, cy - dy * scale);
        grid.line_to(cx + dx * scale, cy);
        grid.line_to(cx, cy + dy * scale);
        grid.close();
    }
    if let Some(grid) = grid.finish() {
        canvas.stroke(&grid, PLOT_GRID, 1.0);
    }
    canvas.rect(cx - dx, cy, dx * 2.0, 1.0, PLOT_ZERO);
    canvas.rect(cx, cy - dy, 1.0, dy * 2.0, PLOT_ZERO);
    let mut trace = PathBuilder::new();
    for (index, &[left, right]) in samples.iter().enumerate() {
        let clean = |value: f32| if value.is_finite() { value.clamp(-1.0, 1.0) } else { 0.0 };
        let (left, right) = (clean(left), clean(right));
        let x = cx + (right - left) * dx * 0.5;
        let y = cy - (left + right) * dy * 0.5;
        if index == 0 {
            trace.move_to(x, y);
        } else {
            trace.line_to(x, y);
        }
    }
    if let Some(trace) = trace.finish() {
        canvas.stroke(&trace, color, 1.2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_field_distinguishes_in_phase_opposite_phase_and_silence() {
        for (right, expected) in [(0.5, Some(1.0)), (-0.5, Some(-1.0)), (0.0, None)] {
            let Signal::Stereo { correlation, .. } = Signal::stereo(vec![[0.5, right]; 32]) else {
                unreachable!();
            };
            assert_eq!(correlation, expected);
        }
    }

    #[test]
    fn history_keeps_a_thirty_second_window_and_marks_disconnects() {
        let mut history = History::default();
        let start = Instant::now();
        for i in 0..=400 {
            history.update(start + HISTORY_INTERVAL * i, Some(0.5));
        }
        assert_eq!(history.points.len(), 301);
        let (time, db) = history.points[0];
        assert_eq!(time, start + Duration::from_secs(10));
        assert!((db.unwrap() + 6.02).abs() < 0.01);
        history.update(start + Duration::from_secs(41), None);
        assert_eq!(history.points.back().unwrap().1, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn comparison_keeps_antiphase_tones_and_measures_actual_gain() {
        let frames: Vec<_> = (0..8192).map(|index| {
            let sample = 0.2 * (std::f32::consts::TAU * 1000.0 * index as f32 / 48000.0).sin();
            [sample, -sample, sample * 2.0, -sample * 2.0]
        }).collect();
        let mut analyzer = crate::spectrum::Analyzer::default();
        let Signal::Comparison { before, after } = Signal::comparison(&frames, 48000, &mut analyzer).unwrap() else {
            unreachable!();
        };
        let peak = |levels: &[f32]| levels.iter().copied().fold(0.0, f32::max);
        assert!(peak(&before) > 0.5);
        assert!(((peak(&after) - peak(&before)) * 66.0 - 6.02).abs() < 0.05);
    }
}
