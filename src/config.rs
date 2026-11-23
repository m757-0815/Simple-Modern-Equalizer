use resonance_ipc::{BandScope, BandState, BandType, ChannelMask};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

static APO_FILTER: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(
    r"(?i)^Filter(?:\s+\d+)?:\s*(ON|OFF)\s+(PK|PEQ|LS|HS)\s+Fc\s+([+\-\d.e]+)\s+Hz\s+Gain\s+([+\-\d.e]+)\s+dB\s+Q\s+([+\-\d.e]+)\s*$"
).unwrap());
static APO_COPY: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(
    r"(?i)^Copy:\s*L=([+\-\d.e]+)\*L\s+R=([+\-\d.e]+)\*R\s*$"
).unwrap());

pub const MANUAL_PREAMP_LIMIT_DB: f64 = 12.0;
pub const MAX_BANDS: usize = 32;
pub const MIN_FREQUENCY: f64 = 20.0;
pub const MAX_FREQUENCY: f64 = 20000.0;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Band {
    pub kind: String,
    pub frequency_hz: f64,
    pub gain_db: f64,
    pub q: f64,
    pub enabled: bool,
    // Older presets have no edit history. Treat those bands as protected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub name: String,
    pub description: String,
    pub preamp_db: f64,
    /// -1 = left only; 0 = centered; 1 = right only. Attenuation, never boost.
    pub balance: f64,
    pub bypass: bool,
    // Keep the version 1 JSON key so older settings still load.
    #[serde(rename = "show_spectrum")]
    pub show_live_view: bool,
    #[serde(default)]
    pub spline: bool,
    pub bands: Vec<Band>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: 1,
            name: "Flat".into(),
            description: "Unmodified sound. Add bands to create your own tuning.".into(),
            preamp_db: 0.0,
            balance: 0.0,
            bypass: false,
            show_live_view: true,
            spline: false,
            bands: (0..5).map(|i| Band::flat(i, 5)).collect(),
        }
    }
}

impl Band {
    pub fn needs_removal_confirmation(&self) -> bool {
        self.edited != Some(false)
    }

    pub fn flat(index: usize, count: usize) -> Self {
        let t = if count <= 1 { 0.5 } else { index as f64 / (count - 1) as f64 };
        Self { kind: "PK".into(), frequency_hz: 40.0 * 400.0_f64.powf(t), gain_db: 0.0, q: 1.0, enabled: true, edited: Some(false) }
    }

    pub fn state(&self) -> BandState {
        let band_type = match self.kind.as_str() {
            "LS" => BandType::LowShelf,
            "HS" => BandType::HighShelf,
            _ => BandType::Peaking,
        };
        BandState { band_type, freq: self.frequency_hz, gain_db: self.gain_db,
            q: self.q, enabled: self.enabled, channels: ChannelMask::ALL,
            slope_db_oct: 12, scope: BandScope::Stereo, dynamics: None }
    }
}

impl Config {
    pub fn flat_spline() -> Self {
        let bands = [MIN_FREQUENCY, MAX_FREQUENCY].into_iter()
            .map(|frequency_hz| Band { frequency_hz, ..Band::flat(0, 1) }).collect();
        Self { spline: true, bands, ..Self::default() }
    }

    pub fn enable_spline(&mut self) -> Result<(), String> {
        self.bands.retain(|band| band.enabled);
        self.sort_bands();
        if self.bands.iter().filter(|band| !Self::is_edge_frequency(band.frequency_hz)).count() > MAX_BANDS - 2 {
            return Err("Spline mode needs room for its two fixed endpoints.".into());
        }
        for frequency in [MIN_FREQUENCY, MAX_FREQUENCY] {
            if !self.bands.iter().any(|band| band.frequency_hz == frequency) {
                self.bands.push(Band { frequency_hz: frequency, ..Band::flat(0, 1) });
            }
        }
        self.sort_bands();
        self.spline = true;
        self.validate()
    }

    pub fn is_edge_frequency(frequency: f64) -> bool {
        frequency == MIN_FREQUENCY || frequency == MAX_FREQUENCY
    }

    pub fn locked_point(&self, index: usize) -> bool {
        self.spline && (index == 0 || index == self.bands.len() - 1)
    }

