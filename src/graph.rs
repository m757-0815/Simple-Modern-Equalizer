use crate::config::{Band, Config, SplineSource};
use crate::plot_canvas::{Canvas, RasterSize};
use crate::theme::{PLOT_GRID, PLOT_ZERO, ThemeColors};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use tiny_skia::{Path, PathBuilder};

const MAX_GAIN_DB: f64 = 24.0;
const W: usize = 1920;
const H: usize = 260;

#[derive(Clone, Copy, PartialEq)]
pub struct GainAxis {
    pub bottom: f64,
    pub top: f64,
}

impl Default for GainAxis {
    fn default() -> Self { Self { bottom: -MAX_GAIN_DB, top: MAX_GAIN_DB } }
}

impl GainAxis {
    pub fn span(self) -> f64 { self.top - self.bottom }

    pub fn y(self, gain: f64) -> f64 { ((self.top - gain) / self.span()).clamp(0.0, 1.0) }
}

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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dot {
    pub x: f64,
    pub y: f64,
    pub enabled: bool,
}

#[derive(Clone, Copy)]
pub struct BandDrag {
    pub index: usize,
    frequency: f64,
    gain: f64,
    q: f64,
    gain_span: f64,
}

impl BandDrag {
    pub fn new(index: usize, band: &Band) -> Self {
        Self { index, frequency: band.frequency_hz, gain: band.gain_db, q: band.q, gain_span: 2.0 * MAX_GAIN_DB }
    }

    pub fn with_gain_span(mut self, span: f64) -> Self {
        self.gain_span = span;
        self
    }

    pub fn update(&mut self, cfg: &mut Config, dx: f64, dy: f64) -> bool {
        let Some(current) = cfg.bands.get(self.index) else { return false; };
        let mut moved = current.clone();
        (moved.frequency_hz, moved.gain_db) = drag_values(self.frequency, self.gain, dx, dy, self.gain_span);
        if cfg.spline {
            if cfg.locked_point(self.index) || (moved.frequency_hz != current.frequency_hz
                && !cfg.can_place_spline_point(moved.frequency_hz, Some(self.index)))
            {
                moved.frequency_hz = current.frequency_hz;
            }
        } else if moved.kind == "PK" && moved.frequency_hz != current.frequency_hz {
            moved.q = if moved.frequency_hz == self.frequency {
                self.q
            } else {
                cfg.automatic_peak_q(self.index, moved.frequency_hz).unwrap_or(self.q)
            };
        }
        if moved == *current { return false; }
        self.index = cfg.replace_band(self.index, moved);
        true
    }
}

fn drag_values(frequency: f64, gain: f64, dx: f64, dy: f64, gain_span: f64) -> (f64, f64) {
    let (x, _) = position(frequency, gain);
    let (moved_frequency, _) = band_values(x + dx, 0.0);
    let moved_gain = rounded_gain(gain - dy * gain_span);
    let moved_frequency = if dx == 0.0 { frequency } else { moved_frequency };
    let moved_gain = if dy == 0.0 { gain } else { moved_gain };
    (moved_frequency, moved_gain)
}

fn rounded_gain(gain: f64) -> f64 {
    (gain.clamp(-MAX_GAIN_DB, MAX_GAIN_DB) * 10.0).round() / 10.0
}

pub fn hit_band(dots: &[Dot], x: f64, y: f64, width: f64, height: f64) -> Option<usize> {
    if width <= 0.0 || height <= 0.0 || ![x, y, width, height].into_iter().all(f64::is_finite) {
        return None;
    }
    dots.iter().enumerate().rev().filter_map(|(index, dot)| {
        let distance = ((x - dot.x) * width).hypot((y - dot.y.clamp(0.0, 1.0)) * height);
        (distance <= 13.0).then_some((index, distance))
    }).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(index, _)| index)
}

#[derive(Default)]
pub struct GraphCache {
    bands: Vec<Band>,
    bypass: bool,
    spline: bool,
    spline_source: Option<SplineSource>,
    other_mode_bands: Option<Vec<Band>>,
    output_offset: f64,
    sample_rate: f64,
    pub axis: GainAxis,
    curve: Vec<f32>,
    rendered_spectrum: Vec<f32>,
    rendered_size: Option<RasterSize>,
    colors: ThemeColors,
    pub dots: Vec<Dot>,
}

