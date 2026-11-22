use crate::plot_canvas::{Canvas, RasterSize};
use crate::theme::{METER_HOT, METER_SCALE, MINT, PLOT_GRID, PLOT_ZERO, SPECTRUM_BAR, SPECTRUM_FILL};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::time::Duration;
use tiny_skia::PathBuilder;

pub const REFRESH_HZ: u64 = 120;
pub const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000_u64.div_ceil(REFRESH_HZ));
pub const SILENT_PEAK_DBFS: f32 = -1000.0;
const WIDTH: usize = 960;
const HEIGHT: usize = 120;

// Two 120 Hz steps retain the same response as one step at 60 Hz.
const SPECTRUM_BLEND: f32 = 0.258_380_14;
const VU_ATTACK: f32 = 0.059_255_615;
const VU_RELEASE: f32 = 0.022_758_985;

pub fn frequency_ticks() -> impl Iterator<Item = (u32, f32)> {
    [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000].into_iter().map(|frequency| {
        let (position, _) = crate::graph::position(f64::from(frequency), 0.0);
        (frequency, position as f32)
    })
}

pub fn sample_peak_dbfs(peak: f32) -> f32 {
    if peak.is_finite() && peak > 0.0 {
        20.0 * peak.log10()
    } else {
        SILENT_PEAK_DBFS
    }
}

pub fn smooth_spectrum(displayed: &mut Vec<f32>, target: &[f32]) {
    displayed.resize(target.len(), 0.0);
    for (level, &sample) in displayed.iter_mut().zip(target) {
        let target = sample.max(0.0).min(1.0);
        let difference = target - *level;
        *level = if difference.abs() < 0.002 { target } else { *level + SPECTRUM_BLEND * difference };
    }
}

#[derive(Default)]
pub struct LiveMeters {
    needle: [f32; 2],
    rendered_mode: Option<i32>,
    rendered_angles: [u16; 2],
    rendered_bins: Vec<f32>,
    rendered_waveform: Vec<f32>,
    rendered_size: Option<RasterSize>,
}

impl LiveMeters {
    /// The meter uses an RMS reference of -18 dBFS for 0 VU.
    fn vu(rms: f32) -> f32 {
        if rms.is_finite() && rms > 0.0 { 20.0 * rms.log10() + 18.0 } else { -80.0 }
    }

    pub fn frame(
        &mut self,
        mode: i32,
        channels: [f32; 2],
        spectrum: &[f32],
        waveform: &[f32],
        size: RasterSize,
    ) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        for (current, rms) in self.needle.iter_mut().zip(channels) {
            let target = (Self::vu(rms).clamp(-20.0, 3.0) + 20.0) / 23.0;
            *current += (target - *current) * if target > *current { VU_ATTACK } else { VU_RELEASE };
            if *current < 0.001 { *current = 0.0; }
        }
        let angles = self.needle.map(|value| (value * 200.0).round() as u16);
        let bins = if spectrum.iter().any(|&level| level > 0.0) { spectrum } else { &[] };
        let unchanged = match mode {
            0 => self.rendered_angles == angles,
            crate::scope::MODE => self.rendered_waveform == waveform,
            _ => self.rendered_bins == bins,
        };
        if self.rendered_mode == Some(mode) && self.rendered_size == Some(size) && unchanged {
            return None;
        }
        self.rendered_mode = Some(mode);
        self.rendered_size = Some(size);
        self.rendered_angles = angles;
        self.rendered_bins.clear();
        self.rendered_bins.extend_from_slice(bins);
        self.rendered_waveform.clear();
        self.rendered_waveform.extend_from_slice(waveform);
        let mut canvas = Canvas::new(WIDTH as u32, HEIGHT as u32, size);
        match mode {
            1 => draw_spectrum(&mut canvas, bins, false),
            2 => draw_spectrum(&mut canvas, bins, true),
            crate::scope::MODE => draw_waveform(&mut canvas, waveform),
            _ => {
                draw_meter(&mut canvas, 240.0, f32::from(angles[0]) / 200.0);
                draw_meter(&mut canvas, 720.0, f32::from(angles[1]) / 200.0);
                canvas.rect(479.5, 12.0, 1.0, 98.0, PLOT_GRID);
            }
        }
        Some(canvas.finish())
    }
}

