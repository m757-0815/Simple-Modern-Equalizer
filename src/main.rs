#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod config;
mod engine;
mod graph;
mod listening;
mod live;
mod plot_canvas;
mod presets;
mod scope;
mod simple;
mod spectrum;
mod storage;
mod theme;
#[cfg(test)]
mod ui_tests;

use config::Config;
use plot_canvas::RasterSize;
use presets::{Preferences, Preset};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::{cell::RefCell, fs, path::{Path, PathBuf}, rc::Rc, time::{Duration, Instant}};
slint::include_modules!();

struct App {
    cfg: Config,
    simple: simple::SimpleSettings,
    presets: Vec<Preset>,
    selected: Option<usize>,
    preferences: Preferences,
    active_output: Option<String>,
    fallback: Option<(Config, Option<usize>, bool)>,
    reference: Option<Config>,
    listening_to_a: bool,
    effective: Config,
    engine: engine::Engine,
    outputs: Vec<String>,
    directory: PathBuf,
    edited: bool,
    plot: graph::GraphCache,
    drag_origin: Option<(usize, f64, f64)>,
    plot_spectrum: Vec<f32>,
    format: Option<engine::AudioFormat>,
}

impl App {
    fn save_graph_heights(&mut self, eq: f32, live: f32) -> Result<(), String> {
        let eq = eq.round().clamp(130.0, 520.0) as i32;
        let live = live.round().clamp(146.0, 420.0) as i32;
        if eq != self.preferences.eq_plot_height || live != self.preferences.live_view_height() as i32 {
            let mut preferences = self.preferences.clone();
            preferences.eq_plot_height = eq;
            preferences.live_view_height = Some(live);
            preferences.save(&self.directory)?;
            self.preferences = preferences;
        }
        Ok(())
    }

    fn apply_audio(&mut self) {
        // Keep the saved Standard bands intact while only Simple tone filters are active.
        let (rate, channels) = self.format.as_ref().map_or((48000.0, 2), |format| (format.sample_rate, format.channels));
        let config = if self.simple.active {
            self.simple.config(self.cfg.bypass, self.preferences.simple_auto_headroom)
        } else {
            let (b, a) = listening::prepare(&self.cfg, self.reference.as_ref(),
                self.preferences.auto_headroom, rate, channels);
            if self.listening_to_a { a.unwrap_or(b) } else { b }
        };
        if let Some(format) = &self.format {
            self.engine.submit(format, config.clone());
        }
        self.effective = config;
    }

    fn select_preset(&mut self, index: usize) {
        let mut config = self.presets[index].config.clone();
        config.bypass = self.cfg.bypass;
        config.show_live_view = self.cfg.show_live_view;
        self.cfg = config;
        self.selected = Some(index);
        self.listening_to_a = false;
        self.edited = false;
    }

    fn save_preset(&mut self, name: &str) -> Result<(), String> {
        let mut config = if self.simple.active {
            self.simple.config(self.cfg.bypass, self.preferences.simple_auto_headroom)
        } else {
            self.cfg.clone()
        };
        config.name = name.into();
        config.show_live_view = self.cfg.show_live_view;
        let preset = Preset::save_as(&self.directory.join("presets"), config)?;
        if !self.simple.active {
            self.cfg.name = name.into();
            self.selected = Some(self.presets.len());
            self.edited = false;
        }
        self.presets.push(preset);
        Ok(())
    }

    fn overwrite_preset(&mut self) -> Result<(), String> {
        if self.simple.active || self.listening_to_a {
            return Err("Open the preset in Standard and switch to B to overwrite it.".into());
        }
        let index = self.selected.ok_or("Select a custom preset first.")?;
        let preset = &mut self.presets[index];
        preset.overwrite(self.cfg.clone())?;
        self.cfg.name.clone_from(&preset.config.name);
        self.edited = false;
        if let Some((config, Some(selected), false)) = &mut self.fallback
            && *selected == index
        {
            config.clone_from(&preset.config);
        }
        Ok(())
    }

