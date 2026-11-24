use std::sync::mpsc;
#[cfg(any(windows, target_os = "linux"))]
use std::{thread, time::Duration};

#[cfg(windows)]
#[path = "now_playing/windows.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "now_playing/linux.rs"]
mod platform;

pub struct Track {
    pub title: String,
    pub artist: String,
}

pub fn subscribe() -> mpsc::Receiver<Option<Track>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    #[cfg(any(windows, target_os = "linux"))]
    thread::spawn(move || {
        let mut source = platform::Source::default();
        // Keep OS media queries off the UI and DSP threads.
        while sender.send(source.track()).is_ok() {
            thread::sleep(Duration::from_secs(1));
        }
    });
    #[cfg(not(any(windows, target_os = "linux")))]
    drop(sender);
    receiver
}
