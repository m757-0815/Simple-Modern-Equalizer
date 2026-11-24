use super::{OutputFormat, Snapshot, Track};
use windows::{
    Media::Control::{
        GlobalSystemMediaTransportControlsSession as Session,
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    },
    Win32::{
        Media::{
            Audio::{
                IMMDeviceEnumerator, MMDeviceEnumerator, PKEY_AudioEngine_DeviceFormat,
                WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
            },
            KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE},
            Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
        },
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance, STGM_READ, StructuredStorage::PropVariantClear},
            Variant::VT_BLOB,
            WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
        },
    },
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
    devices: Option<IMMDeviceEnumerator>,
    apartment: Option<Apartment>,
}

impl Default for Source {
    fn default() -> Self {
        // SAFETY: the background thread owns this apartment and all WinRT session objects.
        let apartment = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.ok().map(|()| Apartment);
        Self { manager: None, devices: None, apartment }
    }
}

impl Source {
    pub(super) fn snapshot(&mut self) -> Snapshot {
        Snapshot { track: self.track(), output: self.output() }
    }

    fn output(&mut self) -> Option<OutputFormat> {
        self.apartment.as_ref()?;
        if self.devices.is_none() {
            // SAFETY: the worker has initialized COM and owns the enumerator.
            self.devices = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ok();
        }
        let devices = self.devices.as_ref()?;
        // SAFETY: GetValue returns an initialized PROPVARIANT. The blob is read
        // while the value is alive, and PropVariantClear releases it afterward.
        unsafe {
            let device = devices.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let properties = device.OpenPropertyStore(STGM_READ).ok()?;
            let mut value = properties.GetValue(&PKEY_AudioEngine_DeviceFormat).ok()?;
            let variant = &*value.Anonymous.Anonymous;
            let format = if variant.vt == VT_BLOB {
                let blob = variant.Anonymous.blob;
                if blob.pBlobData.is_null() {
                    None
                } else {
                    read_wave_format(std::slice::from_raw_parts(blob.pBlobData, blob.cbSize as usize))
                }
            } else {
                None
            };
            let _ = PropVariantClear(&mut value);
            format
        }
    }

    fn track(&mut self) -> Option<Track> {
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

fn read_wave_format(bytes: &[u8]) -> Option<OutputFormat> {
    if bytes.len() < size_of::<WAVEFORMATEX>() {
        return None;
    }
    // SAFETY: the length checks cover both reads, and the packed structures
    // contain only integers, a GUID and an integer union.
    let format = unsafe { bytes.as_ptr().cast::<WAVEFORMATEX>().read_unaligned() };
    if format.nChannels == 0 {
        return None;
    }
    let mut depth = format.wBitsPerSample;
    let float = match u32::from(format.wFormatTag) {
        WAVE_FORMAT_PCM => false,
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE if format.cbSize >= 22 && bytes.len() >= size_of::<WAVEFORMATEXTENSIBLE>() => {
            let extended = unsafe { bytes.as_ptr().cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
            let subtype = extended.SubFormat;
            let float = match subtype {
                KSDATAFORMAT_SUBTYPE_PCM => false,
                KSDATAFORMAT_SUBTYPE_IEEE_FLOAT => true,
                _ => return None,
            };
            let valid = unsafe { extended.Samples.wValidBitsPerSample };
            if valid > depth {
                return None;
            }
            if valid > 0 {
                depth = valid;
            }
            float
        }
        _ => return None,
    };
    OutputFormat::new(format.nSamplesPerSec, depth, float)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_format_reports_valid_bits_in_a_wider_container() {
        let mut bytes = [0; size_of::<WAVEFORMATEXTENSIBLE>()];
        bytes[0..2].copy_from_slice(&(WAVE_FORMAT_EXTENSIBLE as u16).to_le_bytes());
        bytes[2..4].copy_from_slice(&2_u16.to_le_bytes());
        bytes[4..8].copy_from_slice(&48_000_u32.to_le_bytes());
        bytes[14..16].copy_from_slice(&32_u16.to_le_bytes());
        bytes[16..18].copy_from_slice(&22_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&24_u16.to_le_bytes());
        bytes[24..40].copy_from_slice(&[1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113]);

        let format = read_wave_format(&bytes).unwrap();
        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.bit_depth_label(), "24-bit");
        assert!(read_wave_format(&bytes[..24]).is_none());

        bytes[18..20].copy_from_slice(&32_u16.to_le_bytes());
        bytes[24] = 3; // IEEE float subtype
        assert_eq!(read_wave_format(&bytes).unwrap().bit_depth_label(), "32-bit float");
    }
}
