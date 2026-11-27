use crate::convolution::{BLOCK, ConvolutionEngine, IrData};
use crate::true_peak::{PHASES, TAPS};
use std::collections::VecDeque;
use std::sync::Arc;

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

// #######################################################
// render a front speaker pair and adjust stereo width
// #######################################################

// Diffuse-field equalized MIT KEMAR responses at +30°, 0° elevation.
// Mirroring the pair places the other speaker at −30°. See third-party notices.
const SPEAKER_RATE: f64 = 44_100.0;
const SPEAKER_PCM: &[u8; 128 * 4] = include_bytes!("kemar-30-deg.pcm");
const NEAR_PEAK_FRAME: f64 = 17.0;
// Level-match the measured pair to dry playback for broadband material.
const SPEAKER_GAIN: f64 = 0.75 / 32768.0;
const SPATIAL_RAMP_MS: f64 = 15.0;

#[derive(Clone, Debug)]
pub struct Spatial {
    width: f64,
    applied_width: f64,
    amount: f64,
    applied_amount: f64,
    aligned_mix: f64,
    ramp_frames: usize,
    speakers: ConvolutionEngine,
    dry_delay: Vec<[f64; 2]>,
    cursor: usize,
    primed: usize,
    prime_frames: usize,
}

impl Spatial {
    pub fn new(channels: usize, rate: f64) -> Self {
        let mut speakers = ConvolutionEngine::new(2, rate);
        let mut dry_delay = Vec::new();
        let mut prime_frames = 0;
        if channels == 2 {
            let (mut mid, mut side) = (Vec::new(), Vec::new());
            for frame in SPEAKER_PCM.chunks_exact(4) {
                let far = f64::from(i16::from_le_bytes([frame[0], frame[1]]));
                let near = f64::from(i16::from_le_bytes([frame[2], frame[3]]));
                // Symmetric near/far paths diagonalize into two independent FIRs.
                mid.push((near + far) * SPEAKER_GAIN);
                side.push((near - far) * SPEAKER_GAIN);
            }
            let ir = IrData {
                name: "Virtual Speakers".into(), path: String::new(),
                sample_rate: SPEAKER_RATE, channels: vec![mid, side],
            };
            let response_frames = (ir.frames() as f64 * rate / SPEAKER_RATE).ceil() as usize;
            if speakers.load_ir(Arc::new(ir)).is_ok() {
                let latency = speakers.latency_frames();
                let peak = (NEAR_PEAK_FRAME * rate / SPEAKER_RATE).round() as usize;
                dry_delay.resize(latency + peak, [0.0; 2]);
                prime_frames = latency + response_frames;
            }
        }
        Self {
            width: 1.0, applied_width: 1.0, amount: 0.0, applied_amount: 0.0, aligned_mix: 0.0,
            ramp_frames: frames(rate, SPATIAL_RAMP_MS), speakers, dry_delay,
            cursor: 0, primed: 0, prime_frames,
        }
    }

    #[must_use]
    pub fn settings(&self) -> (f64, f64) {
        (self.width, self.amount)
    }

    pub fn set(&mut self, width: f64, virtual_speakers: f64) {
        self.width = if width.is_finite() { width.clamp(0.0, 1.25) } else { 1.0 };
        self.amount = if virtual_speakers.is_finite() { virtual_speakers.clamp(0.0, 1.0) } else { 0.0 };
    }

    #[must_use]
    pub fn warmup_frames(&self) -> usize {
        if self.amount > 0.0 && self.prime_frames > 0 {
            self.prime_frames + self.ramp_frames
        } else if self.width != 1.0 {
            self.ramp_frames
        } else {
            0
        }
    }

    fn reset_speakers(&mut self) {
        self.speakers.reset();
        self.dry_delay.fill([0.0; 2]);
        self.cursor = 0;
        self.primed = 0;
        self.applied_amount = 0.0;
        self.aligned_mix = 0.0;
    }

    pub fn reset(&mut self) {
        self.applied_width = self.width;
        self.reset_speakers();
    }

    pub fn process(&mut self, samples: &mut [f64], channels: usize) {
        if channels != 2 {
            return;
        }
        let step = 1.0 / self.ramp_frames as f64;
        if !self.dry_delay.is_empty() && (self.amount > 0.0 || self.aligned_mix > 0.0) {
            self.process_speakers(samples, step);
        }
        if self.width != 1.0 || self.applied_width != 1.0 {
            for frame in samples.chunks_exact_mut(2) {
                self.applied_width += (self.width - self.applied_width).clamp(-step, step);
                // Adjust the difference channel while leaving the center gain alone.
                let delta = (frame[0] - frame[1]) * (self.applied_width - 1.0) * 0.5;
                frame[0] += delta;
                frame[1] -= delta;
            }
        }
        if self.amount == 0.0 && self.aligned_mix == 0.0 && self.primed > 0 {
            self.reset_speakers();
        }
    }

