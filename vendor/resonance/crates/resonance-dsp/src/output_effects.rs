use crate::true_peak::{PHASES, TAPS};
use std::collections::VecDeque;

// #######################################################
// blend short stereo reflections into the direct sound
// #######################################################

// The opposing ear receives the first reflection. The later, quieter taps
// approximate a short room tail. Their weights sum to one.
const REFLECTIONS: [(f64, f64, bool, f64); 10] = [
    (8.2, 10.1, true, 0.33),
    (13.6, 11.8, false, 0.19),
    (19.2, 22.4, true, 0.13),
    (29.8, 25.6, false, 0.10),
    (41.2, 45.8, true, 0.08),
    (58.7, 54.7, false, 0.06),
    (68.3, 76.9, true, 0.045),
    (89.4, 82.6, false, 0.035),
    (104.9, 111.7, true, 0.02),
    (127.5, 119.6, false, 0.01),
];
const ROOM_MAX_WET: f64 = 0.42;

fn frames(sample_rate: f64, milliseconds: f64) -> usize {
    (sample_rate * milliseconds / 1000.0).ceil().max(1.0) as usize
}

#[derive(Clone, Debug)]
pub struct Room {
    amount: f64,
    applied: f64,
    step: f64,
    delays: [([usize; 2], usize, f64); REFLECTIONS.len()],
    history: Vec<[f64; 2]>,
    cursor: usize,
    damped: [f64; 2],
    damping: f64,
}

impl Room {
    pub fn new(rate: f64) -> Self {
        let delays = REFLECTIONS.map(|(left, right, opposite, gain)| {
            ([frames(rate, left), frames(rate, right)], usize::from(opposite), gain)
        });
        let len = delays.iter().flat_map(|(delays, _, _)| *delays).max().unwrap() + 1;
        Self {
            amount: 0.0, applied: 0.0, step: 1.0 / frames(rate, 15.0) as f64,
            delays, history: vec![[0.0; 2]; len], cursor: 0,
            damped: [0.0; 2], damping: 1.0 - (-std::f64::consts::TAU * 4500.0 / rate).exp(),
        }
    }

    pub fn amount(&self) -> f64 {
        self.amount
    }

    pub fn set_amount(&mut self, amount: f64) {
        self.amount = if amount.is_finite() { amount.clamp(0.0, 1.0) } else { 0.0 };
    }

    pub fn warmup_frames(&self) -> usize {
        self.history.len() - 1
    }

    pub fn reset(&mut self) {
        self.history.fill([0.0; 2]);
        self.cursor = 0;
        self.damped = [0.0; 2];
        self.applied = 0.0;
    }

    pub fn process(&mut self, samples: &mut [f64], channels: usize) {
        if channels != 2 || (self.amount == 0.0 && self.applied == 0.0) {
            return;
        }
        let len = self.history.len();
        for frame in samples.chunks_exact_mut(2) {
            // Keep low settings subtle and give the top of the slider more range.
            let wet = ROOM_MAX_WET * self.applied * (0.35 + 0.65 * self.applied);
            for (ch, sample) in frame.iter_mut().enumerate() {
                self.damped[ch] += self.damping * (*sample - self.damped[ch]);
                self.history[self.cursor][ch] = self.damped[ch];
                let reflected: f64 = self.delays.iter().map(|(delays, opposite, weight)| {
                    let delay = delays[ch];
                    let tap = if self.cursor >= delay { self.cursor - delay } else { self.cursor + len - delay };
                    self.history[tap][ch ^ opposite] * weight
                }).sum();
                // A bounded mix of the direct signal and damped reflections.
                *sample = (1.0 - wet) * *sample + wet * reflected;
            }
            self.cursor += 1;
            if self.cursor == len {
                self.cursor = 0;
            }
            self.applied += (self.amount - self.applied).clamp(-self.step, self.step);
        }
        if self.applied == 0.0 {
            self.reset();
        }
    }
}

// #######################################################
// limit linked output peaks with a short lookahead
// #######################################################

/// Use a small margin below the displayed −1 dBTP ceiling for gain changes
/// within the interpolation window.
const TARGET_PEAK: f64 = 0.881_048_873;
const LOOKAHEAD_MS: f64 = 2.0;
const ATTACK_MS: f64 = 0.5;
const RELEASE_MS: f64 = 75.0;
const TRANSITION_MS: f64 = 5.0;

