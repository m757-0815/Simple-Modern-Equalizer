use resonance_ipc::transport::SyncClient;

pub const MODE: i32 = 3;
const CAPTURE_FRAMES: u32 = 8192;

pub struct Audio {
    pub samples: Vec<f32>,
    pub rate: f64,
}

/// Show 20 ms, using a nearby rising zero crossing when one fits in the history.
pub fn window(samples: &[f32], rate: f64) -> Vec<f32> {
    if !rate.is_finite() || rate <= 0.0 {
        return Vec::new();
    }
    let length = (rate / 50.0).round() as usize;
    if length < 2 || samples.len() < length {
        return Vec::new();
    }
    let latest = samples.len() - length;
    let first = latest.saturating_sub(length / 3).max(1);
    let start = (first..=latest)
        .rev()
        .find(|&index| samples[index - 1] < 0.0 && samples[index] >= 0.0)
        .unwrap_or(latest);
    samples[start..start + length].to_vec()
}

#[cfg(not(windows))]
#[derive(Default)]
pub struct Capture;

#[cfg(not(windows))]
impl Capture {
    pub fn read(
        &mut self,
        client: &mut SyncClient,
        state: &resonance_ipc::DaemonState,
        enabled: bool,
    ) -> Option<Audio> {
        use resonance_ipc::{Command, Response};
        if !enabled {
            return None;
        }
        if state.meters.out_peak <= 0.00001 {
            return Some(Audio { samples: Vec::new(), rate: state.sample_rate });
        }
        match client.send_recv(Command::CaptureOutput { frames: CAPTURE_FRAMES }).ok()? {
            Response::Capture { rate, samples } => Some(Audio { samples, rate }),
            _ => None,
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    pub struct Capture {
        endpoint: Option<String>,
        loopback: Option<Loopback>,
        retry_after: Option<Instant>,
    }

    impl Capture {
        pub fn read(
            &mut self,
            _: &mut SyncClient,
            state: &resonance_ipc::DaemonState,
            enabled: bool,
        ) -> Option<Audio> {
            if !enabled {
                *self = Self::default();
                return None;
            }
            if self.endpoint != state.active_output {
                self.loopback = None;
                self.retry_after = None;
                self.endpoint.clone_from(&state.active_output);
            }
            if self.loopback.as_ref().is_some_and(|capture| capture.failed.load(Ordering::Relaxed)) {
                self.loopback = None;
            }
            if self.loopback.is_none() && self.retry_after.is_none_or(|deadline| Instant::now() >= deadline) {
                self.retry_after = Some(Instant::now() + Duration::from_secs(1));
                self.loopback = Loopback::start();
            }
            self.loopback.as_ref().map(|capture| Audio { samples: capture.samples(), rate: capture.rate })
        }
    }

    struct Loopback {
        _stream: Stream,
        rate: f64,
        buffer: Arc<Mutex<Buffer>>,
        failed: Arc<AtomicBool>,
    }

    struct Buffer {
        samples: VecDeque<f32>,
        updated: Option<Instant>,
    }

    impl Loopback {
        fn start() -> Option<Self> {
            let device = cpal::default_host().default_output_device()?;
            let format = device.default_output_config().ok()?;
            let config = format.config();
            if config.channels == 0 {
                return None;
            }
            let buffer = Arc::new(Mutex::new(Buffer {
                samples: VecDeque::with_capacity(CAPTURE_FRAMES as usize),
                updated: None,
            }));
            let failed = Arc::new(AtomicBool::new(false));
            let stream = match format.sample_format() {
                SampleFormat::F32 => input::<f32>(&device, &config, &buffer, &failed),
                SampleFormat::I16 => input::<i16>(&device, &config, &buffer, &failed),
                SampleFormat::I24 => input::<cpal::I24>(&device, &config, &buffer, &failed),
                SampleFormat::I32 => input::<i32>(&device, &config, &buffer, &failed),
                _ => return None,
            }.ok()?;
            stream.play().ok()?;
            Some(Self { _stream: stream, rate: f64::from(config.sample_rate), buffer, failed })
        }

        fn samples(&self) -> Vec<f32> {
            let mut buffer = self.buffer.lock().unwrap();
            if buffer.updated.is_none_or(|updated| updated.elapsed() >= Duration::from_millis(150)) {
                buffer.samples.clear();
                return Vec::new();
            }
            buffer.samples.iter().copied().collect()
        }
    }

    fn input<T: SizedSample>(
        device: &cpal::Device,
        config: &StreamConfig,
        buffer: &Arc<Mutex<Buffer>>,
        failed: &Arc<AtomicBool>,
    ) -> Result<Stream, cpal::BuildStreamError>
    where
        f32: FromSample<T>,
    {
        let channels = usize::from(config.channels);
        let buffer = Arc::clone(buffer);
        let failed = Arc::clone(failed);
        // On WASAPI, an input stream on the render device captures its loopback.
        device.build_input_stream(
            config,
            move |samples: &[T], _| {
                if let Ok(mut buffer) = buffer.try_lock() {
                    for frame in samples.chunks_exact(channels) {
                        let sum = frame.iter().map(|&sample| f32::from_sample(sample)).sum::<f32>();
                        if buffer.samples.len() == CAPTURE_FRAMES as usize {
                            buffer.samples.pop_front();
                        }
                        buffer.samples.push_back(sum / channels as f32);
                    }
                    buffer.updated = Some(Instant::now());
                }
            },
            move |_| failed.store(true, Ordering::Relaxed),
            None,
        )
    }
}

#[cfg(windows)]
pub use windows::Capture;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_spans_twenty_ms_and_uses_a_recent_rising_edge() {
        let mut samples = vec![-0.5; 1_500];
        samples[500..].fill(0.5);
        let trace = window(&samples, 48_000.0);
        assert_eq!(trace.len(), 960);
        assert_eq!(trace, samples[500..1460]);
        assert_eq!(window(&samples, 44_100.0).len(), 882);
        assert!(window(&samples, f64::NAN).is_empty());
        assert!(window(&samples[..20], 48_000.0).is_empty());
    }
}