    fn process_speakers(&mut self, samples: &mut [f64], step: f64) {
        let target_alignment = f64::from(u8::from(self.amount > 0.0));
        let mut scratch = [0.0; 2 * BLOCK];
        // Small, bounded blocks use the convolver's existing preallocated FIFOs.
        for chunk in samples.chunks_mut(scratch.len()) {
            let processed = &mut scratch[..chunk.len() - chunk.len() % 2];
            for (input, filter) in chunk.chunks_exact(2).zip(processed.chunks_exact_mut(2)) {
                filter[0] = (input[0] + input[1]) * 0.5;
                filter[1] = (input[0] - input[1]) * 0.5;
            }
            self.speakers.process(processed, 2);
            for (output, filtered) in chunk.chunks_exact_mut(2).zip(processed.chunks_exact(2)) {
                let delayed = std::mem::replace(&mut self.dry_delay[self.cursor], [output[0], output[1]]);
                self.cursor += 1;
                if self.cursor == self.dry_delay.len() {
                    self.cursor = 0;
                }
                if self.primed < self.prime_frames {
                    self.primed += 1;
                    continue;
                }
                self.applied_amount += (self.amount - self.applied_amount).clamp(-step, step);
                self.aligned_mix += (target_alignment - self.aligned_mix).clamp(-step, step);
                let binaural = [filtered[0] + filtered[1], filtered[0] - filtered[1]];
                for (sample, (dry, wet)) in output.iter_mut().zip(delayed.into_iter().zip(binaural)) {
                    // At a steady blend, align the dry path to the nearer ear.
                    // Fade to and from the undelayed path only on enable/disable.
                    let aligned = dry + (wet - dry) * self.applied_amount;
                    *sample += (aligned - *sample) * self.aligned_mix;
                }
            }
        }
    }
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
    fn width_preserves_the_center_and_returns_to_exact_passthrough() {
        let rate = 48_000.0;
        let mut spatial = Spatial::new(2, rate);
        let input = [0.5, -0.25];
        let mut dry = input;
        spatial.process(&mut dry, 2);
        assert_eq!(dry, input);
        spatial.set(0.0, 0.0);
        let mut audio = input.repeat(frames(rate, 20.0));
        spatial.process(&mut audio, 2);
        assert!((audio[0] - input[0]).abs() < 0.001);
        assert_eq!(&audio[audio.len() - 2..], &[0.125, 0.125]);
        spatial.set(1.25, 0.0);
        let mut wider = input.repeat(frames(rate, 22.0));
        spatial.process(&mut wider, 2);
        assert_eq!(&wider[wider.len() - 2..], &[0.59375, -0.34375]);
        spatial.set(1.0, 0.0);
        spatial.process(&mut wider, 2);
        spatial.process(&mut dry, 2);
        assert_eq!(dry, input);
    }

    #[test]
    fn virtual_speakers_render_mirrored_ear_responses_at_playback_rates() {
        for rate in [44_100.0, 48_000.0, 192_000.0] {
            let mut spatial = Spatial::new(2, rate);
            spatial.set(1.0, 1.0);
            spatial.process(&mut vec![0.0; spatial.warmup_frames() * 2 + 2], 2);
            let mut left = vec![0.0; frames(rate, 20.0) * 2];
            let mut right = left.clone();
            left[0] = 1.0;
            right[1] = 1.0;
            let mut other = spatial.clone();
            for chunk in left.chunks_mut(62) {
                spatial.process(chunk, 2);
            }
            other.process(&mut right, 2);
            for (l, r) in left.chunks_exact(2).zip(right.chunks_exact(2)) {
                assert!((l[0] - r[1]).abs() < 1e-12 && (l[1] - r[0]).abs() < 1e-12);
            }
            let peak = |channel: usize| left.chunks_exact(2).enumerate()
                .map(|(index, frame)| (index, frame[channel].abs()))
                .max_by(|a, b| a.1.total_cmp(&b.1)).unwrap();
            let (near_frame, near) = peak(0);
            let (far_frame, far) = peak(1);
            assert!(near > far && far > 0.015);
            assert!(far_frame > near_frame && near_frame >= BLOCK);

            spatial.set(1.0, 0.0);
            spatial.process(&mut vec![0.0; frames(rate, 20.0) * 2], 2);
            let mut dry = [0.2, 0.5];
            spatial.process(&mut dry, 2);
            assert_eq!(dry, [0.2, 0.5]);
            spatial.set(0.0, 1.0);
            let mut multichannel = [0.1, -0.2, 0.3, -0.4, 0.5, -0.6];
            let original = multichannel;
            spatial.process(&mut multichannel, 6);
            assert_eq!(multichannel, original);
        }
    }

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
