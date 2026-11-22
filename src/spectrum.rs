use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use std::sync::Arc;

pub const BINS: usize = 128;
const FFT_LENGTH: usize = 8192;
const MIN_DB: f32 = -66.0;

pub struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    samples: Vec<Complex32>,
    scratch: Vec<Complex32>,
}

impl Default for Analyzer {
    fn default() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_LENGTH);
        let window = (0..FFT_LENGTH)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_LENGTH as f32).cos())
            .collect();
        Self {
            scratch: vec![Complex32::default(); fft.get_inplace_scratch_len()],
            samples: vec![Complex32::default(); FFT_LENGTH],
            fft,
            window,
        }
    }
}

impl Analyzer {
    pub fn levels(&mut self, samples: &[f32], rate: f64) -> Option<[f32; BINS]> {
        if !rate.is_finite() || rate <= 40.0 || samples.is_empty() {
            return None;
        }
        let available = samples.len().min(FFT_LENGTH);
        let samples = &samples[samples.len() - available..];
        let pad = (FFT_LENGTH - available) / 2;
        self.samples.fill(Complex32::default());
        let mut window_sum = 0.0;
        for (i, &sample) in samples.iter().enumerate() {
            let weight = self.window[pad + i];
            self.samples[pad + i].re = if sample.is_finite() { sample * weight } else { 0.0 };
            window_sum += weight;
        }
        self.fft.process_with_scratch(&mut self.samples, &mut self.scratch);

        // Match the graph's logarithmic frequency axis while keeping narrow peaks visible.
        let fft_index = |edge: usize| 20.0 * 1000.0_f64.powf(edge as f64 / BINS as f64) * FFT_LENGTH as f64 / rate;
        Some(std::array::from_fn(|band| {
            let low = fft_index(band);
            if low >= FFT_LENGTH as f64 / 2.0 { return 0.0; }
            let high = fft_index(band + 1).min(FFT_LENGTH as f64 / 2.0);
            let center = (low * high).sqrt();
            let left = center.floor() as usize;
            let between = center.fract() as f32;
            let interpolated = self.samples[left].norm() * (1.0 - between) + self.samples[left + 1].norm() * between;
            let peak = (low.ceil() as usize..=high.floor() as usize)
                .map(|bin| self.samples[bin].norm())
                .fold(interpolated, f32::max);
            let db = 20.0 * (2.0 * peak / window_sum).max(1e-12).log10();
            ((db - MIN_DB) / -MIN_DB).clamp(0.0, 1.0)
        }))
    }
}

/// Highlight nearby dominant bands from the output spectrum at a clip event.
/// Digital clipping affects the full waveform, so this is an EQ tuning aid.
pub fn clip_regions(levels: &[f32]) -> Vec<(f32, f32)> {
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
    fn analyzer_locates_a_tone_in_128_logarithmic_bands() {
        let tone: Vec<_> = (0..FFT_LENGTH).map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin() * 0.5).collect();
        let levels = Analyzer::default().levels(&tone, 48_000.0).unwrap();
        let peak = levels.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        let expected = ((1000.0_f64 / 20.0).log10() / 3.0 * BINS as f64).floor() as usize;
        assert!(peak.abs_diff(expected) <= 1);
        assert_eq!(clip_regions(&levels).len(), 1);
        assert!(clip_regions(&[0.0; BINS]).is_empty());
    }
}