fn draw_waveform(canvas: &mut Canvas, samples: &[f32]) {
    let width = WIDTH as f32;
    let middle = HEIGHT as f32 / 2.0;
    let amplitude = middle - 8.0;
    for division in 1..5 {
        canvas.rect(division as f32 * width / 5.0, 0.0, 1.0, HEIGHT as f32, PLOT_GRID);
    }
    for level in [-1.0, -0.5, 0.0, 0.5, 1.0] {
        let y = middle - amplitude * level;
        canvas.rect(0.0, y, width, 1.0, if level == 0.0 { PLOT_ZERO } else { PLOT_GRID });
    }
    let Some((&first, rest)) = samples.split_first() else { return; };
    let y = |sample: f32| middle - amplitude * if sample.is_finite() { sample.clamp(-1.0, 1.0) } else { 0.0 };
    let mut path = PathBuilder::new();
    path.move_to(0.0, y(first));
    for (index, &sample) in rest.iter().enumerate() {
        path.line_to((index + 1) as f32 * width / rest.len() as f32, y(sample));
    }
    if let Some(path) = path.finish() {
        canvas.stroke(&path, MINT, 1.2);
    }
}

fn draw_meter(canvas: &mut Canvas, center_x: f32, needle: f32) {
    const PIVOT_Y: f32 = 110.0;
    const ZERO: f32 = 20.0 / 23.0;
    let polar = |radius: f32, level: f32| {
        let angle = (level - 0.5) * 2.1;
        (center_x + angle.sin() * radius, PIVOT_Y - angle.cos() * radius)
    };
    for (start, end, color) in [(0.0, ZERO, METER_SCALE), (ZERO, 1.0, METER_HOT)] {
        let mut arc = PathBuilder::new();
        let (x, y) = polar(91.0, start);
        arc.move_to(x, y);
        for step in 1..=48 {
            let (x, y) = polar(91.0, start + (end - start) * step as f32 / 48.0);
            arc.line_to(x, y);
        }
        canvas.stroke(&arc.finish().expect("meter arc"), color, 1.0);
    }
    for vu in [-20, -15, -10, -7, -5, -3, 0, 1, 2, 3] {
        let level = (vu + 20) as f32 / 23.0;
        let color = if vu >= 0 { METER_HOT } else { METER_SCALE };
        let (x0, y0) = polar(92.0, level);
        let (x1, y1) = polar(if vu == 0 { 102.0 } else { 97.0 }, level);
        let mut tick = PathBuilder::new();
        tick.move_to(x0, y0);
        tick.line_to(x1, y1);
        canvas.stroke(&tick.finish().expect("meter tick"), color, 1.0);
    }
    let mut pointer = PathBuilder::new();
    pointer.move_to(center_x, PIVOT_Y);
    let (x, y) = polar(83.0, needle);
    pointer.line_to(x, y);
    canvas.stroke(&pointer.finish().expect("meter needle"), MINT, 1.7);
    canvas.fill(&PathBuilder::from_circle(center_x, PIVOT_Y, 3.0).expect("meter pivot"), MINT);
}

