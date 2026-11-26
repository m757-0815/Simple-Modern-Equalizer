use crate::{config::Band, spline};
use resonance_dsp::convolution::BLOCK;
use resonance_ipc::{BandState, BandType};
use rustfft::{FftPlanner, num_complex::Complex};
use std::f64::consts::PI;

const LOOKAHEAD_MS: usize = 6;

fn lookahead_frames(sample_rate: u32) -> usize {
    (sample_rate as usize * LOOKAHEAD_MS / 1000).max(1)
}

pub fn latency_ms(sample_rate: u32) -> f64 {
    1000.0 * (lookahead_frames(sample_rate) + BLOCK) as f64 / f64::from(sample_rate)
}

// #######################################################
// model the analog response of each band
// #######################################################

struct Prototype {
    center: f64,
    prewarp: f64,
    numerator: [f64; 3],
    denominator: [f64; 3],
}

impl Prototype {
    fn new(band: &BandState, sample_rate: f64) -> Option<Self> {
        if !band.enabled || band.gain_db == 0.0 || band.freq >= sample_rate / 2.0 {
            return None;
        }
        // Normalized analog prototypes of the same biquads used in Minimum Phase.
        let a = 10.0_f64.powf(band.gain_db / 40.0);
        let damping = a.sqrt() / band.q;
        let (numerator, denominator) = match band.band_type {
            BandType::Peaking => ([1.0, a / band.q, 1.0], [1.0, 1.0 / (a * band.q), 1.0]),
            BandType::LowShelf => ([a * a, a * damping, a], [1.0, damping, a]),
            BandType::HighShelf => ([a, a * damping, a * a], [a, damping, 1.0]),
            _ => return None,
        };
        Some(Self {
            center: band.freq,
            prewarp: (PI * band.freq / sample_rate).tan(),
            numerator,
            denominator,
        })
    }

    fn response(&self, frequency_ratio: f64) -> Complex<f64> {
        let evaluate = |[constant, linear, quadratic]: [f64; 3]| {
            Complex::new(
                constant - quadratic * frequency_ratio * frequency_ratio,
                linear * frequency_ratio,
            )
        };
        evaluate(self.numerator) / evaluate(self.denominator)
    }

    fn analog(&self, frequency: f64) -> Complex<f64> {
        self.response(frequency / self.center)
    }
}

/// Analog magnitude target, sampled by the graph and headroom estimate.
pub fn response_db(
    bands: &[Band],
    frequencies: impl IntoIterator<Item = f64>,
    sample_rate: f64,
) -> Vec<f64> {
    let prototypes: Vec<_> = bands
        .iter()
        .filter_map(|band| Prototype::new(&band.state(), sample_rate))
        .collect();
    frequencies
        .into_iter()
        .map(|frequency| {
            prototypes
                .iter()
                .map(|filter| 20.0 * filter.analog(frequency).norm().log10())
                .sum()
        })
        .collect()
}

// #######################################################
// build the short matched-phase correction
// #######################################################

pub fn kernel(bands: &[BandState], spline_points: &[Band], sample_rate: u32) -> Vec<f64> {
    let rate = f64::from(sample_rate);
    let radius = lookahead_frames(sample_rate);
    let mut correction = vec![0.0; 2 * radius + 1];
    let prototypes: Vec<_> = bands
        .iter()
        .filter_map(|band| Prototype::new(band, rate))
        .collect();
    if prototypes.is_empty() {
        correction[radius] = 1.0;
    } else {
        let size = (8 * radius + 1).next_power_of_two().max(2048);
        let mut spectrum = vec![Complex::ZERO; size];
        for bin in 0..=size / 2 {
            let normalized = bin as f64 / (size / 2) as f64;
            let frequency = normalized * rate / 2.0;
            let warped = (PI * frequency / rate).tan();
            // A real filter's Nyquist bin must be real. Ease out the phase
            // correction over the last 5% of the band, keeping its magnitude.
            let t = ((1.0 - normalized) / 0.05).clamp(0.0, 1.0);
            let phase_weight = t * t * (3.0 - 2.0 * t);
            let ratio = prototypes
                .iter()
                .map(|filter| {
                    let ratio = filter.analog(frequency) / filter.response(warped / filter.prewarp);
                    Complex::from_polar(ratio.norm(), ratio.arg() * phase_weight)
                })
                .product::<Complex<f64>>();
            spectrum[bin] = ratio;
            if bin > 0 && bin < size / 2 {
                spectrum[size - bin] = ratio.conj();
            }
        }
        FftPlanner::new()
            .plan_fft_inverse(size)
            .process(&mut spectrum);
        for (index, tap) in correction.iter_mut().enumerate() {
            let offset = index as isize - radius as isize;
            let bin = offset.rem_euclid(size as isize) as usize;
            let t = ((offset.unsigned_abs() as f64 / radius as f64 - 0.75) * 4.0).max(0.0);
            let window = 0.5 * (1.0 + (PI * t).cos());
            *tap = spectrum[bin].re * window / size as f64;
        }
    }
    if spline_points.is_empty() {
        correction
    } else {
        cascade(&correction, &spline::kernel(spline_points, sample_rate))
    }
}