    pub fn sort_bands(&mut self) {
        self.bands.sort_by(|left, right| left.frequency_hz.total_cmp(&right.frequency_hz));
    }

    pub fn replace_band(&mut self, index: usize, mut band: Band) -> usize {
        band.edited = Some(true);
        self.bands.remove(index);
        // Keep the previous relative order when several bands share a frequency.
        let first = self.bands.partition_point(|other| other.frequency_hz < band.frequency_hz);
        let last = self.bands.partition_point(|other| other.frequency_hz <= band.frequency_hz);
        let new_index = index.clamp(first, last);
        self.bands.insert(new_index, band);
        new_index
    }

    pub fn protected_band_count(&self, keep: usize) -> usize {
        let removed = if self.spline && keep < self.bands.len() {
            &self.bands[keep.max(2) - 1..self.bands.len() - 1]
        } else {
            self.bands.get(keep..).unwrap_or_default()
        };
        removed.iter().filter(|band| band.needs_removal_confirmation()).count()
    }

    pub fn automatic_peak_q(&self, index: usize, frequency: f64) -> Option<f64> {
        let nearest_gap = self.bands.iter().enumerate()
            .filter(|(other_index, _)| *other_index != index)
            .map(|(_, band)| (band.frequency_hz / frequency).ln().abs())
            .min_by(f64::total_cmp)?;
        // Center the bandwidth on the peak and place its nearer edge halfway
        // to the closest dot on the logarithmic frequency scale.
        let q = (1.0 / (2.0 * (nearest_gap / 2.0).sinh())).clamp(0.1, 20.0);
        Some((q * 100.0).round() / 100.0)
    }

    pub fn add_peak(&mut self, frequency: f64, gain: f64) -> bool {
        if self.bands.len() >= MAX_BANDS { return false; }
        let index = self.bands.partition_point(|band| band.frequency_hz <= frequency);
        if self.spline && (Self::is_edge_frequency(frequency)
            || index > 0 && self.bands[index - 1].frequency_hz == frequency)
        {
            return false;
        }
        self.bands.insert(index, Band {
            frequency_hz: frequency, gain_db: gain, edited: Some(true), ..Band::flat(0, 1)
        });
        if !self.spline && let Some(q) = self.automatic_peak_q(index, frequency) {
            self.bands[index].q = q;
        }
        true
    }

    pub fn validate(&self) -> Result<(), String> {
        fn within(v: f64, lo: f64, hi: f64) -> bool { v.is_finite() && (lo..=hi).contains(&v) }
        if self.schema_version != 1 { return Err("Unsupported JSON schema version".into()); }
        if self.name.trim().is_empty() || self.name.len() > 160 { return Err("Name must contain 1–160 characters".into()); }
        if !within(self.preamp_db, -60.0, 12.0) { return Err("Preamp must be between -60 and +12 dB".into()); }
        if !within(self.balance, -1.0, 1.0) { return Err("Balance must be between -1 and +1".into()); }
        if self.bands.len() > MAX_BANDS { return Err("A maximum of 32 bands is supported".into()); }
        if self.spline && (self.bands.first().map(|band| band.frequency_hz) != Some(MIN_FREQUENCY)
            || self.bands.last().map(|band| band.frequency_hz) != Some(MAX_FREQUENCY)
            || self.bands.windows(2).any(|pair| pair[0].frequency_hz >= pair[1].frequency_hz)
            || self.bands.iter().any(|band| !band.enabled))
        {
            return Err("Spline points must be enabled and ordered, from 20 Hz to 20 kHz.".into());
        }
        for (i, band) in self.bands.iter().enumerate() {
            if !["PK", "LS", "HS"].contains(&band.kind.as_str())
                || !within(band.frequency_hz, 20.0, 20000.0)
                || !within(band.gain_db, -24.0, 24.0)
                || !within(band.q, 0.1, 20.0) {
                return Err(format!("Band {}: use PK/LS/HS, 20–20000 Hz, ±24 dB, Q 0.1–20", i + 1));
            }
        }
        Ok(())
    }