#[derive(Clone, Debug)]
pub struct Limiter {
    enabled: bool,
    delay: Vec<f64>,
    detected: VecDeque<(u64, f64)>,
    gains: Vec<f64>,
    gain_sum: f64,
    gain: f64,
    history: Vec<[f32; TAPS * 2]>,
    sample: u64,
    delay_frames: usize,
    release: f64,
    mix: f64,
    mix_step: f64,
    pub reduction_db: f32,
}

impl Limiter {
    pub fn new(channels: usize, rate: f64) -> Self {
        let delay_frames = frames(rate, LOOKAHEAD_MS).max(TAPS * 2);
        let attack = frames(rate, ATTACK_MS).min(delay_frames - TAPS);
        Self {
            enabled: false, delay: vec![0.0; channels * delay_frames],
            detected: VecDeque::with_capacity(delay_frames + TAPS + 1),
            gains: vec![1.0; attack], gain_sum: attack as f64, gain: 1.0,
            history: vec![[0.0; TAPS * 2]; channels], sample: 0, delay_frames,
            release: 1.0 - (-1000.0 / (rate * RELEASE_MS)).exp(),
            mix: 0.0, mix_step: 1.0 / frames(rate, TRANSITION_MS) as f64, reduction_db: 0.0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn reset(&mut self) {
        self.delay.fill(0.0);
        self.history.fill([0.0; TAPS * 2]);
        self.detected.clear();
        self.gains.fill(1.0);
        self.gain_sum = self.gains.len() as f64;
        self.gain = 1.0;
        self.sample = 0;
        self.mix = 0.0;
        self.reduction_db = 0.0;
    }

    pub fn process(&mut self, samples: &mut [f64], channels: usize) {
        self.reduction_db = 0.0;
        if !self.enabled && self.mix == 0.0 {
            if self.sample != 0 {
                self.reset();
            }
            return;
        }
        let mut lowest_gain = 1.0_f64;
        for frame in samples.chunks_exact_mut(channels) {
            let cursor = self.sample as usize % TAPS;
            let mut peak = 0.0_f64;
            for (history, &sample) in self.history.iter_mut().zip(frame.iter()) {
                peak = peak.max(sample.abs());
                history[cursor] = sample as f32;
                history[cursor + TAPS] = sample as f32;
                let window = &history[cursor + 1..cursor + TAPS + 1];
                for phase in &PHASES {
                    let interpolated: f32 = window.iter().zip(phase).map(|(&x, &c)| x * c).sum();
                    peak = peak.max(f64::from(interpolated.abs()) / 8192.0);
                }
            }
            // Keep the detected peak active until the delayed samples that
            // contributed to it have passed through the interpolation window.
            let oldest = self.sample.saturating_sub((self.delay_frames + TAPS) as u64);
            while self.detected.front().is_some_and(|&(index, _)| index < oldest) {
                self.detected.pop_front();
            }
            while self.detected.back().is_some_and(|&(_, value)| value <= peak) {
                self.detected.pop_back();
            }
            self.detected.push_back((self.sample, peak));
            let required = (TARGET_PEAK / self.detected[0].1).min(1.0);
            self.gain = (self.gain + (1.0 - self.gain) * self.release).min(required);

            // Average a short window of gains to soften the attack. The delay
            // gives this window time to settle before a detected peak arrives.
            let gain_slot = self.sample as usize % self.gains.len();
            self.gain_sum += self.gain - self.gains[gain_slot];
            self.gains[gain_slot] = self.gain;
            let gain = (self.gain_sum / self.gains.len() as f64).clamp(f64::MIN_POSITIVE, 1.0);
            let base = (self.sample as usize % self.delay_frames) * channels;
            if self.sample >= self.delay_frames as u64 {
                let target_mix = f64::from(u8::from(self.enabled));
                self.mix += (target_mix - self.mix).clamp(-self.mix_step, self.mix_step);
            }
            // Crossfade between the direct and delayed paths on live toggles.
            let direct_gain = if self.enabled { (TARGET_PEAK / peak).min(1.0) } else { 1.0 };
            for (sample, delay) in frame.iter_mut().zip(&mut self.delay[base..base + channels]) {
                let delayed = std::mem::replace(delay, *sample);
                *sample = (1.0 - self.mix) * *sample * direct_gain + self.mix * delayed * gain;
            }
            lowest_gain = lowest_gain.min((1.0 - self.mix) * direct_gain + self.mix * gain);
            self.sample += 1;
        }
        self.reduction_db = (-20.0 * lowest_gain.log10()) as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::true_peak::Meter;

    #[test]
    fn room_adds_delayed_stereo_reflections_and_defaults_to_dry() {
        for rate in [48_000.0, 192_000.0] {
            let mut room = Room::new(rate);
            let mut impulse = vec![0.0; frames(rate, 150.0) * 2];
            impulse[0] = 0.8;
            let mut dry = impulse.clone();
            room.process(&mut dry, 2);
            assert_eq!(dry, impulse);

            room.set_amount(1.0);
            room.process(&mut vec![0.0; frames(rate, 20.0) * 2], 2);
            let original = impulse[0];
            room.process(&mut impulse, 2);
            assert!((impulse[0] - original * (1.0 - ROOM_MAX_WET)).abs() < 1e-12);
            let first_right = frames(rate, REFLECTIONS[0].1) * 2 + 1;
            assert!(impulse[first_right] > 0.01);
            assert!(impulse[..first_right].iter().skip(1).step_by(2).all(|sample| *sample == 0.0));
            let last_left = room.warmup_frames() * 2;
            assert!(impulse[last_left] > 0.0001);

            room.set_amount(0.0);
            room.process(&mut vec![0.0; frames(rate, 20.0) * 2], 2);
            room.process(&mut dry, 2);
            assert_eq!(dry[0], original);
            assert!(dry[1..].iter().all(|sample| *sample == 0.0));
        }
    }

    #[test]
    fn room_has_a_clear_stereo_effect_at_full_amount_without_raising_peak_level() {
        for rate in [48_000.0, 192_000.0] {
            let mut room = Room::new(rate);
            room.set_amount(1.0);
            let mut tone = Vec::new();
            for index in 0..frames(rate, 240.0) {
                let sample = (std::f64::consts::TAU * 400.0 * index as f64 / rate).sin();
                tone.extend([sample, 0.0]);
            }
            room.process(&mut tone, 2);
            assert!(tone.iter().all(|sample| sample.abs() <= 1.0 + 1e-12));
            let tail = &tone[tone.len() - frames(rate, 40.0) * 2..];
            let peak = |channel: usize| tail.chunks_exact(2).map(|frame| frame[channel].abs()).fold(0.0, f64::max);
            assert!(peak(1) / peak(0) > 0.18);
        }
    }

    #[test]
    fn limiter_catches_intersample_peaks_and_preserves_stereo_balance() {
        for rate in [48_000.0, 192_000.0] {
            let mut limiter = Limiter::new(2, rate);
            limiter.set_enabled(true);
            limiter.process(&mut vec![0.0; frames(rate, 10.0) * 2], 2);
            let mut audio = Vec::new();
            for index in 0..frames(rate, 12.0) {
                let sample = if index % 4 < 2 { 0.8 } else { -0.8 };
                audio.extend([sample, sample * 0.25]);
            }
            for slice in audio.chunks_mut(94) {
                limiter.process(slice, 2);
            }
            assert!(limiter.reduction_db > 1.5);
            let mut meter = Meter::default();
            for frame in audio.chunks_exact(2) {
                assert!((frame[0] * 0.25 - frame[1]).abs() < 1e-12);
                meter.push([frame[0] as f32, frame[1] as f32]);
            }
            let [left, _] = meter.take_peaks().unwrap();
            assert!(left <= 10.0_f32.powf(-1.0 / 20.0), "{rate} Hz: {left}");
        }
    }

    #[test]
    fn limiter_is_transparent_below_the_ceiling_and_returns_to_dry() {
        let rate = 48_000.0;
        let mut limiter = Limiter::new(2, rate);
        let mut steady = vec![0.3; frames(rate, 20.0) * 2];
        limiter.set_enabled(true);
        limiter.process(&mut steady, 2);
        assert!(steady.iter().all(|sample| (sample - 0.3).abs() < 1e-10));
        assert_eq!(limiter.reduction_db, 0.0);
        limiter.set_enabled(false);
        limiter.process(&mut steady, 2);
        let mut dry = [1.3, -0.3, -0.1, 0.2];
        let before = dry;
        limiter.process(&mut dry, 2);
        assert_eq!(dry, before);
    }
}