/// Combine the correction and the existing minimum-phase spline into one IR.
fn cascade(first: &[f64], second: &[f64]) -> Vec<f64> {
    let length = first.len() + second.len() - 1;
    let size = length.next_power_of_two();
    let mut planner = FftPlanner::new();
    let forward = planner.plan_fft_forward(size);
    let transform = |taps: &[f64]| {
        let mut spectrum = vec![Complex::ZERO; size];
        for (bin, &tap) in spectrum.iter_mut().zip(taps) {
            bin.re = tap;
        }
        forward.process(&mut spectrum);
        spectrum
    };
    let mut spectrum = transform(first);
    for (bin, coefficient) in spectrum.iter_mut().zip(transform(second)) {
        *bin *= coefficient;
    }
    planner.plan_fft_inverse(size).process(&mut spectrum);
    spectrum
        .into_iter()
        .take(length)
        .map(|bin| bin.re / size as f64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use resonance_dsp::{
        convolution::{ConvolutionEngine, IrData},
        filter::BiquadCoeffs,
    };
    use std::sync::Arc;

    fn digital(c: &BiquadCoeffs, omega: f64) -> Complex<f64> {
        let z = Complex::from_polar(1.0, -omega);
        (c.b0 + z * c.b1 + z * z * c.b2) / (1.0 + z * c.a1 + z * z * c.a2)
    }

    fn response(taps: &[f64], omega: f64, delay: usize) -> Complex<f64> {
        taps.iter()
            .enumerate()
            .map(|(index, &tap)| {
                tap * Complex::from_polar(1.0, -omega * (index as f64 - delay as f64))
            })
            .sum()
    }

    #[test]
    fn correction_tracks_analog_gain_and_phase_for_peak_and_shelves() {
        for rate in [44_100, 48_000, 96_000] {
            let hz = f64::from(rate);
            for kind in ["PK", "LS", "HS"] {
                let band = Band {
                    kind: kind.into(),
                    frequency_hz: 15_000.0,
                    gain_db: 10.0,
                    q: 1.15,
                    ..Band::flat(0, 1)
                };
                let biquad = match kind {
                    "PK" => BiquadCoeffs::peaking,
                    "LS" => BiquadCoeffs::low_shelf,
                    _ => BiquadCoeffs::high_shelf,
                };
                let native = biquad(band.frequency_hz, band.gain_db, band.q, hz).unwrap();
                let correction = kernel(&[band.state()], &[], rate);
                for frequency in [100.0, 8_000.0, 13_000.0, 15_000.0, 19_000.0] {
                    let omega = 2.0 * PI * frequency / hz;
                    let warped = 2.0
                        * (frequency / band.frequency_hz * (PI * band.frequency_hz / hz).tan())
                            .atan();
                    let target = digital(&native, warped);
                    let ratio = digital(&native, omega)
                        * response(&correction, omega, lookahead_frames(rate))
                        / target;
                    let gain_error = 20.0 * ratio.norm().log10();
                    let phase_error = ratio.arg().to_degrees();
                    assert!(
                        gain_error.abs() < 0.25 && phase_error.abs() < 1.0,
                        "{kind} at {frequency} Hz ({rate}): {gain_error} dB, {phase_error}°"
                    );
                    let graph = response_db(std::slice::from_ref(&band), [frequency], hz)[0];
                    assert!((graph - 20.0 * target.norm().log10()).abs() < 1e-10);
                }
            }
        }
    }

    #[test]
    fn spline_keeps_its_minimum_phase_response_with_the_matched_delay() {
        let mut cfg = Config::flat_spline();
        assert!(cfg.add_peak(150.0, 8.0));
        let rate = 48_000;
        let expected = spline::kernel(&cfg.bands, rate);
        let ir = kernel(&[], &cfg.bands, rate);
        let center = lookahead_frames(rate);
        for (i, &tap) in expected.iter().enumerate() {
            assert!((tap - ir[i + center]).abs() < 1e-10);
        }
        let mut engine = ConvolutionEngine::new(1, f64::from(rate));
        engine
            .load_ir(Arc::new(IrData {
                name: "Matched Phase delay test".into(),
                path: String::new(),
                sample_rate: f64::from(rate),
                channels: vec![kernel(&[], &[], rate)],
            }))
            .unwrap();
        let delay = center + BLOCK;
        let mut pulse = vec![0.0; delay + 129];
        pulse[0] = 1.0;
        for chunk in pulse.chunks_mut(127) {
            engine.process(chunk, 1);
        }
        assert!((pulse[delay] - 1.0).abs() < 1e-10);
        assert!(pulse[..delay].iter().all(|&sample| sample.abs() < 1e-10));
        assert!((latency_ms(rate) - 11.333).abs() < 0.001);
    }
}
