use crate::{Config, read_config, storage};
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, BTreeSet}, fs, io, path::{Path, PathBuf}, time::{SystemTime, UNIX_EPOCH}};

pub const DEFAULT_EQ_PLOT_HEIGHT: i32 = 260;

#[derive(Clone)]
pub struct Preset {
    pub config: Config,
    path: Option<PathBuf>,
}

impl Preset {
    pub fn builtin(config: Config) -> Self {
        Self { config, path: None }
    }

    pub fn bundled() -> Result<Vec<Self>, String> {
        let catalog: Vec<Config> = serde_json::from_str(include_str!("../presets/catalog.json"))
            .map_err(|error| error.to_string())?;
        let mut presets = vec![Self::builtin(Config::flat_spline())];
        for mut config in catalog {
            config.validate()?;
            config.sort_bands();
            presets.push(Self::builtin(config));
        }
        Ok(presets)
    }

    pub fn id(&self) -> String {
        match &self.path {
            Some(path) => format!("user:{}", path.file_name().unwrap_or_default().to_string_lossy()),
            None => format!("builtin:{}", self.saved_builtin_name()),
        }
    }

    fn saved_builtin_name(&self) -> &str {
        // Keep the original keys used by saved per-output assignments.
        self.config.name.strip_prefix("Beyerdynamic ")
            .filter(|name| name.starts_with("DT 770 ") || name.starts_with("DT 990 "))
            .unwrap_or(&self.config.name)
    }

    pub fn restore_builtin_name(presets: &[Self], current: &mut Config) {
        let name = current.name.strip_suffix(" - Example").unwrap_or(&current.name);
        if let Some(preset) = presets.iter().find(|preset| {
            !preset.is_custom() && preset.config.description == current.description
                && (preset.saved_builtin_name() == name || preset.config.name == name)
        }) {
            current.name.clone_from(&preset.config.name);
        }
    }

    pub fn is_custom(&self) -> bool {
        self.path.is_some()
    }

    pub fn load_custom(directory: &Path) -> io::Result<Vec<Self>> {
        let mut custom: Vec<_> = fs::read_dir(directory)?.flatten().map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("json")))
            .filter_map(|path| read_config(&path).ok().map(|config| Self { config, path: Some(path) }))
            .collect();
        custom.sort_by(|a, b| a.config.name.cmp(&b.config.name).then(a.path.cmp(&b.path)));
        Ok(custom)
    }

    pub fn save_as(directory: &Path, config: Config) -> Result<Self, String> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
        let path = directory.join(format!("preset-{stamp}-{}.json", std::process::id()));
        let preset = Self { config, path: Some(path) };
        preset.write(&preset.config)?;
        Ok(preset)
    }

    pub fn rename(&mut self, name: String) -> Result<(), String> {
        let mut config = self.config.clone();
        config.name = name;
        self.write(&config)?;
        self.config = config;
        Ok(())
    }

    pub fn overwrite(&mut self, mut config: Config) -> Result<(), String> {
        config.name.clone_from(&self.config.name);
        self.write(&config)?;
        self.config = config;
        Ok(())
    }

    pub fn delete_file(&self) -> Result<(), String> {
        let path = self.path.as_ref().ok_or("This built-in preset has no preset file.")?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("Could not delete the preset file ({error}).")),
        }
    }

    fn write(&self, config: &Config) -> Result<(), String> {
        config.validate()?;
        let path = self.path.as_ref().ok_or("Save a copy of this built-in preset to edit it.")?;
        let bytes = serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?;
        storage::write_text(path, &bytes).map_err(|e| e.to_string())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub auto_headroom: bool,
    pub simple_auto_headroom: bool,
    pub phase_mode: crate::phase::Mode,
    pub outputs: BTreeMap<String, String>,
    pub hidden_builtin_presets: BTreeSet<String>,
    pub live_size: i32,
    pub live_view_height: Option<i32>,
    pub eq_plot_height: i32,
    pub eq_range_db: i32,
    pub display: DisplayPreferences,
    pub colors: crate::theme::ThemeColors,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DisplayPreferences {
    // Keep the on-disk ids for the remaining modes. Zero and one were Analog VU and Spectrum.
    pub live_mode: i32,
    pub band_list: bool,
    pub levels: bool,
    pub clipping: bool,
    pub preset_button: bool,
    pub eq_spectrum: bool,
}

impl DisplayPreferences {
    pub fn live_mode_index(&self) -> i32 {
        self.live_mode.saturating_sub(2).clamp(0, crate::live::STEREO_FIELD_MODE)
    }
}

impl Default for DisplayPreferences {
    fn default() -> Self {
        Self {
            live_mode: 2, band_list: true, levels: true,
            clipping: true, preset_button: true, eq_spectrum: false,
        }
    }
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            auto_headroom: false,
            simple_auto_headroom: true,
            phase_mode: crate::phase::Mode::default(),
            outputs: BTreeMap::new(),
            hidden_builtin_presets: BTreeSet::new(),
            live_size: 1,
            live_view_height: None,
            eq_plot_height: DEFAULT_EQ_PLOT_HEIGHT,
            eq_range_db: 24,
            display: DisplayPreferences::default(),
            colors: crate::theme::ThemeColors::default(),
        }
    }
}

