//! Streaming four-times peak estimation for a stereo monitor.

// #######################################################
// set the interpolation coefficients
// #######################################################

pub(crate) const TAPS: usize = 12;
// The example interpolator from ITU-R BS.1770-5 Annex 2, scaled by 8192.
pub(crate) const PHASES: [[f32; TAPS]; 4] = [
    [14., 90., -161., 272., -487., 1125., 7964., -838., 390., -218., 122., -68.],
    [-239., 240., -424., 730., -1364., 3810., 6388., -1641., 832., -477., 271., -155.],
    [-155., 271., -477., 832., -1641., 6388., 3810., -1364., 730., -424., 240., -239.],
    [-68., 122., -218., 390., -838., 7964., 1125., -487., 272., -161., 90., 14.],
];

// #######################################################
// track the highest peak in each channel
// #######################################################

#[derive(Default)]
pub struct Meter {
    // Duplicating the window makes the taps contiguous as the cursor wraps.
    history: [[f32; 2]; TAPS * 2],
    cursor: usize,
    filled: usize,
    peaks: Option<[f32; 2]>,
}

impl Meter {
    /// Discard interpolation history at gaps or format changes. Peaks already
    /// detected still belong to the current reading interval.
    pub fn reset_history(&mut self) {
        self.filled = 0;
    }

    pub fn push(&mut self, frame: [f32; 2]) {
        let frame = frame.map(|sample| if sample.is_finite() { sample } else { 0.0 });
        self.history[self.cursor] = frame;
        self.history[self.cursor + TAPS] = frame;
        self.cursor = (self.cursor + 1) % TAPS;
        self.filled = (self.filled + 1).min(TAPS);

        // Count the original sample peaks even while the interpolation window fills.
        let peaks = self.peaks.get_or_insert([0.0; 2]);
        for (peak, sample) in peaks.iter_mut().zip(frame) {
            *peak = peak.max(sample.abs());
        }
        if self.filled == TAPS {
            // Estimate peaks between samples with four fractional positions.
            let history = &self.history[self.cursor..self.cursor + TAPS];
            for phase in PHASES {
                let mut sums = [0.0_f32; 2];
                for (&[left, right], tap) in history.iter().zip(phase) {
                    sums[0] += left * tap;
                    sums[1] += right * tap;
                }
                for (peak, sum) in peaks.iter_mut().zip(sums) {
                    *peak = peak.max(sum.abs() / 8192.0);
                }
            }
        }
    }

    pub fn take_peaks(&mut self) -> Option<[f32; 2]> {
        // Keep the sample history so peaks that span two reads can still be measured.
        self.peaks.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_an_intersample_over_on_one_channel_across_reads() {
        let mut meter = Meter::default();
        for index in 0..64 {
            let sample = if index % 4 < 2 { 0.80 } else { -0.80 };
            meter.push([sample, sample * 0.5]);
            if index < 40 && index % 7 == 6 {
                meter.take_peaks();
            }
        }
        let [left, right] = meter.take_peaks().unwrap();
        assert!((left - 0.8 * 2.0_f32.sqrt()).abs() < 0.05, "{left}");
        assert!((right * 2.0 - left).abs() < 0.00001);
        assert!(meter.take_peaks().is_none());
    }

    #[test]
    fn a_gap_does_not_interpolate_across_unrelated_audio() {
        let mut meter = Meter::default();
        for _ in 0..TAPS {
            meter.push([1.0, 0.5]);
        }
        meter.take_peaks();
        meter.reset_history();
        for _ in 0..TAPS - 1 {
            meter.push([0.0; 2]);
        }
        assert_eq!(meter.take_peaks(), Some([0.0; 2]));
    }
}