    fn change_output(&mut self, id: Option<String>) {
        if self.active_output == id {
            return;
        }
        let preset_index = id.as_ref().and_then(|id| self.preferences.outputs.get(id))
            .and_then(|preset| self.presets.iter().position(|entry| entry.id() == *preset));
        self.active_output = id;
        self.reference = None;
        self.listening_to_a = false;
        if let Some(index) = preset_index {
            if self.fallback.is_none() {
                self.fallback = Some((self.cfg.clone(), self.selected, self.edited));
            }
            self.select_preset(index);
        } else if let Some((mut config, selected, edited)) = self.fallback.take() {
            config.bypass = self.cfg.bypass;
            config.show_live_view = self.cfg.show_live_view;
            self.cfg = config;
            self.selected = selected;
            self.edited = edited;
        }
    }
}

fn strings(values: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    Rc::new(VecModel::from(values.into_iter().map(Into::into).collect::<Vec<_>>())).into()
}

fn band_row(band: &config::Band) -> BandRow {
    BandRow {
        frequency: format!("{:.1}", band.frequency_hz).into(),
        gain: band.gain_db as f32,
        gain_label: format!("{:+.1} dB", band.gain_db).into(),
        q: format!("{:.2}", band.q).into(),
        kind: match band.kind.as_str() { "LS" => 1, "HS" => 2, _ => 0 },
        enabled: band.enabled,
    }
}

fn paint_graph(ui: &MainWindow, app: &mut App) {
    if app.simple.active {
        return;
    }
    let extent = ui.get_eq_plot_extent();
    let Some(size) = RasterSize::from_logical(extent.width, extent.height, ui.window().scale_factor()) else {
        return;
    };
    let rate = app.format.as_ref().map_or(48000.0, |format| format.sample_rate);
    if let Some(frame) = app.plot.frame(&app.effective, &app.plot_spectrum, rate, size) {
        ui.set_graph(slint::Image::from_rgba8(frame));
    }
    if app.plot.update_dots(&app.effective) {
        let dots: Vec<_> = app.plot.dots.iter().map(|dot| GraphDot {
            x: dot.x as f32,
            y: dot.y as f32,
            enabled: dot.enabled,
        }).collect();
        ui.set_graph_dots(Rc::new(VecModel::from(dots)).into());
    }
}

fn refresh(ui: &MainWindow, app: &App) {
    let cfg = if app.listening_to_a { app.reference.as_ref().unwrap_or(&app.cfg) } else { &app.cfg };
    ui.set_config_name(format!("{}{}", cfg.name, if app.edited && !app.listening_to_a { " · edited" } else { "" }).into());
    ui.set_description(cfg.description.clone().into());
    refresh_preamp(ui, app, cfg);
    ui.set_balance(cfg.balance as f32);
    ui.set_balance_label(if cfg.balance.abs() < 0.005 { "Centered".into() }
        else { format!("{} {:.0}%", if cfg.balance < 0.0 { "Left" } else { "Right" }, cfg.balance.abs() * 100.0).into() });
    ui.set_bypass(app.cfg.bypass);
    ui.set_show_live_view(app.cfg.show_live_view);
    ui.set_band_count(cfg.bands.len() as i32);
    let rows = ui.get_bands();
    if rows.row_count() == cfg.bands.len() {
        for (index, band) in cfg.bands.iter().enumerate() {
            let row = band_row(band);
            if rows.row_data(index).as_ref() != Some(&row) {
                rows.set_row_data(index, row);
            }
        }
    } else {
        ui.set_bands(Rc::new(VecModel::from(cfg.bands.iter().map(band_row).collect::<Vec<_>>())).into());
    }
    ui.set_auto_headroom(if app.simple.active {
        app.preferences.simple_auto_headroom
    } else {
        app.preferences.auto_headroom
    });
    ui.set_listening_to_a(app.listening_to_a);
    ui.set_has_reference(app.reference.is_some());
    ui.set_preset_index(app.selected.map_or(-1, |i| i as i32));
    ui.set_can_rename(!app.listening_to_a && app.selected.is_some_and(|i| app.presets[i].is_custom()));
    let output_ready = ui.get_connected() && ui.get_active_output_label() != "";
    ui.set_can_assign(!app.listening_to_a && app.selected.is_some() && !app.edited && output_ready);
    let mapped = app.active_output.as_ref().and_then(|id| app.preferences.outputs.get(id));
    ui.set_output_mapping(mapped.and_then(|id| app.presets.iter().find(|preset| preset.id() == *id))
        .map(|preset| preset.config.name.clone()).unwrap_or_default().into());
    ui.set_can_unassign(mapped.is_some() && output_ready);
}

