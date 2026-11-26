use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use std::sync::Arc;

pub const STANDARD_BINS: usize = 128;
const DETAILED_BINS: usize = 512;
const FFT_LENGTH: usize = 8192;
const MIN_DB: f32 = -66.0;

// #######################################################
// choose the spectrum detail for each view
// #######################################################

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Standard,
    Detailed,
}

impl Resolution {
    fn bins(self) -> usize {
        match self {
            Self::Standard => STANDARD_BINS,
            Self::Detailed => DETAILED_BINS,
        }
    }
}

pub struct Levels {
    values: [f32; DETAILED_BINS],
    resolution: Resolution,
}

impl Levels {
    pub fn silent(resolution: Resolution) -> Self {
        Self { values: [0.0; DETAILED_BINS], resolution }
    }

    pub fn as_slice(&self) -> &[f32] {
        &self.values[..self.resolution.bins()]
    }
}

// #######################################################
// transform audio into logarithmic frequency bins
// #######################################################

pub struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    window_sum: f32,
    samples: Vec<Complex32>,
    scratch: Vec<Complex32>,
    frequency_edges: [f64; DETAILED_BINS + 1],
    layout: Option<(f64, Resolution)>,
    bins: Vec<Bin>,
    magnitudes: Vec<f32>,
}

struct Bin {
    left: usize,
    between: f32,
    start: usize,
    end: usize,
}

impl Default for Analyzer {
    fn default() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_LENGTH);
        let window: Vec<_> = (0..FFT_LENGTH)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_LENGTH as f32).cos())
            .collect();
        Self {
            scratch: vec![Complex32::default(); fft.get_inplace_scratch_len()],
            samples: vec![Complex32::default(); FFT_LENGTH],
            fft,
            window_sum: window.iter().sum(),
            window,
            frequency_edges: std::array::from_fn(|edge| 20.0 * 1000.0_f64.powf(edge as f64 / DETAILED_BINS as f64)),
            layout: None,
            bins: Vec::new(),
            magnitudes: Vec::new(),
        }
    }
}

impl Analyzer {
    pub fn levels(&mut self, samples: &[f32], rate: f64, resolution: Resolution) -> Option<Levels> {
        if !rate.is_finite() || rate <= 40.0 || samples.is_empty() {
            return None;
        }
        let available = samples.len().min(FFT_LENGTH);
        let samples = &samples[samples.len() - available..];
        let pad = (FFT_LENGTH - available) / 2;
        let (leading, rest) = self.samples.split_at_mut(pad);
        let (active, trailing) = rest.split_at_mut(available);
        leading.fill(Complex32::default());
        trailing.fill(Complex32::default());
        let weights = &self.window[pad..pad + available];
        for ((value, &sample), &weight) in active.iter_mut().zip(samples).zip(weights) {
            let real = if sample.is_finite() { sample * weight } else { 0.0 };
            *value = Complex32::new(real, 0.0);
        }
        let window_sum = if available == FFT_LENGTH { self.window_sum } else { weights.iter().sum() };
        self.fft.process_with_scratch(&mut self.samples, &mut self.scratch);

        if self.layout != Some((rate, resolution)) {
            self.set_layout(rate, resolution);
        }
        for (magnitude, sample) in self.magnitudes.iter_mut().zip(&self.samples) {
            *magnitude = sample.norm();
        }
        let mut values = [0.0; DETAILED_BINS];
        for (value, bin) in values.iter_mut().zip(&self.bins) {
            let interpolated = self.magnitudes[bin.left] * (1.0 - bin.between)
                + self.magnitudes[bin.left + 1] * bin.between;
            let peak = self.magnitudes[bin.start..bin.end].iter().copied().fold(interpolated, f32::max);
            let db = 20.0 * (2.0 * peak / window_sum).max(1e-12).log10();
            *value = ((db - MIN_DB) / -MIN_DB).clamp(0.0, 1.0);
        }
        Some(Levels { values, resolution })
    }

