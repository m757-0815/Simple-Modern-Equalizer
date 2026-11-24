use super::Track;
use windows::{
    Media::Control::{
        GlobalSystemMediaTransportControlsSession as Session,
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    },
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
};

struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: this guard exists only after RoInitialize succeeded on this thread.
        unsafe { RoUninitialize() };
    }
}

pub(super) struct Source {
    manager: Option<Manager>,
    apartment: Option<Apartment>,
}

impl Default for Source {
    fn default() -> Self {
        // SAFETY: the background thread owns this apartment and all WinRT session objects.
        let apartment = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.ok().map(|()| Apartment);
        Self { manager: None, apartment }
    }
}

impl Source {
    pub(super) fn track(&mut self) -> Option<Track> {
        self.apartment.as_ref()?;
        if self.manager.is_none() {
            self.manager = Manager::RequestAsync().and_then(|request| request.join()).ok();
        }
        let manager = self.manager.as_ref()?;
        manager.GetCurrentSession().ok().and_then(read_track).or_else(|| {
            manager.GetSessions().ok()?.into_iter().find_map(read_track)
        })
    }
}

fn read_track(session: Session) -> Option<Track> {
    if session.GetPlaybackInfo().ok()?.PlaybackStatus().ok()? != Status::Playing {
        return None;
    }
    let properties = session.TryGetMediaPropertiesAsync().ok()?.join().ok()?;
    let title = properties.Title().ok()?.to_string_lossy();
    if title.trim().is_empty() {
        return None;
    }
    let artist = properties.Artist().map(|value| value.to_string_lossy()).unwrap_or_default();
    Some(Track { title, artist })
}
