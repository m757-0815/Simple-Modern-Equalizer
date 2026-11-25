use crate::{
    config::{Band, Config},
    matched, spline,
};
use resonance_dsp::convolution::BLOCK;
use resonance_ipc::{BandState, fr::ResponseGrid};
use rustfft::{FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Minimum,
    Matched,
    Linear,
    Zero,
}

impl Mode {
    pub const OPTIONS: [Self; 4] = [Self::Minimum, Self::Matched, Self::Linear, Self::Zero];

    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn delay_budget_ms(self) -> Option<usize> {
        match self {
            Self::Linear => Some(176),
            Self::Zero => Some(250),
            Self::Minimum | Self::Matched => None,
        }
    }

    pub fn biquad_config(self, cfg: &Config) -> Cow<'_, Config> {
        if matches!(self, Self::Minimum | Self::Matched) {
            return Cow::Borrowed(cfg);
        }
        Cow::Owned(Config {
            bands: Vec::new(),
            spline: false,
            spline_source: None,
            other_mode_bands: None,
            ..cfg.clone()
        })
    }
}

/// An IR request is comparable so unrelated settings never reload the audio buffer.
#[derive(PartialEq)]
pub struct Filter {
    pub mode: Mode,
    pub sample_rate: u32,
    bands: Vec<BandState>,
    spline: Vec<Band>,
}

impl Filter {
    pub fn new(mode: Mode, cfg: &Config, sample_rate: u32) -> Option<Self> {
        if cfg.bypass {
            return None;
        }
        let points = cfg.spline_correction();
        let spline = if points.iter().any(|point| point.gain_db != 0.0) {
            points.into_owned()
        } else {
            Vec::new()
        };
        let bands = if mode == Mode::Minimum {
            if spline.is_empty() {
                return None;
            }
            Vec::new()
        } else {
            cfg.parametric_bands()
                .iter()
                .filter(|band| {
                    band.enabled
                        && band.gain_db != 0.0
                        && band.frequency_hz < f64::from(sample_rate) / 2.0
                })
                .map(Band::state)
                .collect()
        };
        Some(Self {
            mode,
            sample_rate,
            bands,
            spline,
        })
    }

    pub fn wav(&self) -> Result<Vec<u8>, String> {
        if self.mode == Mode::Minimum {
            return Ok(spline::kernel_wav(&self.spline, self.sample_rate));
        }
        let taps = if self.mode == Mode::Matched {
            matched::kernel(&self.bands, &self.spline, self.sample_rate)
        } else {
            self.centered_kernel()?
        };
        if taps.iter().any(|&tap| !(tap as f32).is_finite()) {
            return Err("The EQ curve exceeds the audio engine's impulse response range".into());
        }
        Ok(spline::wav(&taps, self.sample_rate))
    }

