use resonance_ipc::{BandScope, BandState, BandType, ChannelMask};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

static APO_FILTER: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(
    r"(?i)^Filter(?:\s+\d+)?:\s*(ON|OFF)\s+(PK|PEQ|LS|HS)\s+Fc\s+([+\-\d.e]+)\s+Hz\s+Gain\s+([+\-\d.e]+)\s+dB\s+Q\s+([+\-\d.e]+)\s*$"
).unwrap());
static APO_COPY: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(
    r"(?i)^Copy:\s*L=([+\-\d.e]+)\*L\s+R=([+\-\d.e]+)\*R\s*$"
).unwrap());

pub const MAX_BANDS: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Band {
    pub kind: String,
    pub frequency_hz: f64,
    pub gain_db: f64,
    pub q: f64,
    pub enabled: bool,
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
            bands: (0..5).map(|i| Band::flat(i, 5)).collect(),
        }
    }
}

impl Band {
    pub fn flat(index: usize, count: usize) -> Self {
        let t = if count <= 1 { 0.5 } else { index as f64 / (count - 1) as f64 };
        Self { kind: "PK".into(), frequency_hz: 40.0 * 400.0_f64.powf(t), gain_db: 0.0, q: 1.0, enabled: true }
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
    pub fn validate(&self) -> Result<(), String> {
        fn within(v: f64, lo: f64, hi: f64) -> bool { v.is_finite() && (lo..=hi).contains(&v) }
        if self.schema_version != 1 { return Err("Unsupported JSON schema version".into()); }
        if self.name.trim().is_empty() || self.name.len() > 160 { return Err("Name must contain 1–160 characters".into()); }
        if !within(self.preamp_db, -60.0, 12.0) { return Err("Preamp must be between -60 and +12 dB".into()); }
        if !within(self.balance, -1.0, 1.0) { return Err("Balance must be between -1 and +1".into()); }
        if self.bands.len() > MAX_BANDS { return Err("A maximum of 32 bands is supported".into()); }
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
        let count = count.min(MAX_BANDS);
        self.bands.truncate(count);
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
            self.bands.push(Band { frequency_hz: (widest[0] * widest[1]).sqrt(), ..Band::flat(0, 1) });
        }
    }

    pub fn gains(&self) -> (f64, f64) {
        if self.bypass { return (1.0, 1.0); }
        ((1.0 - self.balance.max(0.0)), (1.0 + self.balance.min(0.0)))
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        if text.len() > 1024 * 1024 { return Err("Config is larger than 1 MiB".into()); }
        let cfg: Self = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_apo(&self) -> Result<String, String> {
        self.validate()?;
        let mut text = "# SimpleDarkEQ — supported parametric EQ subset\nChannel: all\n".to_string();
        // Retain parameters while exporting a bypassed config as inactive lines.
        let off = if self.bypass { "# " } else { "" };
        text.push_str(&format!("{off}Preamp: {} dB\n", self.preamp_db));
        for (i, b) in self.bands.iter().enumerate() {
            text.push_str(&format!("{off}Filter {}: {} {} Fc {} Hz Gain {} dB Q {}\n",
                i + 1, if b.enabled { "ON" } else { "OFF" }, b.kind, b.frequency_hz, b.gain_db, b.q));
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
            }
            if let Some(c) = APO_FILTER.captures(line) {
                if copied { return Err("APO filters after a balance Copy are unsupported".into()); }
                let kind = c[2].to_uppercase();
                cfg.bands.push(Band { kind: if kind == "PEQ" { "PK".into() } else { kind },
                    frequency_hz: number(&c[3])?, gain_db: number(&c[4])?, q: number(&c[5])?,
                    enabled: c[1].eq_ignore_ascii_case("ON") });
            } else if let Some(c) = APO_COPY.captures(line) {
                let (l, r) = (number(&c[1])?, number(&c[2])?);
                if copied || !(0.0..=1.0).contains(&l) || !(0.0..=1.0).contains(&r)
                    || (l != 1.0 && r != 1.0) { return Err(err()); }
                cfg.balance = if r == 1.0 { 1.0 - l } else { r - 1.0 };
                copied = true;
            } else { return Err(err()); }
        }
        cfg.validate()?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(cfg.bands, result.bands);
        assert_eq!(cfg.preamp_db, result.preamp_db);
        assert!((cfg.balance - result.balance).abs() < 1e-9);
    }
    #[test] fn unsupported_apo_is_rejected() {
        for text in ["Include: other.txt", "Device: Speakers", "Channel: L\nPreamp: -3 dB", "GraphicEQ: 20 1; 100 2"] {
            assert!(Config::from_apo(text).is_err());
        }
    }
    #[test] fn bypass_is_flat_and_balance_never_boosts() {
        let mut cfg = Config::default(); cfg.balance = 1.0;
        assert_eq!(cfg.gains(), (0.0, 1.0)); cfg.bypass = true;
        assert_eq!(cfg.gains(), (1.0, 1.0));
    }
    #[test] fn resize_preserves_edits() {
        let mut cfg = Config::default(); cfg.bands[0].gain_db = -4.0;
        cfg.resize(32); assert_eq!(cfg.bands[0].gain_db, -4.0);
        cfg.resize(100); assert_eq!(cfg.bands.len(), 32);
        cfg.resize(0); assert!(cfg.bands.is_empty());
    }

    #[test]
    fn growing_a_band_at_a_time_creates_distinct_handles() {
        let mut cfg = Config::default();
        let original = cfg.bands.clone();
        for count in 6..=MAX_BANDS { cfg.resize(count); }
        assert_eq!(&cfg.bands[..5], original.as_slice());
        let mut frequencies: Vec<_> = cfg.bands.iter().map(|band| band.frequency_hz).collect();
        frequencies.sort_by(f64::total_cmp);
        assert!(frequencies.windows(2).all(|pair| pair[1] > pair[0]));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn imports_utf8_bom_from_windows_editors() {
        let cfg = Config::default();
        let json = format!("\u{feff}{}", serde_json::to_string(&cfg).unwrap());
        assert_eq!(Config::from_json(&json).unwrap(), cfg);
        assert_eq!(Config::from_apo("\u{feff}Preamp: -5 dB\r\n").unwrap().preamp_db, -5.0);
    }
}