fn refresh_preamp(ui: &MainWindow, app: &App, cfg: &Config) {
    let applied = app.effective.preamp_db;
    let automatic = app.preferences.auto_headroom && !app.simple.active;
    let displayed = if automatic { applied } else { cfg.preamp_db };
    ui.set_preamp(displayed as f32);
    ui.set_preamp_entry(format!("{displayed:+.2}").into());
    ui.set_preamp_label(if app.effective.bypass {
        "Bypassed".into()
    } else if automatic {
        format!("Auto {applied:+.2} dB")
    } else if (cfg.preamp_db - applied).abs() >= 0.025 {
        format!("Applied {applied:+.2} dB")
    } else { "".into() }.into());
}

fn set_manual_preamp(ui: &MainWindow, app: &mut App, value: f64) {
    if app.preferences.auto_headroom || app.listening_to_a || !value.is_finite() {
        refresh_preamp(ui, app, &app.cfg);
        return;
    }
    let mut config = app.cfg.clone();
    config.preamp_db = (value.clamp(-60.0, 12.0) * 20.0).round() / 20.0;
    commit(ui, app, config);
    refresh_preamp(ui, app, &app.cfg);
}

fn sync(ui: &MainWindow, app: &mut App) {
    app.apply_audio();
    refresh(ui, app);
    paint_graph(ui, app);
}

fn refresh_preset_names(ui: &MainWindow, app: &App) {
    ui.set_presets(strings(app.presets.iter().map(|p| p.config.name.clone())));
}

fn commit(ui: &MainWindow, app: &mut App, candidate: Config) {
    match candidate.validate() {
        Ok(()) => {
            if app.cfg != candidate {
                app.cfg = candidate;
                app.edited = true;
                sync(ui, app);
            }
            ui.set_notice("".into());
        }
        Err(e) => { ui.set_notice(e.into()); refresh(ui, app); }
    }
}

fn edit_band(ui: &MainWindow, app: &mut App, index: i32, edit: impl FnOnce(&mut config::Band)) {
    if app.listening_to_a { return; }
    let mut config = app.cfg.clone();
    if let Some(band) = config.bands.get_mut(index as usize) {
        edit(band);
        commit(ui, app, config);
    }
}

fn data_directory() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| ".".into())).join("SimpleDarkEQ")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".config"))
            .join("simpledarkeq")
    }
}