impl Preferences {
    pub fn live_view_height(&self) -> f32 {
        let legacy = match self.live_size { 0 => 146, 2 => 300, _ => 216 };
        self.live_view_height.unwrap_or(legacy).clamp(146, 420) as f32
    }

    pub fn eq_plot_height(&self) -> f32 {
        self.eq_plot_height.max(130) as f32
    }

    pub fn eq_range_db(&self) -> i32 {
        self.eq_range_db.clamp(1, 24)
    }

    pub fn save(&self, directory: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        storage::write_text(&directory.join("preferences.json"), &bytes).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_with_saved_test_tone_settings_still_load() {
        let preferences: Preferences = serde_json::from_str(r#"{
            "eq_range_db": 6,
            "live_view_height": 280,
            "test_tone": {"visible": false, "height": 160, "settings": {"frequency": 220}}
        }"#).unwrap();
        assert_eq!(preferences.eq_range_db(), 6);
        assert_eq!(preferences.live_view_height(), 280.0);
    }

    #[test]
    fn saved_live_display_ids_survive_removed_displays() {
        for (saved, expected) in [
            (0, crate::live::BARS_MODE),
            (1, crate::live::BARS_MODE),
            (2, crate::live::BARS_MODE),
            (3, crate::scope::MODE),
            (4, crate::live::STEREO_LEVEL_MODE),
            (5, crate::live::BEFORE_AFTER_MODE),
            (6, crate::live::LOUDNESS_HISTORY_MODE),
            (7, crate::live::STEREO_FIELD_MODE),
        ] {
            let display: DisplayPreferences =
                serde_json::from_str(&format!(r#"{{"live_mode":{saved}}}"#)).unwrap();
            assert_eq!(display.live_mode_index(), expected);
        }
    }


    #[test]
    fn bundled_catalog_is_valid_and_preserves_legacy_keys() {
        let presets = Preset::bundled().unwrap();
        assert_eq!(presets.iter().map(Preset::id).collect::<Vec<_>>(), [
            "builtin:Flat",
            "builtin:DT 770 PRO - 32 ohms",
            "builtin:DT 770 PRO - 80 ohms",
            "builtin:DT 770 PRO - 250 ohms",
            "builtin:DT 990 PRO - 80 ohms",
            "builtin:DT 990 PRO - 250 ohms",
            "builtin:DT 990 PRO Black Edition - 80 ohms",
            "builtin:DT 990 PRO Black Edition - 250 ohms",
            "builtin:DT 990 Edition - 32 ohms",
            "builtin:DT 990 Edition - 250 ohms",
            "builtin:DT 990 Edition - 600 ohms",
            "builtin:DT 990 PRO X - 48 ohms",
        ]);
        assert_eq!(presets[0].config, Config::flat_spline());

        let old_name = "DT 990 PRO - 250 ohms";
        let preset = presets.iter().find(|preset| preset.config.name == format!("Beyerdynamic {old_name}")).unwrap();
        assert_eq!(preset.id(), format!("builtin:{old_name}"));
        for name in [old_name.to_owned(), format!("{old_name} - Example")] {
            let mut current = preset.config.clone();
            current.name = name;
            current.bands[0].gain_db = -2.0;
            Preset::restore_builtin_name(&presets, &mut current);
            assert_eq!(current.name, preset.config.name);
            assert_eq!(current.bands[0].gain_db, -2.0);
        }
    }

    #[test]
    fn output_assignment_survives_rename_and_reload() {
        let directory = std::env::temp_dir().join(format!("simpledarkeq-preset-test-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let mut preset = Preset::save_as(&directory, Config::default()).unwrap();
        let id = preset.id();
        let mut preferences = Preferences::default();
        let legacy = serde_json::from_str::<Preferences>(r#"{"display":{"eq_graph":false}}"#).unwrap();
        assert_eq!(legacy.eq_plot_height(), 260.0);
        assert_eq!(legacy.eq_range_db(), 24);
        assert_eq!(legacy.colors, crate::theme::ThemeColors::default());
        preferences.colors = crate::theme::ThemeColors::parse("#AABBCC").unwrap();
        preferences.eq_plot_height = 1600;
        preferences.eq_range_db = 1;
        preferences.outputs.insert("DAC endpoint ID".into(), id.clone());
        preferences.save(&directory).unwrap();
        preset.rename("Late night".into()).unwrap();
        let loaded = Preset::load_custom(&directory).unwrap();
        let settings: Preferences = serde_json::from_str(&storage::read_text(&directory.join("preferences.json")).unwrap()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].config.name, "Late night");
        assert_eq!(loaded[0].id(), settings.outputs["DAC endpoint ID"]);
        assert_eq!(settings.eq_plot_height(), 1600.0);
        assert_eq!(settings.eq_range_db(), 1);
        assert_eq!(settings.colors, preferences.colors);
        fs::remove_file(preset.path.unwrap()).unwrap();
        fs::remove_file(directory.join("preferences.json")).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
