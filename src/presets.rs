use crate::{Config, read_config, storage};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, path::{Path, PathBuf}, time::{SystemTime, UNIX_EPOCH}};

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

    pub fn id(&self) -> String {
        match &self.path {
            Some(path) => format!("user:{}", path.file_name().unwrap_or_default().to_string_lossy()),
            None => format!("builtin:{}", self.config.name),
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
    pub outputs: BTreeMap<String, String>,
    pub live_size: i32,
    pub live_view_height: Option<i32>,
    pub eq_plot_height: i32,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            auto_headroom: false,
            simple_auto_headroom: true,
            outputs: BTreeMap::new(),
            live_size: 1,
            live_view_height: None,
            eq_plot_height: DEFAULT_EQ_PLOT_HEIGHT,
        }
    }
}

impl Preferences {
    pub fn live_view_height(&self) -> f32 {
        let legacy = match self.live_size { 0 => 146, 2 => 300, _ => 216 };
        self.live_view_height.unwrap_or(legacy).clamp(146, 420) as f32
    }

    pub fn eq_plot_height(&self) -> f32 {
        self.eq_plot_height.clamp(130, 520) as f32
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
    fn output_assignment_survives_rename_and_reload() {
        let directory = std::env::temp_dir().join(format!("simpledarkeq-preset-test-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let mut preset = Preset::save_as(&directory, Config::default()).unwrap();
        let id = preset.id();
        let mut preferences = Preferences::default();
        assert_eq!(serde_json::from_str::<Preferences>("{}").unwrap().eq_plot_height(), 260.0);
        preferences.eq_plot_height = 410;
        preferences.outputs.insert("DAC endpoint ID".into(), id.clone());
        preferences.save(&directory).unwrap();
        preset.rename("Late night".into()).unwrap();
        let loaded = Preset::load_custom(&directory).unwrap();
        let settings: Preferences = serde_json::from_str(&storage::read_text(&directory.join("preferences.json")).unwrap()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].config.name, "Late night");
        assert_eq!(loaded[0].id(), settings.outputs["DAC endpoint ID"]);
        assert_eq!(settings.eq_plot_height(), 410.0);
        fs::remove_file(preset.path.unwrap()).unwrap();
        fs::remove_file(directory.join("preferences.json")).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