    pub fn resize(&mut self, count: usize) {
        let count = count.clamp(if self.spline { 2 } else { 0 }, MAX_BANDS);
        if self.spline {
            if count < self.bands.len() {
                self.bands.drain(count - 1..self.bands.len() - 1);
            }
        } else {
            self.bands.truncate(count);
        }
        if self.bands.is_empty() {
            self.bands.extend((0..count).map(|index| Band::flat(index, count)));
            return;
        }
        while self.bands.len() < count {
            let mut frequencies = vec![20.0, 20000.0];
            frequencies.extend(self.bands.iter().map(|band| band.frequency_hz));
            frequencies.sort_by(f64::total_cmp);
            let widest = frequencies.windows(2)
                .max_by(|a, b| (a[1] / a[0]).total_cmp(&(b[1] / b[0])))
                .expect("at least both frequency limits");
            let frequency_hz = (widest[0] * widest[1]).sqrt();
            let gain_db = if self.spline { crate::spline::gain_at(&self.bands, frequency_hz) } else { 0.0 };
            self.bands.push(Band { frequency_hz, gain_db, ..Band::flat(0, 1) });
            self.sort_bands();
        }
    }

    pub fn gains(&self) -> (f64, f64) {
        if self.bypass { return (1.0, 1.0); }
        ((1.0 - self.balance.max(0.0)), (1.0 + self.balance.min(0.0)))
    }

    pub fn response(&self, frequencies: impl IntoIterator<Item = f64>, sample_rate: f64) -> Vec<f64> {
        if self.spline {
            frequencies.into_iter().map(|frequency| crate::spline::gain_at(&self.bands, frequency)).collect()
        } else {
            let bands: Vec<_> = self.bands.iter().map(Band::state).collect();
            resonance_ipc::fr::response_for_frequencies(&bands, frequencies, sample_rate)
        }
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        if text.len() > 1024 * 1024 { return Err("Config is larger than 1 MiB".into()); }
        let mut cfg: Self = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
        cfg.sort_bands();
        cfg.validate()?;
        cfg.preamp_db = cfg.preamp_db.max(-MANUAL_PREAMP_LIMIT_DB);
        Ok(cfg)
    }

    pub fn to_apo(&self) -> Result<String, String> {
        self.validate()?;
        let mut text = "# SimpleDarkEQ — supported parametric EQ subset\nChannel: all\n".to_string();
        // Retain parameters while exporting a bypassed config as inactive lines.
        let off = if self.bypass { "# " } else { "" };
        text.push_str(&format!("{off}Preamp: {} dB\n", self.preamp_db));
        if self.spline {
            let points = self.bands.iter().map(|band| format!("{} {}", band.frequency_hz, band.gain_db))
                .collect::<Vec<_>>().join("; ");
            text.push_str(&format!("{off}GraphicEQ: {points}\n"));
        } else {
            for (i, b) in self.bands.iter().enumerate() {
                text.push_str(&format!("{off}Filter {}: {} {} Fc {} Hz Gain {} dB Q {}\n",
                    i + 1, if b.enabled { "ON" } else { "OFF" }, b.kind, b.frequency_hz, b.gain_db, b.q));
            }
        }
        // Diagonal copy is exact at the balance extremes (zero means actual mute).
        let left = 1.0 - self.balance.max(0.0);
        let right = 1.0 + self.balance.min(0.0);
        if self.balance != 0.0 {
            text.push_str(&format!("{off}Copy: L={left}*L R={right}*R\n"));
        }
        Ok(text)
    }

