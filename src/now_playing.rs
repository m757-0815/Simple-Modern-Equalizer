use std::{sync::mpsc, thread, time::Duration};

#[cfg(windows)]
#[path = "now_playing/windows.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "now_playing/linux.rs"]
mod platform;

#[derive(Default)]
pub struct Snapshot {
    pub track: Option<Track>,
    pub output: Option<OutputFormat>,
}

pub struct Track {
    pub title: String,
    pub artist: String,
}

#[derive(Clone, Copy)]
pub struct OutputFormat {
    pub sample_rate: u32,
    bits_per_sample: u16,
    float: bool,
}

impl OutputFormat {
    pub fn new(sample_rate: u32, bits_per_sample: u16, float: bool) -> Option<Self> {
        (sample_rate > 0 && bits_per_sample > 0).then_some(Self { sample_rate, bits_per_sample, float })
    }

    pub fn bit_depth_label(self) -> String {
        let encoding = if self.float { " float" } else { "" };
        format!("{}-bit{encoding}", self.bits_per_sample)
    }
}

pub fn subscribe() -> mpsc::Receiver<Snapshot> {
    let (sender, receiver) = mpsc::sync_channel(1);
    #[cfg(any(windows, target_os = "linux"))]
    thread::spawn(move || {
        let mut source = platform::Source::default();
        // Keep OS media queries and audio-device enumeration off the UI and DSP threads.
        while sender.send(source.snapshot()).is_ok() {
            thread::sleep(Duration::from_secs(1));
        }
    });
    #[cfg(not(any(windows, target_os = "linux")))]
    drop(sender);
    receiver
}

pub fn format_rate(hertz: u32) -> String {
    let mut rate = format!("{:.3}", f64::from(hertz) / 1000.0);
    rate.truncate(rate.trim_end_matches('0').trim_end_matches('.').len());
    format!("{rate} kHz")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_format_preserves_bit_depth_and_fractional_kilohertz() {
        let format = OutputFormat::new(44_100, 24, false).unwrap();
        assert_eq!(format_rate(format.sample_rate), "44.1 kHz");
        assert_eq!(format.bit_depth_label(), "24-bit");
        let float = OutputFormat::new(48_000, 32, true).unwrap();
        assert_eq!(float.bit_depth_label(), "32-bit float");
        assert_eq!(format_rate(48_000), "48 kHz");
        assert!(OutputFormat::new(0, 32, true).is_none());
        assert!(OutputFormat::new(48_000, 0, false).is_none());
    }
}
