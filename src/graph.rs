use crate::config::{Band, Config};
use crate::plot_canvas::{Canvas, RasterSize};
use crate::theme::{MINT, PLOT_GRID, PLOT_ZERO, SPECTRUM_FILL};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use tiny_skia::PathBuilder;

const MAX_GAIN_DB: f64 = 24.0;
const W: usize = 1920;
const H: usize = 260;

pub fn position(frequency_hz: f64, gain_db: f64) -> (f64, f64) {
    (
        (frequency_hz / 20.0).log10() / 3.0,
        (MAX_GAIN_DB - gain_db) / (2.0 * MAX_GAIN_DB),
    )
}

pub fn band_values(x: f64, y: f64) -> (f64, f64) {
    let frequency_hz = 20.0 * 1000.0_f64.powf(x.clamp(0.0, 1.0));
    let gain_db = MAX_GAIN_DB * (1.0 - 2.0 * y.clamp(0.0, 1.0));
    ((frequency_hz * 10.0).round() / 10.0, (gain_db * 10.0).round() / 10.0)
}

#[derive(Clone, Copy, PartialEq)]
pub struct Dot {
    pub x: f64,
    pub y: f64,
    pub enabled: bool,
}

pub fn drag_gain(start_gain: f64, vertical_delta: f64, shelf: bool) -> f64 {
    // At its corner frequency a shelf moves the curve by half its gain.
    let sensitivity = if shelf { 2.0 } else { 1.0 };
    let gain = start_gain - vertical_delta * (2.0 * MAX_GAIN_DB) * sensitivity;
    (gain.clamp(-MAX_GAIN_DB, MAX_GAIN_DB) * 10.0).round() / 10.0
}

pub fn drag_values(frequency: f64, gain: f64, dx: f64, dy: f64, shelf: bool) -> (f64, f64) {
    let moved_frequency = if dx == 0.0 { frequency } else { band_values(position(frequency, 0.0).0 + dx, 0.5).0 };
    let moved_gain = if dy == 0.0 { gain } else { drag_gain(gain, dy, shelf) };
    (moved_frequency, moved_gain)
}

pub fn hit_band(dots: &[Dot], x: f64, y: f64, width: f64, height: f64) -> Option<usize> {
    if width <= 0.0 || height <= 0.0 || ![x, y, width, height].into_iter().all(f64::is_finite) {
        return None;
    }
    dots.iter().enumerate().rev().filter_map(|(index, dot)| {
        let distance = ((x - dot.x) * width).hypot((y - dot.y) * height);
        (distance <= 13.0).then_some((index, distance))
    }).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(index, _)| index)
}

#[derive(Default)]
pub struct GraphCache {
    bands: Vec<Band>,
    bypass: bool,
    preamp_db: f64,
    sample_rate: f64,
    curve: Vec<f32>,
    rendered_spectrum: Vec<f32>,
    rendered_size: Option<RasterSize>,
    pub dots: Vec<Dot>,
}

impl GraphCache {
    pub fn update_dots(&mut self, cfg: &Config) -> bool {
        if self.curve.is_empty() {
            return false;
        }
        let dots: Vec<_> = cfg.bands.iter().map(|band| {
            let x = position(band.frequency_hz, 0.0).0.clamp(0.0, 1.0);
            let pixel = x * (W - 1) as f64;
            let left = pixel.floor() as usize;
            let right = (left + 1).min(W - 1);
            let from = self.curve[left] as f64;
            let to = self.curve[right] as f64;
            let y = (from + (to - from) * pixel.fract() + 0.5) / H as f64;
            Dot { x, y, enabled: band.enabled && !cfg.bypass && band.frequency_hz < self.sample_rate / 2.0 }
        }).collect();
        if self.dots == dots {
            return false;
        }
        self.dots = dots;
        true
    }

