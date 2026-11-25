use crate::config::Band;
use rustfft::{FftPlanner, num_complex::Complex};

// A 20 px handle needs this fraction of the log-frequency graph at minimum window width.
pub const MIN_POINT_GAP: f64 = 0.028;

pub fn points_are_spaced(first: f64, second: f64) -> bool {
    (second / first).log10().abs() / 3.0 + 1e-12 >= MIN_POINT_GAP
}

pub fn gain_at(points: &[Band], frequency: f64) -> f64 {
    let right = points.partition_point(|point| point.frequency_hz <= frequency);
    match right {
        0 => points[0].gain_db,
        right if right == points.len() => points[right - 1].gain_db,
        right => {
            let (left, right) = (&points[right - 1], &points[right]);
            let t = (frequency / left.frequency_hz).ln() / (right.frequency_hz / left.frequency_hz).ln();
            // The graph's cubic Bézier uses these same horizontal end tangents.
            left.gain_db + (right.gain_db - left.gain_db) * (t * t * (3.0 - 2.0 * t))
        }
    }
}

pub fn kernel_wav(points: &[Band], sample_rate: u32) -> Vec<u8> {
    wav(&kernel(points, sample_rate), sample_rate)
}

pub fn wav(taps: &[f64], sample_rate: u32) -> Vec<u8> {
    let data_size = (taps.len() * 4) as u32;
    let mut wav = Vec::with_capacity(44 + data_size as usize);
    wav.extend(b"RIFF");
    wav.extend((36 + data_size).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16_u32.to_le_bytes());
    wav.extend(3_u16.to_le_bytes()); // IEEE float
    wav.extend(1_u16.to_le_bytes()); // Mono IR applies to every channel.
    wav.extend(sample_rate.to_le_bytes());
    wav.extend((sample_rate * 4).to_le_bytes());
    wav.extend(4_u16.to_le_bytes());
    wav.extend(32_u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(data_size.to_le_bytes());
    for &tap in taps {
        wav.extend((tap as f32).to_le_bytes());
    }
    wav
}

fn kernel(points: &[Band], sample_rate: u32) -> Vec<f64> {
    let size = (sample_rate as usize / 4).next_power_of_two().clamp(8192, 65536);
    let mut spectrum: Vec<_> = (0..size).map(|bin| {
        let frequency = f64::from(sample_rate) * bin.min(size - bin) as f64 / size as f64;
        Complex::new(gain_at(points, frequency) * (std::f64::consts::LN_10 / 20.0), 0.0)
    }).collect();
    let mut fft = FftPlanner::new();
    let forward = fft.plan_fft_forward(size);
    let inverse = fft.plan_fft_inverse(size);

    // Keep the causal cepstrum to get a minimum-phase impulse response.
    inverse.process(&mut spectrum);
    for value in &mut spectrum[1..size / 2] {
        *value *= 2.0;
    }
    spectrum[size / 2 + 1..].fill(Complex::ZERO);
    forward.process(&mut spectrum);
    for value in &mut spectrum {
        *value = (*value / size as f64).exp();
    }
    inverse.process(&mut spectrum);

    // Taper the tail to limit truncation ringing and keep the live kernel short.
    let length = size / 2;
    spectrum[..length].iter().enumerate().map(|(index, value)| {
        let tail = (index as f64 / length as f64 - 0.75).max(0.0) * 4.0;
        let window = 0.5 * (1.0 + (std::f64::consts::PI * tail).cos());
        value.re * window / size as f64
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn spline_hits_the_points_and_the_filter_tracks_the_curve() {
        let mut cfg = Config::flat_spline();
        cfg.bands[0].gain_db = -4.0;
        cfg.bands[1].gain_db = -9.0;
        assert!(cfg.add_peak(240.0, 8.0));
        assert!(cfg.add_peak(2700.0, -2.0));
        let rate = 48_000;
        let taps = kernel(&cfg.bands, rate);
        for point in &cfg.bands {
            assert_eq!(gain_at(&cfg.bands, point.frequency_hz), point.gain_db);
        }
        for frequency in [20.0, 58.0, 240.0, 590.0, 2700.0, 7100.0, 20_000.0] {
            let step = -std::f64::consts::TAU * frequency / f64::from(rate);
            let magnitude = taps.iter().enumerate().map(|(i, &tap)| {
                Complex::from_polar(tap, i as f64 * step)
            }).sum::<Complex<f64>>().norm();
            let db = 20.0 * magnitude.log10();
            assert!((db - gain_at(&cfg.bands, frequency)).abs() < 0.15, "{frequency} Hz: {db}");
        }
    }
}
