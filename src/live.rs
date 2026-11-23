use crate::plot_canvas::{Canvas, RasterSize};
use crate::theme::{PLOT_GRID, PLOT_ZERO, ThemeColors};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::time::Duration;
use tiny_skia::PathBuilder;

pub const CAPTURE_INTERVAL: Duration = Duration::from_nanos(8_333_334);
pub const ANALYSIS_INTERVAL: Duration = Duration::from_millis(16);
pub const CONTROL_INTERVAL: Duration = Duration::from_millis(16);
pub const IDLE_INTERVAL: Duration = Duration::from_millis(50);
pub const SILENT_PEAK_DBFS: f32 = -1000.0;
pub const BARS_MODE: i32 = 0;
pub const STEREO_LEVEL_MODE: i32 = 2;
pub const BEFORE_AFTER_MODE: i32 = 3;
pub const LOUDNESS_HISTORY_MODE: i32 = 4;
pub const STEREO_FIELD_MODE: i32 = 5;
const WIDTH: usize = 960;
const HEIGHT: usize = 120;

pub fn display_interval(millihertz: Option<u32>) -> Duration {
    let rate = millihertz.filter(|rate| *rate != 0).unwrap_or(120_000).clamp(30_000, 240_000);
    Duration::from_nanos(1_000_000_000_000_u64.div_ceil(u64::from(rate)))
}

pub fn display_millihertz(window: &slint::Window) -> Option<u32> {
    use slint::winit_030::WinitWindowAccessor;
    window.with_winit_window(|window| window.current_monitor()?.refresh_rate_millihertz()).flatten()
}

pub fn frequency_ticks() -> impl Iterator<Item = (u32, f32)> {
    [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000].into_iter().map(|frequency| {
        let (position, _) = crate::graph::position(f64::from(frequency), 0.0);
        (frequency, position as f32)
    })
}

pub fn amplitude_dbfs(amplitude: f32) -> f32 {
    if amplitude.is_finite() && amplitude > 0.0 {
        20.0 * amplitude.log10()
    } else {
        SILENT_PEAK_DBFS
    }
}

pub fn smooth_spectrum(displayed: &mut Vec<f32>, target: &[f32], elapsed: Duration) -> bool {
    let mut changed = displayed.len() != target.len();
    if displayed.len() != target.len() {
        // A different bin count maps the same index to a different frequency.
        displayed.clear();
        displayed.resize(target.len(), 0.0);
    }
    // Retain the previous 120 Hz response while using the actual elapsed time.
    let blend = -(-elapsed.as_secs_f32() / (-2.0 / (120.0 * 0.55_f32.ln()))).exp_m1();
    for (level, &sample) in displayed.iter_mut().zip(target) {
        let target = sample.max(0.0).min(1.0);
        let difference = target - *level;
        let next = if difference.abs() < 0.002 { target } else { *level + blend * difference };
        changed |= *level != next;
        *level = next;
    }
    changed
}

pub fn smooth_stereo(displayed: &mut [f32; 2], rms: [f32; 2], elapsed: Duration) -> [f32; 2] {
    for (level, amplitude) in displayed.iter_mut().zip(rms) {
        let target = amplitude_dbfs(amplitude).max(-60.0);
        let tau = if target > *level { 0.025 } else { 0.090 };
        let difference = target - *level;
        let blend = -(-elapsed.as_secs_f32() / tau).exp_m1();
        *level = if difference.abs() < 0.05 { target } else { *level + difference * blend };
    }
    std::array::from_fn(|index| if displayed[index] <= -60.0 {
        amplitude_dbfs(rms[index])
    } else {
        displayed[index]
    })
}

#[derive(Default)]
pub struct LiveMeters {
    rendered_mode: Option<i32>,
    rendered_bins: Vec<f32>,
    rendered_waveform: Vec<f32>,
    rendered_size: Option<RasterSize>,
    colors: ThemeColors,
}

impl LiveMeters {
    pub fn set_colors(&mut self, colors: ThemeColors) {
        if self.colors.accent != colors.accent {
            self.colors = colors;
            self.rendered_size = None;
        }
    }