fn read_config(path: &Path) -> Result<Config, String> {
    let text = storage::read_text(path).map_err(|e| e.to_string())?;
    let mut cfg = if path.extension().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("json")) {
        Config::from_json(&text)?
    } else { Config::from_apo(&text)? };
    if cfg.name == "Imported APO" { cfg.name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned(); }
    cfg.validate()?;
    Ok(cfg)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = data_directory();
    fs::create_dir_all(directory.join("presets"))?;
    let Some(_instance) = storage::lock_instance(&directory)? else {
        rfd::MessageDialog::new()
            .set_title("Simple Modern Equalizer")
            .set_description("Simple Modern Equalizer is already running.")
            .show();
        return Ok(());
    };
    // Before any UI/worker threads exist: isolate our IPC endpoint and engine
    // preferences. Windows APO registration is still the vendored engine's ID.
    unsafe {
        std::env::set_var("RESONANCE_SOCKET", directory.join("engine.sock"));
        std::env::set_var("XDG_CONFIG_HOME", directory.join("engine-config"));
    }
    let mut presets = vec![Preset::builtin(Config::default())];
    let examples: Vec<Config> = serde_json::from_str(include_str!("../presets/catalog.json"))?;
    for cfg in examples { cfg.validate().map_err(std::io::Error::other)?; presets.push(Preset::builtin(cfg)); }
    let mut cfg = read_config(&directory.join("current.json")).unwrap_or_default();
    if let Some(name) = cfg.name.strip_suffix(" - Example") {
        if presets.iter().any(|preset| preset.config.name == name && preset.config.description == cfg.description) {
            cfg.name = name.to_owned();
        }
    }
    presets.extend(Preset::load_custom(&directory.join("presets"))?);
    let selected = presets.iter().position(|preset| {
        let mut saved = preset.config.clone();
        saved.bypass = cfg.bypass;
        saved.show_live_view = cfg.show_live_view;
        saved == cfg
    });
    let edited = selected.is_none() && presets.iter().any(|preset| preset.config.name == cfg.name);
    let preferences: Preferences = storage::read_text(&directory.join("preferences.json")).ok()
        .and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
    let simple: simple::SimpleSettings = storage::read_text(&directory.join("simple.json")).ok()
        .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()).unwrap_or_default();
    let initial = if simple.active { simple.config(cfg.bypass, preferences.simple_auto_headroom) } else {
        listening::prepare(&cfg, None, preferences.auto_headroom, 48000.0, 2).0
    };
    let app = Rc::new(RefCell::new(App { engine: engine::Engine::start(), cfg, simple, presets,
        selected, preferences, active_output: None, fallback: None, reference: None, listening_to_a: false,
        effective: initial,
        outputs: vec![], directory, edited, plot: graph::GraphCache::default(), drag_origin: None,
        plot_spectrum: vec![], format: None }));
    let ui = MainWindow::new()?;
    let frequency_ticks = live::frequency_ticks().map(|(frequency, position)| {
        let label = match frequency {
            20 => "20 Hz".into(),
            20000 => "20 kHz".into(),
            1000.. => format!("{}k", frequency / 1000).into(),
            _ => frequency.to_string().into(),
        };
        FrequencyTick { position, label }
    }).collect::<Vec<_>>();
    ui.set_live_frequency_ticks(Rc::new(VecModel::from(frequency_ticks)).into());
    refresh(&ui, &app.borrow());
    paint_graph(&ui, &mut app.borrow_mut());
    {
        let state = app.borrow();
        ui.set_live_view_height(state.preferences.live_view_height());
        ui.set_eq_plot_height(state.preferences.eq_plot_height());
        ui.set_simple_layout(state.simple.active);
        ui.set_simple_bass(state.simple.bass.clamp(-12.0, 12.0));
        ui.set_simple_mid(state.simple.mid.clamp(-12.0, 12.0));
        ui.set_simple_treble(state.simple.treble.clamp(-12.0, 12.0));
        ui.set_simple_loudness(state.simple.loudness);
    }
    refresh_preset_names(&ui, &app.borrow());
    ui.set_outputs(strings(["Follow system default".into()]));

    macro_rules! bind {
        ($callback:ident, |$window:ident, $state:ident, $($arg:ident),*| $body:block) => {{
            let weak = ui.as_weak(); let shared = app.clone();
            ui.$callback(move |$($arg),*| { if let Some($window) = weak.upgrade() {
                #[allow(unused_mut)]
                let mut $state = shared.borrow_mut(); $body
            }});
        }};
    }
    bind!(on_change_layout, |w, a, simple| {
        a.simple.active = simple;
        w.set_simple_layout(simple);
        sync(&w, &mut a);
    });
    {
        let timer = slint::Timer::default();
        let weak = ui.as_weak();
        let app = app.clone();
        ui.on_change_graph_height(move || {
            let weak = weak.clone();
            let app = app.clone();
            timer.start(slint::TimerMode::SingleShot, Duration::from_millis(250), move || {
                if let Some(w) = weak.upgrade()
                    && let Err(error) = app.borrow_mut().save_graph_heights(w.get_eq_plot_height(), w.get_live_view_height())
                {
                    w.set_notice(error.into());
                }
            });
        });
    }
    bind!(on_change_tone, |w, a, bass, mid, treble, loudness| {
        a.simple.bass = bass;
        a.simple.mid = mid;
        a.simple.treble = treble;
        a.simple.loudness = loudness;
        if w.get_simple_layout() {
            a.apply_audio();
        }
    });
    bind!(on_change_preamp, |w, a, value| { set_manual_preamp(&w, &mut a, f64::from(value)); });
    bind!(on_enter_preamp, |w, a, value| {
        let value = value.trim().replace(',', ".").parse::<f64>().unwrap_or(f64::NAN);
        set_manual_preamp(&w, &mut a, value);
    });
    bind!(on_change_balance, |w, a, value| { let mut c = a.cfg.clone(); c.balance = f64::from(value); commit(&w, &mut a, c); });
    bind!(on_change_bypass, |w, a, value| { a.cfg.bypass = value; sync(&w, &mut a); });
    bind!(on_change_live_view, |_w, a, value| {
        a.cfg.show_live_view = value;
    });
    bind!(on_change_count, |w, a, count| {
        w.invoke_cancel_graph_drag();
        let mut c = a.cfg.clone();
        c.resize(count.max(0) as usize);
        commit(&w, &mut a, c);
    });
    bind!(on_edit_band_number, |w, a, index, is_frequency, text| {
        let text = text.trim();
        if let Some(row) = w.get_bands().row_data(index as usize) {
            let displayed = if is_frequency { row.frequency } else { row.q };
            if displayed == text {
                w.set_notice("".into());
                return;
            }
        }
        match text.parse::<f64>() {
            Ok(value) => edit_band(&w, &mut a, index, |band| {
                if is_frequency { band.frequency_hz = value; } else { band.q = value; }
            }),
            Err(_) => w.set_notice(if is_frequency { "Enter a frequency in Hz." } else { "Enter a numeric Q value." }.into()),
        }
    });
    bind!(on_edit_band_gain, |w, a, index, gain| {
        edit_band(&w, &mut a, index, |band| band.gain_db = (f64::from(gain) * 10.0).round() / 10.0);
    });
    bind!(on_edit_band_kind, |w, a, index, kind| {
        edit_band(&w, &mut a, index, |band| band.kind = match kind { 1 => "LS", 2 => "HS", _ => "PK" }.into());
    });
    bind!(on_edit_band_enabled, |w, a, index, enabled| {
        edit_band(&w, &mut a, index, |band| band.enabled = enabled);
    });
    {
        let app = app.clone();
        ui.on_hit_band(move |x, y, width, height| {
            let mut a = app.borrow_mut();
            let hit = graph::hit_band(&a.plot.dots, x.into(), y.into(), width.into(), height.into());
            a.drag_origin = hit.and_then(|index| a.cfg.bands.get(index).map(|band| (index, band.frequency_hz, band.gain_db)));
            a.drag_origin.map_or(-1, |(index, _, _)| index as i32)
        });
    }
    bind!(on_move_band, |w, a, index, horizontal_delta, vertical_delta| {
        let Some((hit, frequency, gain)) = a.drag_origin else { return; };
        if a.listening_to_a || hit != index as usize { return; }
        if let Some(band) = a.cfg.bands.get_mut(index as usize) {
            let mut edited = band.clone();
            (edited.frequency_hz, edited.gain_db) = graph::drag_values(
                frequency, gain, horizontal_delta.into(), vertical_delta.into(), band.kind != "PK");
            if *band == edited {
                return;
            }
            *band = edited;
            w.get_bands().set_row_data(index as usize, band_row(band));
            if !a.edited {
                a.edited = true;
                w.set_config_name(format!("{} · edited", a.cfg.name).into());
                w.set_can_assign(false);
            }
            w.set_notice("".into());
            a.apply_audio();
            refresh_preamp(&w, &a, &a.cfg);
            paint_graph(&w, &mut a);
        }
    });
    bind!(on_choose_preset, |w, a, index| {
        if a.presets.get(index as usize).is_some() {
            w.invoke_cancel_graph_drag();
            a.select_preset(index as usize);
            a.simple.active = false;
            w.set_simple_layout(false);
            w.set_notice("".into());
            sync(&w, &mut a);
        }
    });
    {
        let weak = ui.as_weak();
        let app = app.clone();
        ui.on_name_preset(move |rename, name| {
            let mut a = app.borrow_mut();
            let result = (|| -> Result<(), String> {
                if a.listening_to_a {
                    return Err("Switch to B to manage presets.".into());
                }
                let name = name.trim();
                let rename_index = if rename {
                    Some(a.selected.filter(|&i| a.presets[i].is_custom()).ok_or("Select one of your presets first.")?)
                } else { None };
                if a.presets.iter().enumerate().any(|(i, preset)| Some(i) != rename_index
                    && preset.config.name.to_lowercase() == name.to_lowercase()) {
                    return Err("That preset name is already in use.".into());
                }
                if let Some(index) = rename_index {
                    a.presets[index].rename(name.into())?;
                    a.cfg.name = name.into();
                    if let Some((config, Some(selected), _)) = &mut a.fallback {
                        if *selected == index { config.name = name.into(); }
                    }
                } else {
                    a.save_preset(name)?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    if let Some(w) = weak.upgrade() {
                        refresh_preset_names(&w, &a);
                        sync(&w, &mut a);
                        w.set_notice(if rename { "Preset renamed." } else { "Preset saved." }.into());
                    }
                    "".into()
                }
                Err(error) => error.into(),
            }
        });
    }
    {
        let weak = ui.as_weak();
        let app = app.clone();
        ui.on_overwrite_preset(move || {
            let mut a = app.borrow_mut();
            match a.overwrite_preset() {
                Ok(()) => {
                    if let Some(w) = weak.upgrade() {
                        refresh(&w, &a);
                    }
                    "".into()
                }
                Err(error) => error.into(),
            }
        });
    }
    bind!(on_assign_preset, |w, a, assign| {
        if w.get_connected() && w.get_active_output_label() != "" && let Some(output) = a.active_output.clone() {
            let mut preferences = a.preferences.clone();
            if assign {
                let Some(index) = a.selected.filter(|_| !a.edited && !a.listening_to_a) else { return; };
                preferences.outputs.insert(output, a.presets[index].id());
            } else {
                preferences.outputs.remove(&output);
            }
            match preferences.save(&a.directory) {
                Ok(()) => {
                    a.preferences = preferences;
                    if !assign { a.fallback = None; }
                    w.set_notice(if assign { "Preset linked to this output." } else { "Output link removed." }.into());
                    refresh(&w, &a);
                }
                Err(error) => w.set_notice(error.into()),
            }
        }
    });
    bind!(on_change_headroom, |w, a, enabled| {
        let mut preferences = a.preferences.clone();
        if a.simple.active {
            preferences.simple_auto_headroom = enabled;
        } else {
            preferences.auto_headroom = enabled;
        }
        match preferences.save(&a.directory) {
            Ok(()) => a.preferences = preferences,
            Err(error) => w.set_notice(error.into()),
        }
        sync(&w, &mut a);
    });
    bind!(on_capture_a, |w, a, | {
        a.reference = Some(a.cfg.clone());
        a.listening_to_a = false;
        sync(&w, &mut a);
    });
    bind!(on_listen_a, |w, a, listen| {
        w.invoke_cancel_graph_drag();
        a.listening_to_a = listen && a.reference.is_some();
        sync(&w, &mut a);
    });
    bind!(on_clear_reference, |w, a, | {
        a.reference = None;
        a.listening_to_a = false;
        sync(&w, &mut a);
    });
    bind!(on_choose_output, |w, a, index| {
        let target = if index <= 0 { None } else { a.outputs.get(index as usize - 1).cloned() };
        a.engine.shared.lock().unwrap().target = Some(target);
        w.set_status("Switching output…".into());
    });
    bind!(on_load_config, |w, a, | {
        if let Some(path) = rfd::FileDialog::new().add_filter("EQ configurations", &["json", "txt"]).pick_file() {
            match read_config(&path) {
                Ok(mut c) => {
                    c.bypass = a.cfg.bypass;
                    a.selected = None;
                    a.listening_to_a = false;
                    commit(&w, &mut a, c);
                }
                Err(e) => { w.set_notice(e.into()); }
            }
        }
    });
    bind!(on_save_config, |w, a, | {
        if let Some(path) = rfd::FileDialog::new().add_filter("Simple Modern Equalizer JSON", &["json"]).set_file_name("my-eq.json").save_file() {
            let result = serde_json::to_vec_pretty(&a.cfg).map_err(|e| e.to_string())
                .and_then(|text| storage::write_text(&path, &text).map_err(|e| e.to_string()));
            match result {
                Ok(()) => w.set_notice("Exported JSON.".into()),
                Err(e) => { w.set_notice(e.into()); }
            }
        }
    });
    bind!(on_export_apo, |w, a, | {
        if let Some(path) = rfd::FileDialog::new().add_filter("Equalizer APO", &["txt"]).set_file_name("simpledarkeq.txt").save_file() {
            match a.cfg.to_apo().and_then(|text| storage::write_text(&path, text.as_bytes()).map_err(|e| e.to_string())) {
                Ok(()) => w.set_notice("Exported APO. JSON retains the full editable config, including bypass.".into()),
                Err(e) => w.set_notice(e.into()),
            }
        }
    });

    let weak = ui.as_weak(); let tick_app = app.clone();
    let live_meters = RefCell::new(live::LiveMeters::default());
    let mut clip_until = Instant::now();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, live::FRAME_INTERVAL, move || {
        let Some(w) = weak.upgrade() else { return; };
        let mut a = tick_app.borrow_mut();
        let mode = w.get_live_mode();
        let show_live_view = w.get_show_live_view();
        let (snapshot, status, clipped, waveform) = {
            let mut shared = a.engine.shared.lock().unwrap();
            shared.scope_requested = show_live_view && mode == scope::MODE;
            // Standard uses the spectrum for its EQ overlay and clipping highlights.
            shared.spectrum_requested = !a.simple.active || (show_live_view && matches!(mode, 1 | 2));
            let snapshot = shared.state.as_ref().map(|state| {
                let outputs = (a.outputs != state.available_sinks).then(|| {
                    let names = state.available_sinks.iter().map(|name| state.sink_label(name)).collect::<Vec<_>>();
                    (state.available_sinks.clone(), names)
                });
                let label = state.active_output.as_ref().map(|id| state.sink_label(id));
                let spectrum = shared.spectrum_levels.as_ref().map_or_else(|| state.spectrum.clone(), |levels| levels.to_vec());
                (state.meters, engine::AudioFormat::from(state), spectrum, outputs,
                    state.preferred_output.clone(), label)
            });
            let waveform = if shared.scope_requested { shared.scope_samples.clone() } else { None };
            (snapshot, shared.status.clone(), std::mem::take(&mut shared.clipped), waveform)
        };
        let connection_changed = w.get_connected() != snapshot.is_some();
        w.set_status(status.into()); w.set_connected(snapshot.is_some());
        let (spectrum, stereo_rms) = if let Some((meters, format, spectrum, outputs, preferred, label)) = snapshot {
            w.set_output_peak_dbfs(live::sample_peak_dbfs(meters.out_peak));
            let signal = if meters.out_peak > 0.00001 { "Audio detected at engine" } else { "No signal detected yet" };
            w.set_signal(signal.into());
            if clipped {
                clip_until = Instant::now() + Duration::from_secs(2);
                let regions = spectrum::clip_regions(&spectrum).into_iter()
                    .map(|(start, end)| FrequencyRegion { start, end }).collect::<Vec<_>>();
                w.set_clip_regions(Rc::new(VecModel::from(regions)).into());
            }
            if let Some((ids, names)) = outputs {
                a.outputs = ids;
                w.set_outputs(strings(std::iter::once("Follow system default".into()).chain(names)));
            }
            let index = preferred.as_ref().filter(|id| !cfg!(windows) || Some(*id) == format.output.as_ref())
                .and_then(|id| a.outputs.iter().position(|x| x == id));
            w.set_output_index(index.map_or(0, |i| i as i32 + 1));
            let update = connection_changed || a.format.as_ref() != Some(&format);
            if a.active_output != format.output {
                w.invoke_dismiss_presets();
                w.invoke_cancel_graph_drag();
                a.change_output(format.output.clone());
                clip_until = Instant::now();
                w.set_notice("".into());
            }
            a.format = Some(format);
            w.set_active_output_label(label.unwrap_or_default().into());
            if update {
                a.apply_audio();
                refresh(&w, &a);
            }
            if w.get_active_output_label() == "" {
                w.set_can_assign(false);
                w.set_can_unassign(false);
            }
            (spectrum, meters.stereo_rms)
        } else {
            clip_until = Instant::now();
            w.set_output_peak_dbfs(live::SILENT_PEAK_DBFS);
            w.set_signal("Waiting for audio engine".into());
            w.set_active_output_label("".into());
            w.set_can_assign(false);
            w.set_can_unassign(false);
            (vec![], [0.0; 2])
        };
        let clipping = w.get_connected() && Instant::now() < clip_until;
        w.set_clipping(clipping);
        if !clipping && w.get_clip_regions().row_count() != 0 {
            w.set_clip_regions(ModelRc::default());
        }
        live::smooth_spectrum(&mut a.plot_spectrum, &spectrum);
        if show_live_view {
            let samples = if mode == scope::MODE {
                let scope_status = match waveform.as_deref() {
                    Some(samples) if samples.iter().any(|sample| sample.abs() > 0.00001) => "",
                    Some(_) => "No playback signal",
                    None => "Waiting for output capture",
                };
                w.set_scope_status(scope_status.into());
                waveform.as_deref().unwrap_or_default()
            } else {
                &[]
            };
            let extent = w.get_live_plot_extent();
            if let Some(size) = RasterSize::from_logical(extent.width, extent.height, w.window().scale_factor())
                && let Some(frame) = live_meters.borrow_mut().frame(mode, stereo_rms, &a.plot_spectrum, samples, size)
            {
                w.set_live_image(slint::Image::from_rgba8(frame));
            }
        }
        paint_graph(&w, &mut a);
    });
    ui.run()?;
    timer.stop();
    let mut a = app.borrow_mut();
    let mut current = a.fallback.as_ref().map_or_else(|| a.cfg.clone(), |(config, _, _)| config.clone());
    current.bypass = a.cfg.bypass;
    current.show_live_view = a.cfg.show_live_view;
    storage::write_text(&a.directory.join("current.json"), &serde_json::to_vec_pretty(&current)?)?;
    storage::write_text(&a.directory.join("simple.json"), &serde_json::to_vec_pretty(&a.simple)?)?;
    a.save_graph_heights(ui.get_eq_plot_height(), ui.get_live_view_height())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_tone_saves_preserve_standard_until_explicit_overwrite() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("simpledarkeq-layout-test-{}-{nonce}", std::process::id()));
        let preset_directory = directory.join("presets");
        fs::create_dir_all(&preset_directory).unwrap();
        let mut standard = Config { preamp_db: -8.0, balance: 0.3, ..Config::default() };
        standard.bands[0].gain_db = -4.0;
        standard.bands[1].enabled = false;
        let mut app = App {
            cfg: standard.clone(),
            simple: simple::SimpleSettings::default(),
            presets: vec![Preset::builtin(Config::default())],
            selected: Some(0),
            preferences: Preferences::default(),
            active_output: None,
            fallback: None,
            reference: None,
            listening_to_a: false,
            effective: standard.clone(),
            engine: engine::Engine::idle_for_test(),
            outputs: vec![],
            directory,
            edited: true,
            plot: graph::GraphCache::default(),
            drag_origin: None,
            plot_spectrum: vec![],
            format: None,
        };

        app.simple.active = true;
        app.simple.bass = 6.0;
        app.apply_audio();
        assert_eq!(app.effective, app.simple.config(false, true));
        app.save_preset("Simple bass boost").unwrap();
        let saved = Preset::load_custom(&preset_directory).unwrap();
        assert_eq!(saved[0].config.bands, app.effective.bands);
        assert_eq!(app.cfg, standard);
        assert_eq!(app.selected, Some(0));
        assert!(app.edited);

        app.simple.active = false;
        app.apply_audio();
        assert_eq!(app.effective, standard);

        assert!(app.overwrite_preset().is_err());
        app.save_preset("Standard tuning").unwrap();
        let index = app.selected.unwrap();
        let preset_id = app.presets[index].id();
        app.cfg.bands[0].gain_db = 2.5;
        app.edited = true;
        app.simple.active = true;
        assert!(app.overwrite_preset().is_err());
        app.simple.active = false;
        app.overwrite_preset().unwrap();
        assert!(!app.edited);
        assert_eq!(app.selected, Some(index));
        let saved = Preset::load_custom(&preset_directory).unwrap();
        assert_eq!(saved.len(), 2);
        let overwritten = saved.iter().find(|preset| preset.id() == preset_id).unwrap();
        assert_eq!(overwritten.config, app.cfg);
        assert_eq!(overwritten.config.name, "Standard tuning");

        for entry in fs::read_dir(&preset_directory).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        fs::remove_dir(preset_directory).unwrap();
        fs::remove_dir(app.directory).unwrap();
    }
}
