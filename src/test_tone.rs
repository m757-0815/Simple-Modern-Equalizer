use std::{
    f64::consts::TAU,
    sync::{Arc, atomic::{AtomicU32, Ordering}},
    time::{Duration, Instant},
};

const SECONDS: u32 = 30;
const PEAK: f64 = 0.0158489319; // -36 dBFS before EQ
const MIN_FREQUENCY: u32 = 20;
const MAX_FREQUENCY: u32 = 20_000;

#[derive(Default)]
pub struct TestTone(Option<Session>);

struct Session {
    player: platform::Player,
    frequency: Arc<AtomicU32>,
    max_frequency: u32,
    output: String,
    deadline: Instant,
}

impl TestTone {
    pub fn start(&mut self, output: &str, frequency: i32) -> Result<i32, String> {
        self.stop();
        let frequency = Arc::new(AtomicU32::new(bounded_frequency(frequency)));
        let (player, max_frequency) = platform::Player::start(output, frequency.clone())?;
        let actual_frequency = frequency.load(Ordering::Relaxed);
        self.0 = Some(Session {
            player,
            frequency,
            max_frequency,
            output: output.into(),
            deadline: Instant::now() + Duration::from_millis(u64::from(SECONDS) * 1000 + 250),
        });
        Ok(actual_frequency as i32)
    }

    pub fn stop(&mut self) {
        self.0 = None;
    }

    pub fn change_frequency(&self, frequency: i32) -> i32 {
        let mut frequency = bounded_frequency(frequency);
        if let Some(session) = &self.0 {
            frequency = frequency.min(session.max_frequency);
            session.frequency.store(frequency, Ordering::Relaxed);
        }
        frequency as i32
    }

    pub fn poll(&mut self, output: Option<&str>, now: Instant) -> Result<bool, String> {
        if let Some(session) = &mut self.0
            && output == Some(session.output.as_str()) && now < session.deadline
        {
            match session.player.is_playing() {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(error) => {
                    self.stop();
                    return Err(error);
                }
            }
        }
        self.stop();
        Ok(false)
    }
}

fn bounded_frequency(frequency: i32) -> u32 {
    frequency.clamp(MIN_FREQUENCY as i32, MAX_FREQUENCY as i32) as u32
}

struct Sine {
    frequency: Arc<AtomicU32>,
    rate: f64,
    phase: f64,
    remaining: u32,
    total: u32,
    fade: u32,
}

impl Sine {
    fn new(rate: u32, frequency: Arc<AtomicU32>) -> Self {
        let total = rate * SECONDS;
        Self { frequency, rate: f64::from(rate), phase: 0.0, remaining: total, total, fade: (rate / 50).max(1) }
    }

    fn next_block(&mut self) -> impl Iterator<Item = f32> + '_ {
        let frequency = f64::from(self.frequency.load(Ordering::Relaxed));
        let step = TAU * frequency / self.rate;
        std::iter::from_fn(move || {
            if self.remaining == 0 {
                return None;
            }
            let edge = (self.total - self.remaining).min(self.remaining - 1);
            let gain = PEAK * f64::from(edge.min(self.fade)) / f64::from(self.fade);
            let sample = gain * self.phase.sin();
            self.phase += step;
            if self.phase >= TAU {
                self.phase -= TAU;
            }
            self.remaining -= 1;
            Some(sample as f32)
        })
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use cpal::{
        Device, DeviceId, FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
        traits::{DeviceTrait, HostTrait, StreamTrait},
    };
    use std::sync::atomic::AtomicBool;

    pub struct Player {
        _stream: Stream,
        failed: Arc<AtomicBool>,
    }

    impl Player {
        pub fn start(output: &str, frequency: Arc<AtomicU32>) -> Result<(Self, u32), String> {
            let host = cpal::default_host();
            let device = host.device_by_id(&DeviceId(host.id(), output.into()))
                .ok_or("The selected audio output is unavailable.")?;
            let format = device.default_output_config()
                .map_err(|error| format!("Could not read the output format. {error}"))?;
            let config = format.config();
            let max_frequency = MAX_FREQUENCY.min(config.sample_rate.saturating_sub(1) / 2);
            if config.channels == 0 || max_frequency < MIN_FREQUENCY {
                return Err("The output format does not support test tone playback.".into());
            }
            frequency.fetch_min(max_frequency, Ordering::Relaxed);
            let signal = Sine::new(config.sample_rate, frequency);
            let failed = Arc::new(AtomicBool::new(false));
            let stream = match format.sample_format() {
                SampleFormat::F32 => build::<f32>(&device, &config, signal, failed.clone()),
                SampleFormat::I16 => build::<i16>(&device, &config, signal, failed.clone()),
                SampleFormat::I24 => build::<cpal::I24>(&device, &config, signal, failed.clone()),
                SampleFormat::I32 => build::<i32>(&device, &config, signal, failed.clone()),
                _ => return Err("The output format does not support test tone playback.".into()),
            }.map_err(|error| format!("Could not open the test tone stream. {error}"))?;
            stream.play().map_err(|error| format!("Could not start the test tone. {error}"))?;
            Ok((Self { _stream: stream, failed }, max_frequency))
        }