fn draw_spectrum(canvas: &mut Canvas, bins: &[f32], bars: bool) {
    const BASE: f32 = 117.0;
    const RANGE: f32 = 109.0;
    for y in [8.0, 35.0, 62.0, 90.0, BASE] {
        canvas.rect(0.0, y, WIDTH as f32, 1.0, PLOT_GRID);
    }
    for (_, position) in frequency_ticks() {
        let x = (position * (WIDTH - 1) as f32).round();
        canvas.rect(x, 8.0, 1.0, BASE - 8.0, PLOT_GRID);
    }
    if bins.is_empty() { return; }
    let level_y = |value: f32| BASE - value.clamp(0.0, 1.0) * RANGE;
    if bars {
        for (index, &bin) in bins.iter().enumerate() {
            let start = index as f32 * WIDTH as f32 / bins.len() as f32;
            let end = (index + 1) as f32 * WIDTH as f32 / bins.len() as f32;
            let gutter = ((end - start) * 0.18).min(5.0);
            let top = level_y(bin);
            if top < BASE {
                canvas.rect(start + gutter, top, end - start - 2.0 * gutter, BASE - top, SPECTRUM_BAR);
                canvas.rect(start + gutter, top, end - start - 2.0 * gutter, (BASE - top).min(1.0), MINT);
            }
        }
        return;
    }

    let mut outline = PathBuilder::new();
    let width = WIDTH as f32;
    let points = bins.len() as f32;
    outline.move_to(0.0, level_y(bins[0]));
    for (index, &bin) in bins.iter().enumerate() {
        outline.line_to((index as f32 + 0.5) * width / points, level_y(bin));
    }
    outline.line_to(width, level_y(bins[bins.len() - 1]));
    let mut fill = outline.clone();
    fill.line_to(width, BASE);
    fill.line_to(0.0, BASE);
    fill.close();
    if let Some(path) = fill.finish() {
        canvas.fill(&path, SPECTRUM_FILL);
    }
    if let Some(path) = outline.finish() {
        canvas.stroke(&path, MINT, 1.2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SIZE: RasterSize = RasterSize { width: 1920, height: 240 };

    #[test]
    fn resizing_invalidates_a_static_live_frame() {
        let mut meters = LiveMeters::default();
        let size = RasterSize::from_logical(800.0, 300.0, 1.25).unwrap();
        assert!(meters.frame(1, [0.0; 2], &[], &[], TEST_SIZE).is_some());
        let frame = meters.frame(1, [0.0; 2], &[], &[], size).unwrap();
        assert_eq!((frame.width(), frame.height()), (1000, 375));
        assert!(meters.frame(1, [0.0; 2], &[], &[], size).is_none());
    }

    #[test]
    fn live_frequency_ticks_span_the_logarithmic_audio_range() {
        let ticks = frequency_ticks().collect::<Vec<_>>();
        assert_eq!(ticks.len(), 10);
        assert_eq!(ticks.first(), Some(&(20, 0.0)));
        assert_eq!(ticks.last(), Some(&(20000, 1.0)));
        assert!((ticks[3].1 - 1.0 / 3.0).abs() < 0.00001);
        assert!(ticks.windows(2).all(|pair| pair[0].1 < pair[1].1));
    }

    #[test]
    fn sample_peak_uses_full_scale_as_its_reference() {
        assert_eq!(sample_peak_dbfs(0.0), SILENT_PEAK_DBFS);
        assert_eq!(sample_peak_dbfs(f32::NAN), SILENT_PEAK_DBFS);
        assert_eq!(sample_peak_dbfs(1.0), 0.0);
        assert!((sample_peak_dbfs(0.5) + 6.0206).abs() < 0.0001);
        assert!(sample_peak_dbfs(1.1) > 0.0);
    }

    #[test]
    fn spectrum_moves_between_samples_and_settles_when_silent() {
        let mut bins = Vec::new();
        smooth_spectrum(&mut bins, &[1.0]);
        let first_frame = bins[0];
        assert!(first_frame > 0.0 && first_frame < 1.0);
        smooth_spectrum(&mut bins, &[1.0]);
        assert!(bins[0] > first_frame && bins[0] < 1.0);
        assert!((bins[0] - 0.45).abs() < 1e-6);
        for _ in 0..20 {
            smooth_spectrum(&mut bins, &[0.0]);
        }
        assert_eq!(bins, [0.0]);
    }

    #[test]
    fn vu_reference_and_channel_independence() {
        let reference = 10.0_f32.powf(-18.0 / 20.0);
        assert!(LiveMeters::vu(reference).abs() < 0.01);
        let mut meters = LiveMeters::default();
        assert!(meters.frame(0, [reference, 0.0], &[], &[], TEST_SIZE).is_some());
        assert!(meters.needle[0] > 0.0);
        assert_eq!(meters.needle[1], 0.0);
        meters.frame(0, [reference, 0.0], &[], &[], TEST_SIZE);
        let peak = meters.needle[0];
        assert!((peak - 0.115 * 20.0 / 23.0).abs() < 1e-6);
        for _ in 0..2 {
            meters.frame(0, [0.0; 2], &[], &[], TEST_SIZE);
        }
        assert!((meters.needle[0] - peak * 0.955).abs() < 1e-6);
    }

    #[test]
    fn idle_monitor_only_renders_on_mode_changes() {
        let mut meters = LiveMeters::default();
        assert!(meters.frame(0, [0.0; 2], &[], &[], TEST_SIZE).is_some());
        assert!(meters.frame(0, [0.0; 2], &[0.0; 16], &[], TEST_SIZE).is_none());
        assert!(meters.frame(1, [0.0; 2], &[], &[], TEST_SIZE).is_some());
        assert!(meters.frame(2, [0.0; 2], &[], &[], TEST_SIZE).is_some());
        assert!(meters.frame(2, [0.0; 2], &[0.3; 16], &[], TEST_SIZE).is_some());
        assert!(meters.frame(2, [0.0; 2], &[0.3; 16], &[], TEST_SIZE).is_none());
    }

    #[test]
    fn oscilloscope_updates_from_waveform_samples() {
        let mut meters = LiveMeters::default();
        let frame = |meters: &mut LiveMeters, waveform: &[f32]| {
            meters.frame(crate::scope::MODE, [0.0; 2], &[], waveform, TEST_SIZE)
        };
        let grid = frame(&mut meters, &[]).unwrap();
        let trace = frame(&mut meters, &[-0.5, 0.0, 0.5]).unwrap();
        assert_ne!(grid.as_bytes(), trace.as_bytes());
        assert!(frame(&mut meters, &[-0.5, 0.0, 0.5]).is_none());
        assert!(frame(&mut meters, &[]).is_some());
    }
}