    pub fn frame(&mut self, cfg: &Config, spectrum: &[f32], sample_rate: f64, size: RasterSize) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        let rate = if sample_rate.is_finite() && sample_rate > 0.0 { sample_rate } else { 48000.0 };
        let changed = self.curve.is_empty() || self.bypass != cfg.bypass ||
            (!cfg.bypass && (self.bands != cfg.bands || self.preamp_db != cfg.preamp_db || self.sample_rate != rate));
        if changed {
            let levels = if cfg.bypass {
                vec![0.0; W]
            } else {
                let bands: Vec<_> = cfg.bands.iter().map(Band::state).collect();
                let frequencies = (0..W).map(|x| (20.0 * 1000.0_f64.powf(x as f64 / (W - 1) as f64)).min(rate / 2.0));
                resonance_ipc::fr::response_for_frequencies(&bands, frequencies, rate)
            };
            let preamp = if cfg.bypass { 0.0 } else { cfg.preamp_db };
            self.curve = levels.into_iter().map(|db| {
                let y = (MAX_GAIN_DB - (db + preamp).clamp(-MAX_GAIN_DB, MAX_GAIN_DB)) / (2.0 * MAX_GAIN_DB);
                (y * (H - 1) as f64) as f32
            }).collect();
            self.bands.clone_from(&cfg.bands);
            self.bypass = cfg.bypass;
            self.preamp_db = cfg.preamp_db;
            self.sample_rate = rate;
        }
        let visible = if spectrum.iter().any(|&level| level > 0.0) { spectrum } else { &[] };
        if !changed && visible == self.rendered_spectrum && self.rendered_size == Some(size) {
            return None;
        }
        self.rendered_spectrum.clear();
        self.rendered_spectrum.extend_from_slice(visible);
        self.rendered_size = Some(size);
        Some(render(&self.curve, visible, size))
    }
}