    /// Synthesize the combined magnitude with zero phase at the FFT origin.
    /// Move the symmetric response into causal time, reserving one convolution
    /// block so the centered delay and buffering fit the selected budget.
    fn centered_kernel(&self) -> Result<Vec<f64>, String> {
        let budget = self
            .mode
            .delay_budget_ms()
            .expect("centered EQ requires a delay budget");
        let radius = (self.sample_rate as usize * budget / 1000)
            .checked_sub(BLOCK)
            .filter(|&frames| frames > 0)
            .ok_or("Sample rate is too low for the selected EQ phase mode")?;
        let mut taps = vec![0.0; 2 * radius + 1];
        if self.bands.is_empty() && self.spline.is_empty() {
            // Keep the selected delay stable when a preset is flat.
            taps[radius] = 1.0;
            return Ok(taps);
        }

        let size = taps.len().next_power_of_two();
        let rate = f64::from(self.sample_rate);
        let frequencies: Vec<_> = (0..=size / 2)
            .map(|bin| rate * bin as f64 / size as f64)
            .collect();
        let grid = ResponseGrid::new(&frequencies, rate);
        let mut gains = vec![0.0; frequencies.len()];
        for band in &self.bands {
            for (total, gain) in gains.iter_mut().zip(grid.band_response(band)) {
                *total += gain;
            }
        }
        let mut spectrum = vec![Complex::ZERO; size];
        for (bin, (frequency, mut gain)) in frequencies.into_iter().zip(gains).enumerate() {
            if !self.spline.is_empty() {
                gain += spline::gain_at(&self.spline, frequency);
            }
            let magnitude = 10.0_f64.powf(gain / 20.0);
            spectrum[bin].re = magnitude;
            spectrum[(size - bin) % size].re = magnitude;
        }
        FftPlanner::new()
            .plan_fft_inverse(size)
            .process(&mut spectrum);
        taps[radius] = spectrum[0].re / size as f64;
        for offset in 1..radius {
            // The Parzen window has a nonnegative spectrum. Tapering the IR
            // with it averages positive magnitudes without introducing a sign flip.
            let t = offset as f64 / radius as f64;
            let window = if t <= 0.5 {
                1.0 - 6.0 * t * t * (1.0 - t)
            } else {
                2.0 * (1.0 - t).powi(3)
            };
            let tap = f64::midpoint(spectrum[offset].re, spectrum[size - offset].re) * window
                / size as f64;
            taps[radius - offset] = tap;
            taps[radius + offset] = tap;
        }
        Ok(taps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use resonance_dsp::convolution::{ConvolutionEngine, IrData};
    use resonance_ipc::fr::response_for_frequencies;
    use std::{f64::consts::PI, sync::Arc};

    #[test]
    fn both_buffered_modes_keep_the_combined_curve_without_relative_phase_shift() {
        let mut cfg = Config::flat_spline();
        assert!(cfg.add_peak(700.0, 4.0));
        cfg.other_mode_bands = Some(vec![Band {
            frequency_hz: 700.0,
            gain_db: -7.0,
            ..Band::flat(0, 1)
        }]);
        cfg.validate().unwrap();
        for mode in [Mode::Linear, Mode::Zero] {
            let filter = Filter::new(mode, &cfg, 48000).unwrap();
            let taps = filter.centered_kernel().unwrap();
            let center = taps.len() / 2;
            assert!(taps.iter().eq(taps.iter().rev()));
            for frequency in [20.0, 100.0, 700.0, 3000.0, 20000.0] {
                let omega = -2.0 * PI * frequency / 48000.0;
                let response: Complex<f64> = taps
                    .iter()
                    .enumerate()
                    .map(|(index, &tap)| {
                        Complex::from_polar(1.0, omega * (index as f64 - center as f64)) * tap
                    })
                    .sum();
                let gain = 20.0 * response.norm().log10();
                let target = response_for_frequencies(&filter.bands, [frequency], 48000.0)[0]
                    + spline::gain_at(&filter.spline, frequency);
                assert!(
                    response.re > 0.0 && response.im.abs() < 1e-9,
                    "{mode:?} at {frequency} Hz: {response}"
                );
                assert!(
                    (gain - target).abs() < 0.08,
                    "{mode:?} at {frequency} Hz: {gain} vs {target}"
                );
            }
        }
    }

    #[test]
    fn zero_phase_reaches_its_full_delay_through_the_streaming_convolver() {
        for rate in [44100, 48000, 96000, 192000] {
            let rate_hz = f64::from(rate);
            let mut cfg = Config::default();
            let filter = Filter::new(Mode::Zero, &cfg, rate).unwrap();
            let taps = filter.centered_kernel().unwrap();
            let delay = rate as usize / 4;
            let mut convolver = ConvolutionEngine::new(2, rate_hz);
            convolver
                .load_ir(Arc::new(IrData {
                    name: "Zero Phase test".into(),
                    path: String::new(),
                    sample_rate: rate_hz,
                    channels: vec![taps],
                }))
                .unwrap();
            let mut input = vec![0.0; (delay + BLOCK + 1) * 2];
            input[0] = 0.5;
            input[1] = -0.25;
            for chunk in input.chunks_mut(193 * 2) {
                convolver.process(chunk, 2);
            }
            assert!((input[2 * delay] - 0.5).abs() < 1e-9, "{rate} Hz");
            assert!((input[2 * delay + 1] + 0.25).abs() < 1e-9, "{rate} Hz");
            assert!(input[..2 * delay].iter().all(|&sample| sample.abs() < 1e-9));
            cfg.bypass = true;
            assert!(Filter::new(Mode::Zero, &cfg, rate).is_none());
        }
    }
}