    pub fn stereo_levels(
        &mut self,
        frames: impl Iterator<Item = [f32; 2]> + Clone,
        rate: f64,
        resolution: Resolution,
    ) -> Option<Levels> {
        // Analyze ears separately so out-of-phase signals still show up.
        let mut samples: Vec<_> = frames.clone().map(|[left, _]| left).collect();
        let mut levels = self.levels(&samples, rate, resolution)?;
        samples.clear();
        samples.extend(frames.map(|[_, right]| right));
        let right = self.levels(&samples, rate, resolution)?;
        for (combined, &level) in levels.values.iter_mut().zip(right.as_slice()) {
            *combined = combined.max(level);
        }
        Some(levels)
    }

    fn set_layout(&mut self, rate: f64, resolution: Resolution) {
        // Match the graph's logarithmic frequency axis while keeping narrow peaks visible.
        let count = resolution.bins();
        let step = DETAILED_BINS / count;
        let indices_per_hz = FFT_LENGTH as f64 / rate;
        let fft_index = |edge: usize| self.frequency_edges[edge * step] * indices_per_hz;
        self.bins.clear();
        for band in 0..count {
            let low = fft_index(band);
            if low >= FFT_LENGTH as f64 / 2.0 { break; }
            let high = fft_index(band + 1).min(FFT_LENGTH as f64 / 2.0);
            let center = (low * high).sqrt();
            self.bins.push(Bin {
                left: center.floor() as usize,
                between: center.fract() as f32,
                start: low.ceil() as usize,
                end: high.floor() as usize + 1,
            });
        }
        let magnitude_count = self.bins.last().map_or(0, |bin| bin.end.max(bin.left + 2));
        self.magnitudes.resize(magnitude_count, 0.0);
        self.layout = Some((rate, resolution));
    }
}

// #######################################################
// find prominent frequency ranges for the EQ overlays
// #######################################################

/// Group the strongest neighboring bands in a recent output spectrum.
pub fn dominant_regions(levels: &[f32]) -> Vec<(f32, f32)> {
    let strongest = levels.iter().copied().fold(0.0, f32::max);
    if strongest < 0.25 { return Vec::new(); }
    let threshold = (strongest - 10.0 / -MIN_DB).max(0.25);
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < levels.len() {
        if levels[index] < threshold {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < levels.len() && levels[index] >= threshold { index += 1; }
        ranges.push((start as f32 / levels.len() as f32, index as f32 / levels.len() as f32));
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_resolutions_locate_a_tone_on_the_same_frequency_axis() {
        let tone: Vec<_> = (0..FFT_LENGTH).map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin() * 0.5).collect();
        let mut analyzer = Analyzer::default();
        for (resolution, count) in [(Resolution::Standard, 128), (Resolution::Detailed, 512)] {
            let levels = analyzer.levels(&tone, 48_000.0, resolution).unwrap();
            let levels = levels.as_slice();
            assert_eq!(levels.len(), count);
            let peak = levels.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
            let expected = ((1000.0_f64 / 20.0).log10() / 3.0 * count as f64).floor() as usize;
            assert!(peak.abs_diff(expected) <= 1);
            assert_eq!(dominant_regions(levels).len(), 1);
        }
        assert!(dominant_regions(&[0.0; STANDARD_BINS]).is_empty());
    }

    #[test]
    fn stereo_overlay_finds_a_tone_that_cancels_in_the_mono_mix() {
        let frames = (0..FFT_LENGTH).map(|index| {
            let sample = (std::f32::consts::TAU * 1000.0 * index as f32 / 48_000.0).sin();
            [sample, -sample]
        });
        let levels = Analyzer::default().stereo_levels(frames, 48_000.0, Resolution::Standard).unwrap();
        let regions = dominant_regions(levels.as_slice());
        let tone_position = (1000.0_f32 / 20.0).log10() / 3.0;
        assert!(regions.iter().any(|&(start, end)| start <= tone_position && tone_position <= end));
    }
}