    pub fn frame(
        &mut self,
        mode: i32,
        spectrum: &[f32],
        waveform: &[f32],
        size: RasterSize,
    ) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        let bins = if spectrum.iter().any(|&level| level > 0.0) { spectrum } else { &[] };
        let unchanged = match mode {
            crate::scope::MODE => self.rendered_waveform == waveform,
            _ => self.rendered_bins == bins,
        };
        if self.rendered_mode == Some(mode) && self.rendered_size == Some(size) && unchanged {
            return None;
        }
        self.rendered_mode = Some(mode);
        self.rendered_size = Some(size);
        self.rendered_bins.clear();
        self.rendered_bins.extend_from_slice(bins);
        self.rendered_waveform.clear();
        self.rendered_waveform.extend_from_slice(waveform);
        let mut canvas = Canvas::new(WIDTH as u32, HEIGHT as u32, size);
        match mode {
            BARS_MODE => draw_bars(&mut canvas, bins, size.width as usize, self.colors),
            crate::scope::MODE => draw_waveform(&mut canvas, waveform, self.colors.accent),
            _ => return None,
        }
        Some(canvas.finish())
    }
}

fn draw_waveform(canvas: &mut Canvas, samples: &[f32], color: [u8; 3]) {
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
        canvas.stroke(&path, color, 1.2);
    }
}

fn bar_cells(bins: &[f32], pixel_width: usize) -> impl Iterator<Item = (usize, usize, f32)> + '_ {
    let count = bins.len().min((pixel_width / 2).max(1));
    (0..count).map(move |index| {
        let first = index * bins.len() / count;
        let end = (index + 1) * bins.len() / count;
        let peak = bins[first..end].iter().copied().fold(0.0, f32::max);
        let left = index * pixel_width / count;
        let right = (index + 1) * pixel_width / count;
        (left, right, peak)
    })
}

