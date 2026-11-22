use crate::config::{Band, Config};
use serde::{Deserialize, Serialize};

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
            tone("LS", 120.0, self.bass.clamp(-12.0, 12.0).into(), 0.707),
            tone("PK", 1000.0, self.mid.clamp(-12.0, 12.0).into(), 0.85),
            tone("HS", 4000.0, self.treble.clamp(-12.0, 12.0).into(), 0.707),
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
        Config {
            name: "Simple".into(),
            description: String::new(),
            preamp_db,
            bypass,
            show_live_view: false,
            bands,
            ..Config::default()
        }
    }
}

fn tone(kind: &str, frequency_hz: f64, gain_db: f64, q: f64) -> Band {
    Band { kind: kind.into(), frequency_hz, gain_db, q, enabled: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loudness_adds_shelves_and_reserves_headroom_at_maximum_boost() {
        let mut controls = SimpleSettings { bass: 12.0, mid: 12.0, treble: 12.0, ..SimpleSettings::default() };
        let off = controls.config(false, true);
        controls.loudness = true;
        let on = controls.config(false, true);
        assert_eq!(on.bands[..3], off.bands);
        assert_eq!(on.bands[3].kind, "LS");
        assert_eq!(on.bands[4].kind, "HS");
        assert_eq!(on.preamp_db, -43.5);
        assert!(on.validate().is_ok());
        assert!(controls.config(true, true).bypass);
        let without_headroom = controls.config(false, false);
        assert_eq!(without_headroom.preamp_db, 0.0);
        assert_eq!(without_headroom.bands, on.bands);
    }
}