    /// Intentionally strict: unsupported commands never disappear silently.
    pub fn from_apo(text: &str) -> Result<Self, String> {
        if text.len() > 1024 * 1024 { return Err("Config is larger than 1 MiB".into()); }
        let mut cfg = Self { name: "Imported APO".into(), description: "Imported from an Equalizer APO configuration.".into(), bands: vec![], ..Self::default() };
        let mut copied = false;
        for (line_no, raw) in text.trim_start_matches('\u{feff}').lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() { continue; }
            let err = || format!("APO line {}: unsupported command or filter syntax", line_no + 1);
            let number = |s: &str| s.parse::<f64>().map_err(|_| err());
            if line.eq_ignore_ascii_case("Channel: all") { continue; }
            if let Some((key, value)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("Preamp") {
                    let words: Vec<_> = value.split_whitespace().collect();
                    if words.len() != 2 || !words[1].eq_ignore_ascii_case("dB") { return Err(err()); }
                    cfg.preamp_db += number(words[0])?;
                    continue;
                }
                if key.eq_ignore_ascii_case("GraphicEQ") {
                    if copied || !cfg.bands.is_empty() { return Err(err()); }
                    for pair in value.trim().trim_end_matches(';').split(';') {
                        let words: Vec<_> = pair.split_whitespace().collect();
                        if words.len() != 2 { return Err(err()); }
                        cfg.bands.push(Band {
                            frequency_hz: number(words[0])?, gain_db: number(words[1])?, edited: None,
                            ..Band::flat(0, 1)
                        });
                    }
                    cfg.sort_bands();
                    let edges = [cfg.bands[0].gain_db, cfg.bands.last().unwrap().gain_db];
                    cfg.enable_spline()?;
                    cfg.bands.first_mut().unwrap().gain_db = edges[0];
                    cfg.bands.last_mut().unwrap().gain_db = edges[1];
                    continue;
                }
            }
            if let Some(c) = APO_FILTER.captures(line) {
                if copied || cfg.spline { return Err(err()); }
                let kind = c[2].to_uppercase();
                cfg.bands.push(Band { kind: if kind == "PEQ" { "PK".into() } else { kind },
                    frequency_hz: number(&c[3])?, gain_db: number(&c[4])?, q: number(&c[5])?,
                    enabled: c[1].eq_ignore_ascii_case("ON"), edited: None });
            } else if let Some(c) = APO_COPY.captures(line) {
                let (l, r) = (number(&c[1])?, number(&c[2])?);
                if copied || !(0.0..=1.0).contains(&l) || !(0.0..=1.0).contains(&r)
                    || (l != 1.0 && r != 1.0) { return Err(err()); }
                cfg.balance = if r == 1.0 { 1.0 - l } else { r - 1.0 };
                copied = true;
            } else { return Err(err()); }
        }
        cfg.validate()?;
        cfg.preamp_db = cfg.preamp_db.max(-MANUAL_PREAMP_LIMIT_DB);
        cfg.sort_bands();
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_manual_preamp_loads_at_the_new_lower_limit() {
        let mut cfg = Config::flat_spline();
        cfg.preamp_db = -30.0;
        let loaded = Config::from_json(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(loaded.preamp_db, -12.0);
        assert_eq!(loaded.bands, cfg.bands);
        assert_eq!(Config::from_apo("Preamp: -30 dB").unwrap().preamp_db, -12.0);
    }

    #[test] fn json_roundtrip_and_validation() {
        let cfg = Config::default();
        assert_eq!(Config::from_json(&serde_json::to_string(&cfg).unwrap()).unwrap(), cfg);
        let mut invalid = cfg;
        invalid.bands[0].q = f64::NAN;
        assert!(invalid.validate().is_err());
    }
    #[test] fn apo_roundtrip_preserves_disabled_bands_and_balance() {
        let mut cfg = Config::default();
        cfg.balance = -0.35; cfg.preamp_db = -5.0; cfg.bands[1].enabled = false;
        let result = Config::from_apo(&cfg.to_apo().unwrap()).unwrap();
        assert_eq!(cfg.bands.iter().map(Band::state).collect::<Vec<_>>(),
            result.bands.iter().map(Band::state).collect::<Vec<_>>());
        assert_eq!(cfg.preamp_db, result.preamp_db);
        assert!((cfg.balance - result.balance).abs() < 1e-9);
    }
    #[test] fn unsupported_apo_is_rejected() {
        for text in ["Include: other.txt", "Device: Speakers", "Channel: L\nPreamp: -3 dB",
            "GraphicEQ: 100 2; 100 3", "GraphicEQ: 100 2\nFilter: ON PK Fc 100 Hz Gain 4 dB Q 1"] {
            assert!(Config::from_apo(text).is_err());
        }
    }

    #[test]
    fn spline_resize_and_apo_roundtrip_preserve_fixed_endpoints() {
        let mut cfg = Config::from_apo("GraphicEQ: 100 1; 500 3; 1000 -2").unwrap();
        assert!(cfg.spline);
        assert_eq!(cfg.bands[0].gain_db, 1.0);
        assert_eq!(cfg.bands.last().unwrap().gain_db, -2.0);
        cfg.bands.last_mut().unwrap().edited = Some(true);
        assert_eq!(cfg.protected_band_count(2), 3);
        cfg.resize(0);
        assert_eq!(cfg.bands.len(), 2);
        assert!(cfg.locked_point(0) && cfg.locked_point(1));
        assert_eq!(cfg.bands[1].gain_db, -2.0);
        assert!(!cfg.add_peak(MIN_FREQUENCY, 10.0));
        cfg.resize(9);
        assert!(cfg.validate().is_ok());
        let apo = Config::from_apo(&cfg.to_apo().unwrap()).unwrap();
        assert!(apo.spline);
        for (expected, actual) in cfg.bands.iter().zip(apo.bands) {
            assert_eq!(expected.frequency_hz, actual.frequency_hz);
            assert_eq!(expected.gain_db, actual.gain_db);
        }
    }
    #[test] fn bypass_is_flat_and_balance_never_boosts() {
        let mut cfg = Config::default(); cfg.balance = 1.0;
        assert_eq!(cfg.gains(), (0.0, 1.0)); cfg.bypass = true;
        assert_eq!(cfg.gains(), (1.0, 1.0));
    }
    #[test] fn resize_preserves_edits() {
        let mut cfg = Config::default(); cfg.bands[0].gain_db = -4.0;
        let edited = cfg.bands[0].clone();
        cfg.resize(32); assert!(cfg.bands.contains(&edited));
        cfg.resize(100); assert_eq!(cfg.bands.len(), 32);
        cfg.resize(0); assert!(cfg.bands.is_empty());
    }

    #[test]
    fn growing_a_band_at_a_time_creates_distinct_handles() {
        let mut cfg = Config::default();
        let original = cfg.bands.clone();
        for count in 6..=MAX_BANDS { cfg.resize(count); }
        assert!(original.iter().all(|band| cfg.bands.contains(band)));
        assert!(cfg.bands.windows(2).all(|pair| pair[0].frequency_hz < pair[1].frequency_hz));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn crossing_and_crossing_back_keeps_the_same_filter_and_its_gain() {
        let mut cfg = Config::default();
        let mut moved = cfg.bands[0].clone();
        moved.kind = "LS".into();
        moved.gain_db = 5.0;
        moved.q = 0.7;
        moved.enabled = false;
        let other_bands = cfg.bands[1..].to_vec();
        moved.frequency_hz = 18_000.0;
        let right = cfg.replace_band(0, moved.clone());
        assert_eq!(right, 4);
        assert_eq!(cfg.bands[..right], other_bands);
        let mut moved = cfg.bands[right].clone();
        moved.frequency_hz = 25.0;
        assert_eq!(cfg.replace_band(right, moved.clone()), 0);
        assert_eq!(cfg.bands[0], moved);
        assert_eq!(cfg.bands[1..], other_bands);
    }

    #[test]
    fn removal_protects_edits_even_after_reset_and_reload() {
        let mut cfg = Config::default();
        assert_eq!(cfg.protected_band_count(4), 0);
        let original = cfg.bands[4].clone();
        let mut band = original.clone();
        band.gain_db = -3.5;
        cfg.replace_band(4, band);
        cfg.replace_band(4, original);
        let loaded = Config::from_json(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(loaded.protected_band_count(4), 1);
        assert_eq!(loaded.protected_band_count(5), 0);
    }

    #[test]
    fn older_unsorted_presets_are_ordered_and_their_filters_are_protected() {
        let mut cfg = Config::default();
        for band in &mut cfg.bands { band.edited = None; }
        cfg.bands.reverse();
        let loaded = Config::from_json(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert!(loaded.bands.windows(2).all(|pair| pair[0].frequency_hz < pair[1].frequency_hz));
        assert_eq!(loaded.protected_band_count(3), 2);
    }

    #[test]
    fn imports_utf8_bom_from_windows_editors() {
        let cfg = Config::default();
        let json = format!("\u{feff}{}", serde_json::to_string(&cfg).unwrap());
        assert_eq!(Config::from_json(&json).unwrap(), cfg);
        assert_eq!(Config::from_apo("\u{feff}Preamp: -5 dB\r\n").unwrap().preamp_db, -5.0);
    }
}