impl GraphCache {
    pub fn set_colors(&mut self, colors: ThemeColors) {
        if self.colors.accent != colors.accent {
            self.colors = colors;
            self.rendered_size = None;
        }
    }

    pub fn output_bounds(&self) -> (f64, f64) {
        (self.axis.bottom + self.output_offset, self.axis.top + self.output_offset)
    }

    pub fn zero_position(&self) -> f64 {
        (self.axis.top + self.output_offset) / self.axis.span()
    }

    pub fn update_dots(&mut self, cfg: &Config) -> bool {
        if self.sample_rate == 0.0 {
            return false;
        }
        let dots: Vec<_> = cfg.bands.iter().map(|band| {
            let (x, _) = position(band.frequency_hz, band.gain_db);
            let y = if cfg.spline && !cfg.bypass && !self.curve.is_empty() {
                // The sampled curve includes both editors' filters. The saved
                // spline gain is relative to the filters at conversion time.
                let sample = (x * W as f64 - 0.5).clamp(0.0, (self.curve.len() - 1) as f64);
                let left = sample.floor() as usize;
                let right = sample.ceil() as usize;
                let a = f64::from(self.curve[left]);
                let b = f64::from(self.curve[right]);
                (a + (b - a) * sample.fract() + 0.5) / H as f64
            } else {
                (self.axis.top - band.gain_db) / self.axis.span()
            };
            Dot { x, y, enabled: !cfg.bypass && band.enabled }
        }).collect();
        if self.dots == dots {
            return false;
        }
        self.dots = dots;
        true
    }

    pub fn frame(&mut self, cfg: &Config, spectrum: &[f32], sample_rate: f64, size: RasterSize) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        let rate = if sample_rate.is_finite() && sample_rate > 0.0 { sample_rate } else { 48000.0 };
        let preamp = if cfg.bypass { 0.0 } else { cfg.preamp_db };
        let direct_points = cfg.spline_points().filter(|_| !cfg.bypass
            && cfg.spline_source.is_none() && cfg.parametric_bands().is_empty());
        // Keep EQ geometry in band-gain coordinates; the scale includes effective preamp.
        let changed = self.sample_rate == 0.0 || self.bypass != cfg.bypass || self.spline != cfg.spline ||
            self.bands != cfg.bands || self.spline_source != cfg.spline_source ||
            self.other_mode_bands != cfg.other_mode_bands ||
            self.output_offset != preamp || (!cfg.bypass && self.sample_rate != rate);
        if changed {
            self.output_offset = preamp;
            let levels = if direct_points.is_some() {
                Vec::new()
            } else if cfg.bypass {
                vec![0.0; W]
            } else {
                let frequencies = (0..W).map(|x| (20.0 * 1000.0_f64.powf(x as f64 / (W - 1) as f64)).min(rate / 2.0));
                cfg.response(frequencies, rate)
            };
            self.curve = levels.into_iter().map(|db| {
                let y = self.axis.y(db);
                (y * (H - 1) as f64) as f32
            }).collect();
            self.bands.clone_from(&cfg.bands);
            self.spline_source.clone_from(&cfg.spline_source);
            self.other_mode_bands.clone_from(&cfg.other_mode_bands);
            self.bypass = cfg.bypass;
            self.spline = cfg.spline;
            self.sample_rate = rate;
        }
        let visible = if spectrum.iter().any(|&level| level > 0.0) { spectrum } else { &[] };
        if !changed && visible == self.rendered_spectrum && self.rendered_size == Some(size) {
            return None;
        }
        self.rendered_spectrum.clear();
        self.rendered_spectrum.extend_from_slice(visible);
        self.rendered_size = Some(size);
        let path = if let Some(points) = direct_points {
            band_spline(points, self.axis)
        } else {
            response_path(&self.curve)
        };
        Some(render(path.as_ref(), self.axis, self.output_offset, cfg.bypass, visible, size, self.colors))
    }
}

