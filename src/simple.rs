use crate::config::{Band, Config};
use serde::{Deserialize, Serialize};

const BASS_HZ: f64 = 120.0;
const MID_HZ: f64 = 1000.0;
const TREBLE_HZ: f64 = 4000.0;

pub fn clipping_controls(regions: &[(f32, f32)]) -> [bool; 3] {
    // Group the spectrum around the tone controls using log-frequency midpoints.
    let edges = [
        0.0,
        crate::graph::position((BASS_HZ * MID_HZ).sqrt(), 0.0).0 as f32,
        crate::graph::position((MID_HZ * TREBLE_HZ).sqrt(), 0.0).0 as f32,
        1.0,
    ];
    std::array::from_fn(|index| {
        regions.iter().any(|&(start, end)| start < edges[index + 1] && end > edges[index])
    })
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SimpleSettings {
    pub active: bool,
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    pub loudness: bool,
}

impl SimpleSettings {
    pub fn config(&self, bypass: bool, auto_headroom: bool) -> Config {
        let mut bands = vec![
            tone("LS", BASS_HZ, self.bass.clamp(-12.0, 12.0).into(), 0.707),
            tone("PK", MID_HZ, self.mid.clamp(-12.0, 12.0).into(), 0.85),
            tone("HS", TREBLE_HZ, self.treble.clamp(-12.0, 12.0).into(), 0.707),
        ];
        if self.loudness {
            bands.extend([
                tone("LS", 180.0, 5.0, 0.707),
                tone("HS", 6000.0, 2.5, 0.707),
            ]);
        }
        // Reserve headroom for positive gains, even where filters overlap.
        let preamp_db = if auto_headroom {
            -bands.iter().map(|band| band.gain_db.max(0.0)).sum::<f64>()
        } else {
            0.0
        };
        let mut config = Config {
            name: "Simple".into(),
            description: String::new(),
            preamp_db,
            bypass,
            show_live_view: false,
            bands,
            ..Config::default()
        };
        config.sort_bands();
        config
    }
}

fn tone(kind: &str, frequency_hz: f64, gain_db: f64, q: f64) -> Band {
    Band { kind: kind.into(), frequency_hz, gain_db, q, enabled: true, edited: Some(true) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_regions_can_mark_one_or_several_tone_controls() {
        let region = |frequency| {
            let center = crate::graph::position(frequency, 0.0).0 as f32;
            (center - 0.005, center + 0.005)
        };
        assert_eq!(clipping_controls(&[]), [false, false, false]);
        assert_eq!(clipping_controls(&[region(BASS_HZ)]), [true, false, false]);
        assert_eq!(clipping_controls(&[region(MID_HZ)]), [false, true, false]);
        assert_eq!(clipping_controls(&[region(BASS_HZ), region(TREBLE_HZ)]), [true, false, true]);
    }

    #[test]
    fn loudness_adds_shelves_and_reserves_headroom_at_maximum_boost() {
        let mut controls = SimpleSettings { bass: 12.0, mid: 12.0, treble: 12.0, ..SimpleSettings::default() };
        let off = controls.config(false, true);
        controls.loudness = true;
        let on = controls.config(false, true);
        assert!(off.bands.iter().all(|band| on.bands.contains(band)));
        assert_eq!(on.bands[1].kind, "LS");
        assert_eq!(on.bands[4].kind, "HS");
        assert_eq!(on.preamp_db, -43.5);
        assert!(on.validate().is_ok());
        assert!(controls.config(true, true).bypass);
        let without_headroom = controls.config(false, false);
        assert_eq!(without_headroom.preamp_db, 0.0);
        assert_eq!(without_headroom.bands, on.bands);
    }
}