fn draw_bars(canvas: &mut Canvas, bins: &[f32], pixel_width: usize, colors: ThemeColors) {
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
    let units_per_pixel = WIDTH as f32 / pixel_width as f32;
    let bar_color = colors.graph_tint(0.49);
    for (left, right, bin) in bar_cells(bins, pixel_width) {
        // Snap the bars and their one-pixel gaps to the backing image.
        let start = left as f32 * units_per_pixel;
        let width = (right - left).saturating_sub(1).max(1) as f32 * units_per_pixel;
        let top = level_y(bin);
        if top < BASE {
            canvas.rect(start, top, width, BASE - top, bar_color);
            canvas.rect(start, top, width, (BASE - top).min(1.0), colors.accent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SIZE: RasterSize = RasterSize { width: 1920, height: 240 };

    #[test]
    fn detailed_bars_fit_pixel_width_and_keep_peaks_when_grouped() {
        let mut levels = [0.0; 512];
        levels[255] = 0.9;
        let narrow: Vec<_> = bar_cells(&levels, 640).collect();
        assert_eq!(narrow.len(), 320);
        assert_eq!(narrow.first().unwrap().0, 0);
        assert_eq!(narrow.last().unwrap().1, 640);
        assert_eq!(narrow.iter().filter(|cell| cell.2 == 0.9).count(), 1);
        assert!(narrow.windows(2).all(|adjacent| adjacent[0].1 == adjacent[1].0));
        let wide: Vec<_> = bar_cells(&levels, 1080).collect();
        assert_eq!(wide.len(), levels.len());
        assert_eq!(wide[255].2, 0.9);
    }

    #[test]
    fn resizing_invalidates_a_static_live_frame() {
        let mut meters = LiveMeters::default();
        let size = RasterSize::from_logical(800.0, 300.0, 1.25).unwrap();
        assert!(meters.frame(BARS_MODE, &[], &[], TEST_SIZE).is_some());
        let frame = meters.frame(BARS_MODE, &[], &[], size).unwrap();
        assert_eq!((frame.width(), frame.height()), (1000, 375));
        assert!(meters.frame(BARS_MODE, &[], &[], size).is_none());
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
        assert_eq!(amplitude_dbfs(0.0), SILENT_PEAK_DBFS);
        assert_eq!(amplitude_dbfs(f32::NAN), SILENT_PEAK_DBFS);
        assert_eq!(amplitude_dbfs(1.0), 0.0);
        assert!((amplitude_dbfs(0.5) + 6.0206).abs() < 0.0001);
        assert!(amplitude_dbfs(1.1) > 0.0);
    }

    #[test]
    fn spectrum_moves_between_samples_and_settles_when_silent() {
        let mut bins = Vec::new();
        smooth_spectrum(&mut bins, &[1.0], CAPTURE_INTERVAL);
        let first_frame = bins[0];
        assert!(first_frame > 0.0 && first_frame < 1.0);
        smooth_spectrum(&mut bins, &[1.0], CAPTURE_INTERVAL);
        assert!(bins[0] > first_frame && bins[0] < 1.0);
        assert!((bins[0] - 0.45).abs() < 1e-6);
        for _ in 0..20 {
            smooth_spectrum(&mut bins, &[0.0], CAPTURE_INTERVAL);
        }
        assert_eq!(bins, [0.0]);
        smooth_spectrum(&mut bins, &[1.0], CAPTURE_INTERVAL);
        smooth_spectrum(&mut bins, &[0.0, 1.0], CAPTURE_INTERVAL);
        assert_eq!(bins, [0.0, first_frame]);
    }

    #[test]
    fn the_visual_clock_tracks_display_refresh_and_bounds_fallbacks() {
        for hz in [60, 120, 144, 165, 240] {
            let frames = Duration::from_secs(1).as_secs_f64() / display_interval(Some(hz * 1000)).as_secs_f64();
            assert!((frames - f64::from(hz)).abs() < 0.001);
        }
        assert_eq!(display_interval(Some(0)), display_interval(None));
        assert_eq!(display_interval(Some(360_000)), display_interval(Some(240_000)));
    }

    #[test]
    fn meter_motion_is_consistent_across_refresh_rates() {
        let animate = |hz: u32| {
            let elapsed = display_interval(Some(hz * 1000));
            let mut spectrum = Vec::new();
            let mut stereo = [-60.0; 2];
            for _ in 0..hz / 5 {
                smooth_spectrum(&mut spectrum, &[1.0], elapsed);
                smooth_stereo(&mut stereo, [0.5, 0.25], elapsed);
            }
            (spectrum[0], stereo)
        };
        let (spectrum_a, stereo_a) = animate(60);
        let (spectrum_b, stereo_b) = animate(240);
        assert!((spectrum_a - spectrum_b).abs() < 0.002);
        assert!(stereo_a.into_iter().zip(stereo_b).all(|(a, b)| (a - b).abs() < 0.06));
    }

    #[test]
    fn idle_monitor_only_renders_on_mode_changes() {
        let mut meters = LiveMeters::default();
        assert!(meters.frame(BARS_MODE, &[], &[], TEST_SIZE).is_some());
        assert!(meters.frame(BARS_MODE, &[0.0; 16], &[], TEST_SIZE).is_none());
        assert!(meters.frame(crate::scope::MODE, &[], &[], TEST_SIZE).is_some());
        assert!(meters.frame(BARS_MODE, &[0.3; 16], &[], TEST_SIZE).is_some());
        assert!(meters.frame(BARS_MODE, &[0.3; 16], &[], TEST_SIZE).is_none());
    }

    #[test]
    fn oscilloscope_updates_from_waveform_samples() {
        let mut meters = LiveMeters::default();
        let frame = |meters: &mut LiveMeters, waveform: &[f32]| {
            meters.frame(crate::scope::MODE, &[], waveform, TEST_SIZE)
        };
        let grid = frame(&mut meters, &[]).unwrap();
        let trace = frame(&mut meters, &[-0.5, 0.0, 0.5]).unwrap();
        assert_ne!(grid.as_bytes(), trace.as_bytes());
        assert!(frame(&mut meters, &[-0.5, 0.0, 0.5]).is_none());
        assert!(frame(&mut meters, &[]).is_some());
    }
}