fn band_spline(bands: &[Band], axis: GainAxis) -> Option<Path> {
    let mut points = bands.iter().map(|band| {
        let (x, _) = position(band.frequency_hz, band.gain_db);
        // Clip the finished curve at the viewport, preserving its shape near the edges.
        let y = (axis.top - band.gain_db) / axis.span();
        ((x * W as f64) as f32, (y * H as f64) as f32)
    });
    let (mut x, mut y) = points.next()?;
    let mut path = PathBuilder::new();
    path.move_to(x, y);
    for (next_x, next_y) in points {
        // Horizontal tangents join the segments smoothly without overshoot.
        // Each segment depends only on its two endpoints.
        let third = (next_x - x) / 3.0;
        path.cubic_to(x + third, y, next_x - third, next_y, next_x, next_y);
        (x, y) = (next_x, next_y);
    }
    path.finish()
}

fn response_path(curve: &[f32]) -> Option<Path> {
    let mut path = PathBuilder::new();
    path.move_to(0.5, *curve.first()? + 0.5);
    for (x, &y) in curve.iter().enumerate().skip(1) {
        path.line_to(x as f32 + 0.5, y + 0.5);
    }
    path.finish()
}

fn render(curve: Option<&Path>, axis: GainAxis, offset: f64, bypass: bool, spectrum: &[f32], size: RasterSize, colors: ThemeColors) -> SharedPixelBuffer<Rgba8Pixel> {
    let mut canvas = Canvas::new(W as u32, H as u32, size);
    let spectrum_fill = colors.graph_tint(0.17);
    for (i, value) in spectrum.iter().enumerate() {
        let height = value.clamp(0.0, 1.0) * (H - 18) as f32;
        let start = i as f32 * W as f32 / spectrum.len() as f32;
        let end = (i + 1) as f32 * W as f32 / spectrum.len() as f32;
        let gutter = (end - start) * 0.15;
        canvas.rect(start + gutter, H as f32 - height, end - start - 2.0 * gutter, height, spectrum_fill);
    }
    let step = if axis.span() > 60.0 { 12.0 } else { 6.0 };
    let first = ((axis.bottom + offset) / step).ceil() as i32;
    let last = ((axis.top + offset) / step).floor() as i32;
    for index in first..=last {
        let db = f64::from(index) * step;
        let y = (axis.y(db - offset) * (H - 1) as f64).round() as f32;
        canvas.rect(0.0, y, W as f32, 1.0, if index == 0 { PLOT_ZERO } else { PLOT_GRID });
    }
    for f in [20.0_f64, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0] {
        let (normalized_x, _) = position(f, 0.0);
        let x = (normalized_x * (W - 1) as f64).round() as f32;
        canvas.rect(x, 0.0, 1.0, H as f32, PLOT_GRID);
    }
    if let Some(path) = curve {
        canvas.stroke(path, if bypass { [134, 134, 142] } else { colors.accent }, 2.0);
    }
    canvas.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spline_drag_locks_endpoint_frequencies_and_preserves_other_points() {
        let mut config = Config::flat_spline();
        assert!(config.add_peak(100.0, 2.0));
        assert!(config.add_peak(1000.0, -3.0));
        let mut left = BandDrag::new(0, &config.bands[0]);
        assert!(left.update(&mut config, 0.3, -0.1));
        assert_eq!(config.bands[0].frequency_hz, 20.0);
        let untouched = [config.bands[0].clone(), config.bands[2].clone(), config.bands[3].clone()];
        let mut drag = BandDrag::new(1, &config.bands[1]);
        let too_close = (980.0_f64 / 100.0).log10() / 3.0;
        assert!(drag.update(&mut config, too_close, 0.1));
        assert_eq!(config.bands[1].frequency_hz, 100.0);
        assert!(config.bands[1].gain_db < 2.0);
        assert!(drag.update(&mut config, 0.5, 0.1));
        assert_eq!(drag.index, 2);
        assert_eq!([&config.bands[0], &config.bands[1], &config.bands[3]], untouched.each_ref());
        assert!(config.validate().is_ok());
    }

    const TEST_SIZE: RasterSize = RasterSize { width: W as u32, height: H as u32 };

    #[test]
    fn switching_to_spline_preserves_the_parametric_curve() {
        let mut config = Config::default();
        config.bands[0].kind = "LS".into();
        config.bands[0].gain_db = 8.0;
        config.bands[2].gain_db = -12.0;
        config.bands[2].q = 20.0;
        config.bands[4].kind = "HS".into();
        config.bands[4].gain_db = 6.0;
        config.preamp_db = -5.0;
        let mut cache = GraphCache::default();
        let before = cache.frame(&config, &[], 48000.0, TEST_SIZE).unwrap();
        cache.update_dots(&config);
        assert_eq!(cache.dots.len(), config.bands.len());
        assert_eq!(hit_band(&cache.dots, cache.dots[2].x, cache.dots[2].y, W as f64, H as f64), Some(2));
        config.convert_to_spline(48000.0);
        config.validate().unwrap();
        let after = cache.frame(&config, &[], 48000.0, TEST_SIZE).unwrap();
        assert_eq!(before.as_bytes(), after.as_bytes());
        config.bands[3].gain_db += 2.0;
        let edited = cache.frame(&config, &[], 48000.0, TEST_SIZE).unwrap();
        assert_ne!(edited.as_bytes(), after.as_bytes());
        cache.update_dots(&config);
        let low_handle = cache.dots[0];
        config.convert_to_parametric();
        assert_eq!(config.bands.len(), 5);
        Config::from_json(&serde_json::to_string(&config).unwrap()).unwrap();
        let restored = cache.frame(&config, &[], 48000.0, TEST_SIZE).unwrap();
        assert_eq!(edited.as_bytes(), restored.as_bytes());

        config.bands[0].gain_db -= 2.0;
        let edited = cache.frame(&config, &[], 48000.0, TEST_SIZE).unwrap();
        config.convert_to_spline(48000.0);
        let restored = cache.frame(&config, &[], 48000.0, TEST_SIZE).unwrap();
        assert_eq!(edited.as_bytes(), restored.as_bytes());
        cache.update_dots(&config);
        assert_eq!(cache.dots[0].x, low_handle.x);
        assert!(cache.dots[0].y > low_handle.y + 0.02);
        for dot in &cache.dots {
            let x = ((dot.x * W as f64).floor() as usize).min(W - 1);
            let y = ((dot.y * H as f64).floor() as usize).min(H - 1);
            assert!((y.saturating_sub(1)..=(y + 1).min(H - 1)).any(|row| {
                let offset = 4 * (row * W + x);
                restored.as_bytes()[offset..offset + 3] == cache.colors.accent
            }), "spline handle at ({x}, {y}) is off the response curve");
        }
    }

    #[test]
    fn switching_a_spline_only_preset_preserves_its_curve() {
        let mut config = Config::flat_spline();
        config.bands[0].gain_db = 6.0;
        config.bands[1].gain_db = -5.0;
        let mut cache = GraphCache::default();
        let before = cache.frame(&config, &[], 48000.0, TEST_SIZE).unwrap();
        config.convert_to_parametric();
        let after = cache.frame(&config, &[], 48000.0, TEST_SIZE).unwrap();
        assert_eq!(before.as_bytes(), after.as_bytes());
    }

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
        let colors = ThemeColors::default();
        let path = response_path(&vec![119.75; W]).unwrap();
        let image = render(Some(&path), GainAxis::default(), 0.0, false, &[], TEST_SIZE, colors);
        let pixel = |y: usize| {
            let offset = (y * W + 8) * 4;
            &image.as_bytes()[offset..offset + 3]
        };
        let background = crate::theme::PLOT_BACKGROUND;
        assert_eq!(pixel(120), colors.accent);
        let edge = pixel(119);
        assert!(edge[1] > background[1] && edge[1] < colors.accent[1]);
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
        assert_eq!(drag_values(frequency, gain, 0.0, -0.04, 48.0).0, frequency);
        assert_eq!(drag_values(frequency, gain, 0.12, 0.0, 48.0).1, gain);
        assert_eq!(drag_values(frequency, gain, 0.0, 0.0, 48.0), (frequency, gain));
    }

    #[test]
    fn graph_does_not_reflect_the_curve_past_nyquist() {
        let mut cfg = Config::default();
        cfg.bands[1].frequency_hz = 5000.0;
        cfg.bands[1].gain_db = 10.0;
        let mut cache = GraphCache::default();
        cache.frame(&cfg, &[], 16000.0, TEST_SIZE);
        cache.update_dots(&cfg);
        assert_eq!(cache.dots.len(), cfg.bands.len());
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
        assert_eq!(cache.curve, curve);
        assert_eq!(cache.output_bounds(), (-27.0, 21.0));
        cfg.bypass = true;
        assert!(cache.frame(&cfg, &[], 48000.0, TEST_SIZE).is_some());
        assert_eq!(cache.curve, curve);
    }

    #[test]
    fn pick_clears_invalid_bounds_and_uses_topmost_overlapping_dot() {
        let (x, y) = (0.4, 0.5);
        let dots = [Dot { x, y, enabled: true }; 2];
        assert_eq!(hit_band(&dots, x, y, 400.0, 180.0), Some(1));
        assert_eq!(hit_band(&dots, x, y, 0.0, 180.0), None);
        assert_eq!(hit_band(&dots, f64::NAN, y, 400.0, 180.0), None);
    }

    #[test]
    fn moving_the_middle_band_keeps_the_outer_handles_fixed() {
        let mut cfg = Config::flat_spline();
        assert!(cfg.add_peak(750.0, 0.0));
        let mut cache = GraphCache::default();
        cache.frame(&cfg, &[], 48000.0, TEST_SIZE);
        assert!(cache.update_dots(&cfg));
        let original = cache.dots.clone();
        let mut drag = BandDrag::new(1, &cfg.bands[1]);
        assert!(drag.update(&mut cfg, 0.01, -0.15));
        cache.frame(&cfg, &[], 48000.0, TEST_SIZE);
        assert!(cache.update_dots(&cfg));
        for index in [0, 2] {
            let dot = cache.dots[index];
            assert_eq!((dot.x, dot.y), (original[index].x, original[index].y));
        }
        assert_ne!(cache.dots[1].y, original[1].y);
        cfg.bypass = true;
        cache.frame(&cfg, &[], 48000.0, TEST_SIZE);
        assert!(cache.update_dots(&cfg));
        assert!(!cache.dots[0].enabled);
    }

    #[test]
    fn spline_hits_every_dot_and_only_changes_near_the_moved_point() {
        let mut cfg = Config::default();
        let axis = GainAxis::default();
        let original = band_spline(&cfg.bands, axis).unwrap();
        let mut drag = BandDrag::new(1, &cfg.bands[1]);
        assert!(drag.update(&mut cfg, 0.012, -0.15));
        let moved = band_spline(&cfg.bands, axis).unwrap();
        assert_ne!(&original.points()[..7], &moved.points()[..7]);
        assert_eq!(&original.points()[6..], &moved.points()[6..]);
        for (index, band) in cfg.bands.iter().enumerate() {
            let (x, y) = position(band.frequency_hz, band.gain_db);
            let knot = moved.points()[index * 3];
            assert_eq!(knot.x, (x * W as f64) as f32);
            assert_eq!(knot.y, (y * H as f64) as f32);
        }
    }

    #[test]
    fn manual_preamp_moves_the_scale_and_keeps_all_spline_points_visible() {
        let mut cfg = Config::flat_spline();
        cfg.bands[0].gain_db = -24.0;
        cfg.bands[1].gain_db = 24.0;
        assert!(cfg.add_peak(1000.0, 6.0));
        let mut cache = GraphCache::default();
        cache.frame(&cfg, &[], 48000.0, TEST_SIZE).unwrap();
        cache.update_dots(&cfg);
        let original = cache.dots.clone();
        for preamp in [-12.0, 12.0, 0.0] {
            cfg.preamp_db = preamp;
            cache.frame(&cfg, &[], 48000.0, TEST_SIZE).unwrap();
            assert!(!cache.update_dots(&cfg));
            assert_eq!(cache.dots, original);
            assert_eq!(cache.output_bounds(), (preamp - 24.0, preamp + 24.0));
            assert_eq!(cache.zero_position(), (24.0 + preamp) / 48.0);
            for (dot, band) in cache.dots.iter().zip(&cfg.bands) {
                assert_eq!(band_values(dot.x, dot.y), (band.frequency_hz, band.gain_db));
            }
        }
    }

    #[test]
    fn auto_headroom_changes_output_labels_without_moving_other_points() {
        let mut cfg = Config::flat_spline();
        cfg.preamp_db = 10.0;
        assert!(cfg.add_peak(1000.0, 0.0));
        let mut cache = GraphCache::default();
        let mut draw = |cfg: &Config| {
            let (effective, _) = crate::listening::prepare(cfg, None, true, 48000.0, 2);
            cache.frame(&effective, &[], 48000.0, TEST_SIZE);
            cache.update_dots(&effective);
            (cache.dots.clone(), cache.output_bounds(), effective)
        };
        let (previous, previous_bounds, _) = draw(&cfg);
        cfg.bands[1].gain_db = 24.0;
        let (next, next_bounds, effective) = draw(&cfg);
        assert_eq!(previous[0], next[0]);
        assert_eq!(previous[2], next[2]);
        assert!(next[1].y < previous[1].y);
        assert!(next_bounds.1 < previous_bounds.1);
        let plotted_gain = next_bounds.1 - next[1].y * (next_bounds.1 - next_bounds.0);
        assert!((plotted_gain - (24.0 + effective.preamp_db)).abs() < 1e-12);
    }

    #[test]
    fn horizontal_peak_drag_changes_only_its_width_and_frequency() {
        let mut cfg = Config::default();
        cfg.bands.truncate(3);
        for (band, frequency) in cfg.bands.iter_mut().zip([500.0, 1000.0, 2000.0]) {
            band.frequency_hz = frequency;
        }
        cfg.bands[1].gain_db = 5.25;
        cfg.bands[1].q = 0.84;
        let original = cfg.bands.clone();
        let mut drag = BandDrag::new(1, &cfg.bands[1]);
        let dx = |frequency| position(frequency, 0.0).0 - position(1000.0, 0.0).0;
        assert!(drag.update(&mut cfg, dx(1250.0), 0.0));
        assert_eq!(cfg.bands[1].q, 2.11);
        assert_eq!(cfg.bands[1].gain_db, 5.25);
        assert!(drag.update(&mut cfg, dx(2001.0), 0.0));
        assert_eq!(drag.index, 2);
        assert_eq!(cfg.bands[2].q, 20.0);
        assert_eq!(cfg.bands[..2], [original[0].clone(), original[2].clone()]);
        assert!(drag.update(&mut cfg, 0.0, -0.125));
        assert_eq!(drag.index, 1);
        assert_eq!(cfg.bands[1].q, 0.84);
    }

    #[test]
    fn vertical_drags_shelves_and_isolated_peaks_keep_their_width() {
        let mut cfg = Config::default();
        cfg.bands[0].kind = "LS".into();
        cfg.bands[0].q = 0.7;
        let mut drag = BandDrag::new(0, &cfg.bands[0]);
        assert!(drag.update(&mut cfg, 0.1, -0.125));
        let moved = &cfg.bands[drag.index];
        assert_eq!((moved.q, moved.gain_db), (0.7, 6.0));
        let mut drag = BandDrag::new(2, &cfg.bands[2]);
        let q = cfg.bands[2].q;
        assert!(drag.update(&mut cfg, 0.0, -0.125));
        assert_eq!(cfg.bands[2].q, q);
        cfg.bands = vec![cfg.bands[2].clone()];
        let mut drag = BandDrag::new(0, &cfg.bands[0]);
        assert!(drag.update(&mut cfg, 0.1, 0.0));
        assert_eq!(cfg.bands[0].q, q);
    }
}