        pub fn is_playing(&mut self) -> Result<bool, String> {
            if self.failed.load(Ordering::Relaxed) {
                return Err("The test tone stream lost its audio output.".into());
            }
            Ok(true)
        }
    }

    fn build<T: SizedSample + FromSample<f32>>(
        device: &Device, config: &StreamConfig, mut signal: Sine, failed: Arc<AtomicBool>,
    ) -> Result<Stream, cpal::BuildStreamError> {
        let channels = usize::from(config.channels);
        device.build_output_stream(
            config,
            move |buffer: &mut [T], _| {
                let mut samples = signal.next_block();
                for frame in buffer.chunks_exact_mut(channels) {
                    let sample = T::from_sample(samples.next().unwrap_or_default());
                    frame.fill(T::EQUILIBRIUM);
                    frame[..channels.min(2)].fill(sample);
                }
            },
            move |_| failed.store(true, Ordering::Relaxed),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_is_quiet_fades_and_keeps_phase_when_retuned() {
        let frequency = Arc::new(AtomicU32::new(1_000));
        let mut sine = Sine::new(48_000, frequency.clone());
        let opening: Vec<_> = sine.next_block().take(989).collect();
        assert_eq!(opening[0], 0.0);
        assert!(opening[..240].iter().all(|sample| sample.abs() < (PEAK / 4.0) as f32));
        assert!((f64::from(opening[972]) - PEAK).abs() < 1e-8);

        let phase = sine.phase;
        frequency.store(2_000, Ordering::Relaxed);
        let retuned: Vec<_> = sine.next_block().take(2).collect();
        assert!((f64::from(retuned[0]) - PEAK * phase.sin()).abs() < 1e-8);
        assert!((f64::from(retuned[1]) - PEAK * (phase + TAU / 24.0).sin()).abs() < 1e-8);

        let total = (48_000 * SECONDS) as usize;
        let end = sine.next_block().skip(total - opening.len() - retuned.len() - 2)
            .collect::<Vec<_>>();
        assert_eq!(end.len(), 2);
        assert_eq!(end[1], 0.0);
        assert_eq!(sine.next_block().count(), 0);
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::{io::Write, process::{Child, Command, Stdio}, thread};

    pub struct Player(Child);

    impl Player {
        pub fn start(_: &str, frequency: Arc<AtomicU32>) -> Result<(Self, u32), String> {
            // This virtual sink feeds the EQ graph and the device selected in the app.
            let mut player = Self(Command::new("pw-play")
                .args([
                    "--target=resonance",
                    r#"--properties={"node.dont-fallback":true,"node.dont-reconnect":true}"#,
                    "--latency=20ms", "--rate=48000", "--channels=2", "--format=f32", "-",
                ])
                .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
                .spawn().map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        "Test tone playback requires pw-play from the PipeWire utilities.".into()
                    } else {
                        format!("Could not start the test tone. {error}")
                    }
                })?);
            let mut input = player.0.stdin.take().ok_or("Could not open the test tone input.")?;
            thread::Builder::new().name("test-tone".into()).spawn(move || {
                let mut signal = Sine::new(48_000, frequency);
                let mut block = [0; 480 * 8];
                loop {
                    let mut bytes = 0;
                    for (sample, frame) in signal.next_block().zip(block.chunks_exact_mut(8)) {
                        let sample = sample.to_ne_bytes();
                        frame[..4].copy_from_slice(&sample);
                        frame[4..].copy_from_slice(&sample);
                        bytes += 8;
                    }
                    if bytes == 0 || input.write_all(&block[..bytes]).is_err() {
                        break;
                    }
                }
            }).map_err(|error| format!("Could not generate the test tone. {error}"))?;
            Ok((player, MAX_FREQUENCY))
        }

        pub fn is_playing(&mut self) -> Result<bool, String> {
            match self.0.try_wait() {
                Ok(None) => Ok(true),
                Ok(Some(status)) if status.success() => Ok(false),
                Ok(Some(_)) | Err(_) => Err("PipeWire could not play the test tone through the EQ output.".into()),
            }
        }
    }

    impl Drop for Player {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::*;

    pub struct Player;

    impl Player {
        pub fn start(_: &str, _: Arc<AtomicU32>) -> Result<(Self, u32), String> {
            Err("Test tone playback is unavailable on this platform.".into())
        }

        pub fn is_playing(&mut self) -> Result<bool, String> {
            Ok(false)
        }
    }
}