fn render(curve: &[f32], spectrum: &[f32], size: RasterSize) -> SharedPixelBuffer<Rgba8Pixel> {
    let mut canvas = Canvas::new(W as u32, H as u32, size);
    for (i, value) in spectrum.iter().enumerate() {
        let height = value.clamp(0.0, 1.0) * (H - 18) as f32;
        let start = i as f32 * W as f32 / spectrum.len() as f32;
        let end = (i + 1) as f32 * W as f32 / spectrum.len() as f32;
        let gutter = (end - start) * 0.15;
        canvas.rect(start + gutter, H as f32 - height, end - start - 2.0 * gutter, height, SPECTRUM_FILL);
    }
    for db in [-24.0, -18.0, -12.0, -6.0, 0.0, 6.0, 12.0, 18.0, 24.0] {
        let (_, normalized_y) = position(20.0, db);
        let y = (normalized_y * (H - 1) as f64).round() as f32;
        canvas.rect(0.0, y, W as f32, 1.0, if db == 0.0 { PLOT_ZERO } else { PLOT_GRID });
    }
    for f in [20.0_f64, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0] {
        let (normalized_x, _) = position(f, 0.0);
        let x = (normalized_x * (W - 1) as f64).round() as f32;
        canvas.rect(x, 0.0, 1.0, H as f32, PLOT_GRID);
    }
    let mut path = PathBuilder::new();
    path.move_to(0.5, curve[0] + 0.5);
    for (x, &y) in curve.iter().enumerate().skip(1) {
        path.line_to(x as f32 + 0.5, y + 0.5);
    }
    if let Some(path) = path.finish() {
        canvas.stroke(&path, MINT, 2.0);
    }
    canvas.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SIZE: RasterSize = RasterSize { width: W as u32, height: H as u32 };

    #[test]
    fn resizing_rerasterizes_the_eq_without_recalculating_its_response() {
        let mut cache = GraphCache::default();
        let config = Config::default();
        let original = cache.frame(&config, &[], 48000.0, TEST_SIZE).unwrap();
        let curve = cache.curve.as_ptr();
        let size = RasterSize::from_logical(1000.0, 320.0, 1.5).unwrap();
        let resized = cache.frame(&config, &[], 48000.0, size).unwrap();
        assert_eq!((original.width(), original.height()), (W as u32, H as u32));
        assert_eq!((resized.width(), resized.height()), (1500, 480));
        assert_eq!(cache.curve.as_ptr(), curve);
        assert!(cache.frame(&config, &[], 48000.0, size).is_none());
    }

    #[test]
    fn response_stroke_blends_fractional_pixel_edges() {
        let image = render(&vec![119.75; W], &[], TEST_SIZE);
        let pixel = |y: usize| {
            let offset = (y * W + 8) * 4;
            &image.as_bytes()[offset..offset + 3]
        };
        let background = crate::theme::PLOT_BACKGROUND;
        assert_eq!(pixel(120), MINT);
        let edge = pixel(119);
        assert!(edge[1] > background[1] && edge[1] < MINT[1]);
    }

    #[test]
    fn drag_mapping_round_trips_and_clamps() {
        let (x, y) = position(1200.0, -9.5);
        assert_eq!(band_values(x, y), (1200.0, -9.5));
        assert_eq!(band_values(-0.2, 1.2), (20.0, -24.0));
        assert_eq!(band_values(1.2, -0.2), (20000.0, 24.0));
    }

    #[test]
    fn one_axis_drag_preserves_the_other_parameter_at_full_precision() {
        let (frequency, gain) = (1000.222, 3.873);
        assert_eq!(drag_values(frequency, gain, 0.0, -0.04, false).0, frequency);
        assert_eq!(drag_values(frequency, gain, 0.12, 0.0, false).1, gain);
        assert_eq!(drag_values(frequency, gain, 0.0, 0.0, false), (frequency, gain));
    }

    #[test]
    fn graph_does_not_reflect_the_curve_past_nyquist() {
        let mut cfg = Config::default();
        cfg.bands[1].frequency_hz = 5000.0;
        cfg.bands[1].gain_db = 10.0;
        let mut cache = GraphCache::default();
        cache.frame(&cfg, &[], 16000.0, TEST_SIZE);
        cache.update_dots(&cfg);
        assert!(cache.dots[1].enabled);
        assert!(!cache.dots.last().unwrap().enabled);
        assert!(cache.curve[W - 70..].iter().all(|y| *y == cache.curve[W - 1]));
    }

    #[test]
    fn graph_cache_skips_idle_frames_and_reacts_to_each_visible_change() {
        let mut cache = GraphCache::default();
        let mut cfg = Config::default();
        assert!(cache.frame(&cfg, &[], 48000.0, TEST_SIZE).is_some());
        assert!(cache.frame(&cfg, &[0.0; 16], 48000.0, TEST_SIZE).is_none());
        cfg.name = "Renamed only".into();
        assert!(cache.frame(&cfg, &[], 48000.0, TEST_SIZE).is_none());
        let curve = cache.curve.clone();
        cfg.show_live_view = false;
        assert!(cache.frame(&cfg, &[0.2; 16], 48000.0, TEST_SIZE).is_some());
        assert_eq!(cache.curve, curve);
        assert!(cache.frame(&cfg, &[0.2; 16], 48000.0, TEST_SIZE).is_none());
        cfg.preamp_db = -3.0;
        assert!(cache.frame(&cfg, &[], 48000.0, TEST_SIZE).is_some());
        assert_ne!(cache.curve, curve);
        cfg.bypass = true;
        assert!(cache.frame(&cfg, &[], 48000.0, TEST_SIZE).is_some());
        assert_eq!(cache.curve, curve);
    }

    #[test]
    fn pick_clears_invalid_bounds_and_uses_topmost_overlapping_dot() {
        let mut cfg = Config::default();
        cfg.bands[1] = cfg.bands[0].clone();
        let mut cache = GraphCache::default();
        cache.frame(&cfg, &[], 48000.0, TEST_SIZE);
        cache.update_dots(&cfg);
        let Dot { x, y, .. } = cache.dots[0];
        assert_eq!(hit_band(&cache.dots, x, y, 400.0, 180.0), Some(1));
        assert_eq!(hit_band(&cache.dots, x, y, 0.0, 180.0), None);
        assert_eq!(hit_band(&cache.dots, f64::NAN, y, 400.0, 180.0), None);
    }

    #[test]
    fn handles_follow_the_combined_curve_and_preamp() {
        let mut cfg = Config::default();
        cfg.bands[1] = cfg.bands[0].clone();
        cfg.bands[0].gain_db = 4.0;
        cfg.bands[1].gain_db = 3.0;
        cfg.preamp_db = -5.0;
        let mut cache = GraphCache::default();
        cache.frame(&cfg, &[], 48000.0, TEST_SIZE);
        assert!(cache.update_dots(&cfg));
        let y = cache.dots[0].y;
        assert!((y - position(cfg.bands[0].frequency_hz, 2.0).1).abs() < 0.01);
        assert_eq!(y, cache.dots[1].y);
        cfg.bypass = true;
        cache.frame(&cfg, &[], 48000.0, TEST_SIZE);
        assert!(cache.update_dots(&cfg));
        assert!((cache.dots[0].y - 0.5).abs() < 0.01);
        assert!(!cache.dots[0].enabled);
    }

    #[test]
    fn horizontal_drags_keep_gain_and_shelf_drags_compensate_for_the_corner() {
        assert_eq!(drag_gain(4.0, 0.0, true), 4.0);
        assert_eq!(drag_gain(4.0, -0.125, false), 10.0);
        assert_eq!(drag_gain(4.0, -0.125, true), 16.0);
        assert_eq!(drag_gain(4.0, -1.0, true), 24.0);
    }
}
