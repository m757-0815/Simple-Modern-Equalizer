#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#[cfg(target_os = "linux")]
mod autostart;
mod config;
mod engine;
mod fit;
mod graph;
mod listening;
mod live;
mod monitor;
mod plot_canvas;
mod presets;
mod scope;
mod simple;
mod spectrum;
mod spline;
mod storage;
mod theme;
#[cfg(test)]
mod ui_tests;

use config::Config;
use plot_canvas::RasterSize;
use presets::{DisplayPreferences, Preferences, Preset};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::{cell::RefCell, collections::VecDeque, fs, path::{Path, PathBuf}, rc::Rc, sync::mpsc, time::{Duration, Instant}};
slint::include_modules!();

const UNDO_LIMIT: usize = 15;

#[derive(Clone, Copy, PartialEq)]
enum EditGroup {
    GraphDrag,
    BandGain(i32),
    Preamp,
    Balance,
    Tone,
}

struct UndoState {
    config: Config,
    simple: simple::SimpleSettings,
    selected: Option<usize>,
    edited: bool,
    auto_headroom: bool,
    simple_auto_headroom: bool,
}

struct PendingFit {
    original: Config,
    rate: u32,
    result: mpsc::Receiver<(Config, f64)>,
    superseded: bool,
}

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
    drag_origin: Option<graph::BandDrag>,
    pending_band_removal: Option<(Config, Config)>,
    plot_spectrum: Vec<f32>,
    format: Option<engine::AudioFormat>,
    undo: VecDeque<UndoState>,
    edit_group: Option<EditGroup>,
    pending_fit: Option<PendingFit>,
}

impl App {
    fn undo_state(&self) -> UndoState {
        UndoState {
            config: self.cfg.clone(),
            simple: self.simple.clone(),
            selected: self.selected,
            edited: self.edited,
            auto_headroom: self.preferences.auto_headroom,
            simple_auto_headroom: self.preferences.simple_auto_headroom,
        }
    }

    fn remember_current(&mut self, group: Option<EditGroup>) {
        let previous = self.undo_state();
        self.remember(previous, group);
    }

    fn remember(&mut self, previous: UndoState, group: Option<EditGroup>) {
        if let Some(job) = &mut self.pending_fit {
            job.superseded = true;
        }
        if group.is_some() && self.edit_group == group {
            return;
        }
        if self.undo.len() == UNDO_LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(previous);
        self.edit_group = group;
    }

    fn undo_edit(&mut self) -> Result<bool, String> {
        let Some(previous) = self.undo.back() else { return Ok(false); };
        if self.preferences.auto_headroom != previous.auto_headroom
            || self.preferences.simple_auto_headroom != previous.simple_auto_headroom
        {
            let mut preferences = self.preferences.clone();
            preferences.auto_headroom = previous.auto_headroom;
            preferences.simple_auto_headroom = previous.simple_auto_headroom;
            preferences.save(&self.directory)?;
            self.preferences = preferences;
        }
        let previous = self.undo.pop_back().unwrap();
        let show_live_view = self.cfg.show_live_view;
        self.cfg = previous.config;
        self.cfg.show_live_view = show_live_view;
        self.simple = previous.simple;
        self.selected = previous.selected;
        self.edited = previous.edited;
        self.listening_to_a = false;
        self.drag_origin = None;
        self.pending_band_removal = None;
        self.edit_group = None;
        if let Some(job) = &mut self.pending_fit {
            job.superseded = true;
        }
        Ok(true)
    }

    fn fit_rate(&self) -> u32 {
        self.format.as_ref().map_or(48000, |format| format.sample_rate.round() as u32)
    }

    fn auto_headroom(&self) -> bool {
        if self.simple.active { self.preferences.simple_auto_headroom } else { self.preferences.auto_headroom }
    }

    fn set_auto_headroom(&mut self, enabled: bool) -> Result<(), String> {
        let changed = self.auto_headroom() != enabled;
        let mut preferences = self.preferences.clone();
        if self.simple.active {
            preferences.simple_auto_headroom = enabled;
        } else {
            preferences.auto_headroom = enabled;
        }
        preferences.save(&self.directory)?;

        if changed && self.simple.active {
            self.simple.preamp_db = self.simple.config(false, true, self.cfg.balance).preamp_db.max(-config::MANUAL_PREAMP_LIMIT_DB);
        } else if changed {
            let (rate, channels) = self.format.as_ref().map_or((48000.0, 2), |format| (format.sample_rate, format.channels));
            let active = Config { bypass: false, ..self.cfg.clone() };
            let automatic = listening::prepare(&active, None, true, rate, channels).0.preamp_db;
            let manual = automatic.max(-config::MANUAL_PREAMP_LIMIT_DB);
            if manual != self.cfg.preamp_db {
                self.cfg.preamp_db = manual;
                self.edited = true;
            }
        }
        self.preferences = preferences;
        Ok(())
    }

    fn save_graph_heights(&mut self, eq: f32, live: f32) -> Result<(), String> {
        let eq = eq.round().max(130.0) as i32;
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
            self.simple.config(self.cfg.bypass, self.preferences.simple_auto_headroom, self.cfg.balance)
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

    fn import_apo(&mut self, path: &Path) -> Result<(), String> {
        let mut config = read_apo_config(path)?;
        let base_name = config.name.clone();
        let mut number = 1;
        while self.presets.iter().any(|preset| preset.config.name.to_lowercase() == config.name.to_lowercase()) {
            number += 1;
            let suffix = format!(" ({number})");
            let end = base_name.floor_char_boundary(config::MAX_NAME_BYTES - suffix.len());
            config.name = format!("{}{suffix}", &base_name[..end]);
        }
        let preset = Preset::save_as(&self.directory.join("presets"), config)?;
        self.presets.push(preset);
        self.select_preset(self.presets.len() - 1);
        self.simple.active = false;
        Ok(())
    }

    fn save_preset(&mut self, name: &str) -> Result<(), String> {
        let mut config = if self.simple.active {
            self.simple.config(self.cfg.bypass, self.preferences.simple_auto_headroom, self.cfg.balance)
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
        if self.active_output.is_some() {
            self.undo.clear();
            self.edit_group = None;
        }
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

fn text_model(values: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    Rc::new(VecModel::from(values.into_iter().map(Into::into).collect::<Vec<_>>())).into()
}

fn band_row(cfg: &Config, index: usize) -> BandRow {
    let band = &cfg.bands[index];
    BandRow {
        frequency: format!("{:.1}", band.frequency_hz).into(),
        gain: band.gain_db as f32,
        gain_label: format!("{:+.1} dB", band.gain_db).into(),
        q: format!("{:.2}", band.q).into(),
        kind: match band.kind.as_str() { "LS" => 1, "HS" => 2, _ => 0 },
        enabled: band.enabled,
        locked: cfg.locked_point(index),
    }
}

fn draw_eq_graph(ui: &MainWindow, app: &mut App) {
    if app.simple.active {
        return;
    }
    let extent = ui.get_eq_plot_extent();
    let Some(size) = RasterSize::from_logical(extent.width, extent.height, ui.window().scale_factor()) else {
        return;
    };
    let rate = app.format.as_ref().map_or(48000.0, |format| format.sample_rate);
    let overlay = if ui.get_show_eq_spectrum() { &app.plot_spectrum[..] } else { &[] };
    app.plot.set_colors(app.preferences.colors);
    if let Some(frame) = app.plot.frame(&app.effective, overlay, rate, size) {
        ui.set_graph(slint::Image::from_rgba8(frame));
        let (bottom, top) = app.plot.output_bounds();
        let label = |gain: f64| -> SharedString {
            let rounded = (gain * 100.0).round() / 100.0;
            if rounded == 0.0 { "0".into() } else { format!("{rounded:+}").into() }
        };
        ui.set_eq_bottom_db(label(bottom));
        ui.set_eq_top_db(label(top));
        ui.set_eq_zero_position(app.plot.zero_position() as f32);
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

fn apply_colors(ui: &MainWindow, colors: theme::ThemeColors) {
    let appearance = ui.global::<Appearance>();
    let [red, green, blue] = colors.accent;
    let accent = slint::Color::from_rgb_u8(red, green, blue);
    appearance.set_accent(accent);
    appearance.set_accent_hex(theme::ThemeColors::hex(colors.accent).into());
    appearance.set_app_icon(plot_canvas::app_icon(colors.accent));
    // Slint 1.18 reads the built-in widget accent from the window context.
    slint::private_unstable_api::re_exports::WindowInner::from_pub(ui.window())
        .context().set_accent_color(accent);
}

fn keep_widget_accent(ui: &MainWindow) -> slint::private_unstable_api::re_exports::ChangeTracker {
    use slint::private_unstable_api::re_exports::{ChangeTracker, WindowInner};

    // Slint's desktop observer can replace the widget accent after startup.
    let tracker = ChangeTracker::default();
    tracker.init(ui.as_weak(), |weak| {
        let ui = weak.upgrade()?;
        let selected = ui.global::<Appearance>().get_accent();
        let widget_accent = WindowInner::from_pub(ui.window()).context().accent_color();
        Some((selected, widget_accent))
    }, |weak, accents| {
        if let Some((selected, widget_accent)) = *accents
            && selected != widget_accent
            && let Some(ui) = weak.upgrade()
        {
            WindowInner::from_pub(ui.window()).context().set_accent_color(selected);
        }
    });
    tracker
}

fn update_controls(ui: &MainWindow, app: &App) {
    let cfg = if app.listening_to_a { app.reference.as_ref().unwrap_or(&app.cfg) } else { &app.cfg };
    ui.set_graph_edited(app.edited && !app.listening_to_a);
    ui.set_can_undo(!app.undo.is_empty());
    ui.set_fitting_eq(app.pending_fit.is_some());
    if app.cfg.fit_target.is_none() { ui.set_fit_detail("".into()); }
    refresh_preamp(ui, app, cfg);
    ui.set_balance(cfg.balance as f32);
    ui.set_balance_label(if cfg.balance.abs() < 0.005 { "Centered".into() }
        else { format!("{} {:.0}%", if cfg.balance < 0.0 { "Left" } else { "Right" }, cfg.balance.abs() * 100.0).into() });
    ui.set_bypass(app.cfg.bypass);
    ui.set_show_live_view(app.cfg.show_live_view);
    ui.set_spline_eq(cfg.spline);
    ui.set_band_count(cfg.bands.len() as i32);
    refresh_band_rows(ui, cfg);
    ui.set_auto_headroom(app.auto_headroom());
    ui.set_listening_to_a(app.listening_to_a);
    ui.set_preset_index(app.selected.map_or(-1, |i| i as i32));
    ui.set_can_rename(!app.listening_to_a && app.selected.is_some_and(|i| app.presets[i].is_custom()));
    let output_ready = ui.get_connected() && ui.get_active_output_label() != "";
    ui.set_can_assign(!app.listening_to_a && app.selected.is_some() && !app.edited && output_ready);
    let mapped = app.active_output.as_ref().and_then(|id| app.preferences.outputs.get(id));
    ui.set_output_mapping(mapped.and_then(|id| app.presets.iter().find(|preset| preset.id() == *id))
        .map(|preset| preset.config.name.clone()).unwrap_or_default().into());
    ui.set_can_unassign(mapped.is_some() && output_ready);
}

fn refresh_band_rows(ui: &MainWindow, cfg: &Config) {
    let rows = ui.get_bands();
    if rows.row_count() == cfg.bands.len() {
        for index in 0..cfg.bands.len() {
            let row = band_row(cfg, index);
            if rows.row_data(index).as_ref() != Some(&row) {
                rows.set_row_data(index, row);
            }
        }
    } else {
        ui.set_bands(Rc::new(VecModel::from((0..cfg.bands.len()).map(|index| band_row(cfg, index)).collect::<Vec<_>>())).into());
    }
}

fn refresh_preamp(ui: &MainWindow, app: &App, cfg: &Config) {
    let applied = app.effective.preamp_db;
    let automatic = app.auto_headroom();
    let manual = if app.simple.active { app.simple.manual_preamp() } else { cfg.preamp_db };
    let displayed = if automatic { applied } else { manual };
    ui.set_preamp(displayed as f32);
    ui.set_preamp_entry(format!("{displayed:+.2}").into());
    ui.set_preamp_label(if app.effective.bypass {
        "Bypassed".into()
    } else if automatic {
        format!("Auto {applied:+.2} dB")
    } else if (manual - applied).abs() >= 0.025 {
        format!("Applied {applied:+.2} dB")
    } else { "".into() }.into());
}

fn set_manual_preamp(ui: &MainWindow, app: &mut App, value: f64, group: Option<EditGroup>) {
    if app.auto_headroom() || (!app.simple.active && app.listening_to_a) || !value.is_finite() {
        refresh_preamp(ui, app, &app.cfg);
        return;
    }
    let value = (value.clamp(-config::MANUAL_PREAMP_LIMIT_DB, config::MANUAL_PREAMP_LIMIT_DB) * 20.0).round() / 20.0;
    if app.simple.active {
        if app.simple.preamp_db != value {
            app.remember_current(group);
            app.simple.preamp_db = value;
            app.apply_audio();
            ui.set_can_undo(true);
        }
    } else {
        let mut config = app.cfg.clone();
        config.preamp_db = value;
        commit_edit(ui, app, config, group);
    }
    refresh_preamp(ui, app, &app.cfg);
}

fn apply_and_update_controls(ui: &MainWindow, app: &mut App) {
    app.apply_audio();
    update_controls(ui, app);
    draw_eq_graph(ui, app);
}

fn refresh_preset_names(ui: &MainWindow, app: &App) {
    ui.set_presets(text_model(app.presets.iter().map(|p| p.config.name.clone())));
}

fn commit(ui: &MainWindow, app: &mut App, candidate: Config) {
    commit_edit(ui, app, candidate, None);
}

fn commit_edit(ui: &MainWindow, app: &mut App, mut candidate: Config, group: Option<EditGroup>) -> bool {
    if candidate.spline == app.cfg.spline && candidate.bands != app.cfg.bands {
        candidate.fit_target = None;
    }
    match candidate.validate() {
        Ok(()) => {
            if app.cfg != candidate {
                if app.cfg.spline != candidate.spline {
                    ui.invoke_clear_point_selection();
                }
                app.remember_current(group);
                app.cfg = candidate;
                app.edited = true;
                apply_and_update_controls(ui, app);
            }
            ui.set_notice("".into());
            true
        }
        Err(e) => {
            ui.set_notice(e.into());
            update_controls(ui, app);
            false
        }
    }
}

fn poll_fit(ui: &MainWindow, app: &mut App) {
    let Some(job) = app.pending_fit.take() else { return; };
    match job.result.try_recv() {
        Ok((candidate, error)) => {
            ui.set_fitting_eq(false);
            if job.superseded || app.cfg != job.original || app.fit_rate() != job.rate || app.simple.active || app.listening_to_a {
                ui.set_notice("EQ changed during fitting. Switch modes again to fit the current curve.".into());
                return;
            }
            commit(ui, app, candidate);
            ui.set_fit_detail(format!("Fit Error {error:.2} dB").into());
            if error > fit::TOLERANCE_DB {
                ui.set_notice(format!("The fitted curve differs by up to {error:.2} dB. Use Undo to restore the previous EQ.").into());
            }
        }
        Err(mpsc::TryRecvError::Empty) => app.pending_fit = Some(job),
        Err(mpsc::TryRecvError::Disconnected) => {
            ui.set_fitting_eq(false);
            ui.set_notice("Could not fit the EQ curve.".into());
        }
    }
}

fn edit_band(ui: &MainWindow, app: &mut App, index: i32, group: Option<EditGroup>, edit: impl FnOnce(&mut config::Band)) {
    if app.listening_to_a { return; }
    let index = index as usize;
    let Some(original) = app.cfg.bands.get(index) else { return; };
    let mut band = original.clone();
    edit(&mut band);
    if &band == original { return; }
    if app.cfg.spline && band.frequency_hz != original.frequency_hz
        && (config::MIN_FREQUENCY..=config::MAX_FREQUENCY).contains(&band.frequency_hz)
        && !app.cfg.can_place_spline_point(band.frequency_hz, Some(index))
    {
        refresh_band_rows(ui, &app.cfg);
        ui.set_notice("Leave more space between spline points.".into());
        return;
    }
    ui.invoke_cancel_graph_drag();
    let mut config = app.cfg.clone();
    let new_index = config.replace_band(index, band);
    if commit_edit(ui, app, config, group) {
        ui.set_selected_point(new_index as i32);
    }
}

fn fit_initial_window(window: &slint::Window) {
    use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};

    let mut fitted = false;
    window.on_winit_window_event(move |window, event| {
        if !fitted && matches!(event, WindowEvent::RedrawRequested) {
            let fit = window.with_winit_window(|native| {
                let monitor = native.current_monitor().or_else(|| native.primary_monitor())?;
                let area = monitor.size();
                let scale = native.scale_factor();
                let size = native.inner_size();
                let width = size.width.min(area.width.saturating_sub((24.0 * scale).round() as u32));
                let height = size.height.min(area.height.saturating_sub((64.0 * scale).round() as u32));
                Some((!native.is_maximized() && !window.is_fullscreen() && width > 0 && height > 0
                    && (width < size.width || height < size.height))
                    .then(|| slint::PhysicalSize::new(width, height)))
            }).flatten();
            if let Some(size) = fit {
                fitted = true;
                if let Some(size) = size {
                    window.set_size(size);
                }
            }
        }
        EventResult::Propagate
    });
}

fn data_directory() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| ".".into())).join("SimpleDarkEQ")
    } else {
        config_home().join("simpledarkeq")
    }
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".config"))
}

fn read_config(path: &Path) -> Result<Config, String> {
    if !path.extension().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("json")) {
        return read_apo_config(path);
    }
    let text = storage::read_text(path).map_err(|e| e.to_string())?;
    Config::from_json(&text)
}

fn read_apo_config(path: &Path) -> Result<Config, String> {
    let text = storage::read_text(path).map_err(|e| e.to_string())?;
    let mut cfg = Config::from_apo(&text)?;
    if let Some(stem) = path.file_stem() {
        let stem = stem.to_string_lossy();
        let name = stem.trim();
        if !name.is_empty() {
            cfg.name = name[..name.floor_char_boundary(config::MAX_NAME_BYTES)].into();
        }
    }
    cfg.validate()?;
    Ok(cfg)
}

fn configure_color_picker(ui: &MainWindow) {
    let appearance = ui.global::<Appearance>();
    appearance.on_format_color(|c| theme::ThemeColors::hex([c.red(), c.green(), c.blue()]).into());
    appearance.on_parse_color(|text, fallback| {
        theme::ThemeColors::parse(&text).map_or(fallback, |colors| {
            let [r, g, b] = colors.accent;
            slint::Color::from_rgb_u8(r, g, b)
        })
    });
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Resolve desktop autostart before isolating the engine's config.
    #[cfg(target_os = "linux")]
    let login_entry = config_home().join("autostart/simple-modern-equalizer.desktop");
    let directory = data_directory();
    fs::create_dir_all(directory.join("presets"))?;
    let Some(_instance) = storage::lock_instance(&directory)? else {
        #[cfg(target_os = "linux")]
        if std::env::args_os().skip(1).any(|arg| arg == "--autostart") {
            return Ok(());
        }
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
    let flat = Config::flat_spline();
    let mut presets = vec![Preset::builtin(flat.clone())];
    let examples: Vec<Config> = serde_json::from_str(include_str!("../presets/catalog.json"))?;
    for mut cfg in examples {
        cfg.validate().map_err(std::io::Error::other)?;
        cfg.sort_bands();
        presets.push(Preset::builtin(cfg));
    }
    let mut cfg = read_config(&directory.join("current.json")).unwrap_or_else(|_| flat.clone());
    if cfg.bands == Config::default().bands && cfg.name == flat.name && !cfg.spline {
        cfg.bands = flat.bands;
        cfg.spline = true;
    }
    Preset::restore_builtin_name(&presets, &mut cfg);
    presets.extend(Preset::load_custom(&directory.join("presets"))?);
    let selected = presets.iter().position(|preset| {
        let mut saved = preset.config.clone();
        saved.bypass = cfg.bypass;
        saved.show_live_view = cfg.show_live_view;
        // Edit history affects removal warnings, not the preset's sound or selection.
        for (saved_band, current_band) in saved.bands.iter_mut().zip(&cfg.bands) {
            saved_band.edited = current_band.edited;
        }
        saved == cfg
    });
    let edited = selected.is_none() && presets.iter().any(|preset| preset.config.name == cfg.name);
    let preferences: Preferences = storage::read_text(&directory.join("preferences.json")).ok()
        .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()).unwrap_or_default();
    let simple: simple::SimpleSettings = storage::read_text(&directory.join("simple.json")).ok()
        .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()).unwrap_or_default();
    let initial = if simple.active { simple.config(cfg.bypass, preferences.simple_auto_headroom, cfg.balance) } else {
        listening::prepare(&cfg, None, preferences.auto_headroom, 48000.0, 2).0
    };
    let app = Rc::new(RefCell::new(App { engine: engine::Engine::start(&directory), cfg, simple, presets,
        selected, preferences, active_output: None, fallback: None, reference: None, listening_to_a: false,
        effective: initial,
        outputs: vec![], directory, edited, plot: graph::GraphCache::default(), drag_origin: None,
        pending_band_removal: None, plot_spectrum: vec![], format: None,
        undo: VecDeque::new(), edit_group: None, pending_fit: None }));
    let ui = MainWindow::new()?;
    #[cfg(target_os = "linux")]
    {
        ui.set_has_login_start(true);
        ui.set_login_start(autostart::enabled(&login_entry));
        ui.on_change_login_start(move |enabled| {
            match autostart::set_enabled(&login_entry, enabled) {
                Ok(()) => "".into(),
                Err(error) => format!("Could not update login startup. {error}").into(),
            }
        });
    }
    configure_color_picker(&ui);
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_source_license(env!("CARGO_PKG_LICENSE").into());
    ui.set_license_text(include_str!("../LICENSE").into());
    apply_colors(&ui, app.borrow().preferences.colors);
    let _widget_accent = keep_widget_accent(&ui);
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
    update_controls(&ui, &app.borrow());
    draw_eq_graph(&ui, &mut app.borrow_mut());
    {
        let state = app.borrow();
        ui.set_live_view_height(state.preferences.live_view_height());
        ui.set_eq_plot_height(state.preferences.eq_plot_height());
        let display = &state.preferences.display;
        ui.set_live_mode(display.live_mode_index());
        ui.set_show_band_list(display.band_list);
        ui.set_show_levels(display.levels);
        ui.set_show_clipping(display.clipping);
        ui.set_show_preset_button(display.preset_button);
        ui.set_show_eq_spectrum(display.eq_spectrum);
        ui.set_simple_layout(state.simple.active);
        ui.set_simple_bass(state.simple.bass.clamp(-12.0, 12.0));
        ui.set_simple_mid(state.simple.mid.clamp(-12.0, 12.0));
        ui.set_simple_treble(state.simple.treble.clamp(-12.0, 12.0));
        ui.set_simple_loudness(state.simple.loudness);
    }
    refresh_preset_names(&ui, &app.borrow());
    ui.set_outputs(text_model(["Follow system default".into()]));

    macro_rules! on_ui_action {
        ($callback:ident, |$window:ident, $state:ident, $($arg:ident),*| $body:block) => {{
            let weak = ui.as_weak(); let shared = app.clone();
            ui.$callback(move |$($arg),*| { if let Some($window) = weak.upgrade() {
                #[allow(unused_mut)]
                let mut $state = shared.borrow_mut(); $body
            }});
        }};
    }
    on_ui_action!(on_change_layout, |w, a, simple| {
        if a.simple.active != simple {
            a.remember_current(None);
            w.invoke_cancel_graph_drag();
        }
        a.simple.active = simple;
        w.set_simple_layout(simple);
        apply_and_update_controls(&w, &mut a);
    });
    {
        let weak = ui.as_weak();
        ui.on_change_full_screen(move |enabled| {
            if let Some(w) = weak.upgrade() {
                w.window().set_fullscreen(enabled);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let app = app.clone();
        ui.on_change_colors(move |accent| {
            let Some(ui) = weak.upgrade() else { return "The window is closed.".into() };
            let colors = match theme::ThemeColors::parse(&accent) {
                Ok(colors) => colors,
                Err(error) => return error.into(),
            };
            let mut app = app.borrow_mut();
            let mut preferences = app.preferences.clone();
            preferences.colors = colors;
            if let Err(error) = preferences.save(&app.directory) {
                return error.into();
            }
            app.preferences = preferences;
            apply_colors(&ui, colors);
            draw_eq_graph(&ui, &mut app);
            "".into()
        });
    }
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
    on_ui_action!(on_change_tone, |w, a, bass, mid, treble, loudness| {
        if (a.simple.bass, a.simple.mid, a.simple.treble, a.simple.loudness) == (bass, mid, treble, loudness) {
            return;
        }
        let group = (a.simple.loudness == loudness).then_some(EditGroup::Tone);
        a.remember_current(group);
        a.simple.bass = bass;
        a.simple.mid = mid;
        a.simple.treble = treble;
        a.simple.loudness = loudness;
        if w.get_simple_layout() {
            a.apply_audio();
            w.set_can_undo(true);
            refresh_preamp(&w, &a, &a.cfg);
        }
    });
    on_ui_action!(on_change_preamp, |w, a, value| {
        set_manual_preamp(&w, &mut a, f64::from(value), Some(EditGroup::Preamp));
    });
    on_ui_action!(on_enter_preamp, |w, a, value| {
        let value = value.trim().replace(',', ".").parse::<f64>().unwrap_or(f64::NAN);
        set_manual_preamp(&w, &mut a, value, None);
    });
    on_ui_action!(on_change_balance, |w, a, value| {
        let mut c = a.cfg.clone();
        c.balance = f64::from(value);
        commit_edit(&w, &mut a, c, Some(EditGroup::Balance));
    });
    on_ui_action!(on_change_bypass, |w, a, value| {
        if a.cfg.bypass != value {
            a.remember_current(None);
            a.cfg.bypass = value;
            apply_and_update_controls(&w, &mut a);
        }
    });
    on_ui_action!(on_finish_edit, |_w, a, | { a.edit_group = None; });
    on_ui_action!(on_undo, |w, a, | {
        match a.undo_edit() {
            Ok(true) => {
                w.invoke_clear_point_selection();
                w.set_simple_layout(a.simple.active);
                w.set_simple_bass(a.simple.bass);
                w.set_simple_mid(a.simple.mid);
                w.set_simple_treble(a.simple.treble);
                w.set_simple_loudness(a.simple.loudness);
                w.set_notice("".into());
                w.set_fit_detail("".into());
                apply_and_update_controls(&w, &mut a);
            }
            Ok(false) => {}
            Err(error) => w.set_notice(error.into()),
        }
    });
    on_ui_action!(on_change_live_view, |_w, a, value| {
        a.cfg.show_live_view = value;
    });
    on_ui_action!(on_toggle_eq_mode, |w, a, | {
        if a.listening_to_a || a.simple.active || a.pending_fit.is_some() { return; }
        w.invoke_cancel_graph_drag();
        let rate = a.fit_rate();
        let original = a.cfg.clone();
        let source = original.clone();
        let (send, result) = mpsc::channel();
        std::thread::spawn(move || { let _ = send.send(fit::convert(source, rate)); });
        a.pending_fit = Some(PendingFit { original, rate, result, superseded: false });
        w.set_fitting_eq(true);
        w.set_notice("".into());
    });
    on_ui_action!(on_change_count, |w, a, count| {
        if a.listening_to_a || a.simple.active { return; }
        w.invoke_clear_point_selection();
        a.pending_band_removal = None;
        let count = (count.max(if a.cfg.spline { 2 } else { 0 }) as usize).min(config::MAX_BANDS);
        let protected = a.cfg.protected_band_count(count);
        let mut candidate = a.cfg.clone();
        candidate.resize(count);
        if protected == 0 {
            let placed = candidate.bands.len();
            commit(&w, &mut a, candidate);
            if placed < count {
                w.set_band_count(placed as i32);
                w.set_notice("No room for another spline point. Move or remove a point to make space.".into());
            }
        } else {
            let current_count = a.cfg.bands.len();
            w.set_band_count(current_count as i32);
            a.pending_band_removal = Some((a.cfg.clone(), candidate));
            w.invoke_warn_band_removal(count as i32, current_count as i32, protected as i32);
        }
    });
    on_ui_action!(on_delete_band, |w, a, index| {
        if a.listening_to_a || a.simple.active { return; }
        let index = index as usize;
        let Some(band) = a.cfg.bands.get(index) else { return; };
        if a.cfg.locked_point(index) { return; }
        w.invoke_clear_point_selection();
        let mut candidate = a.cfg.clone();
        candidate.bands.remove(index);
        if band.needs_removal_confirmation() {
            a.pending_band_removal = Some((a.cfg.clone(), candidate));
            w.invoke_warn_single_band_removal((index + 1) as i32);
        } else {
            a.pending_band_removal = None;
            commit(&w, &mut a, candidate);
        }
    });
    on_ui_action!(on_resolve_band_removal, |w, a, confirmed| {
        if let Some((original, candidate)) = a.pending_band_removal.take()
            && confirmed && !a.listening_to_a && !a.simple.active && a.cfg == original
        {
            commit(&w, &mut a, candidate);
        }
    });
    on_ui_action!(on_edit_band_number, |w, a, index, is_frequency, text| {
        if a.cfg.spline && (!is_frequency || a.cfg.locked_point(index as usize)) { return; }
        let text = text.trim();
        if let Some(row) = w.get_bands().row_data(index as usize) {
            let displayed = if is_frequency { row.frequency } else { row.q };
            if displayed == text {
                w.set_notice("".into());
                return;
            }
        }
        match text.parse::<f64>() {
            Ok(value) => edit_band(&w, &mut a, index, None, |band| {
                if is_frequency { band.frequency_hz = value; } else { band.q = value; }
            }),
            Err(_) => w.set_notice(if is_frequency { "Enter a frequency in Hz." } else { "Enter a numeric Q value." }.into()),
        }
    });
    on_ui_action!(on_edit_band_gain, |w, a, index, gain| {
        edit_band(&w, &mut a, index, Some(EditGroup::BandGain(index)), |band| band.gain_db = (f64::from(gain) * 10.0).round() / 10.0);
    });
    on_ui_action!(on_edit_band_kind, |w, a, index, kind| {
        if a.cfg.spline { return; }
        edit_band(&w, &mut a, index, None, |band| band.kind = match kind { 1 => "LS", 2 => "HS", _ => "PK" }.into());
    });
    on_ui_action!(on_edit_band_enabled, |w, a, index, enabled| {
        if a.cfg.spline { return; }
        edit_band(&w, &mut a, index, None, |band| band.enabled = enabled);
    });
    on_ui_action!(on_add_band, |w, a, x, y, width, height| {
        if a.listening_to_a || a.simple.active
            || graph::hit_band(&a.plot.dots, x.into(), y.into(), width.into(), height.into()).is_some()
        {
            return;
        }
        let (frequency, gain) = graph::band_values(x.into(), y.into());
        let mut config = a.cfg.clone();
        let index = config.bands.partition_point(|band| band.frequency_hz <= frequency);
        if config.add_peak(frequency, gain) {
            w.invoke_cancel_graph_drag();
            commit(&w, &mut a, config);
            w.set_selected_point(index as i32);
        } else if config.bands.len() >= config::MAX_BANDS {
            w.set_notice("The equalizer already has 32 bands.".into());
        } else if config.spline {
            w.set_notice("Leave more space between spline points.".into());
        }
    });
    {
        let app = app.clone();
        ui.on_hit_band(move |x, y, width, height| {
            let mut a = app.borrow_mut();
            a.edit_group = None;
            let hit = graph::hit_band(&a.plot.dots, x.into(), y.into(), width.into(), height.into());
            a.drag_origin = hit.and_then(|index| a.cfg.bands.get(index).map(|band| {
                graph::BandDrag::new(index, band).with_gain_span(a.plot.axis.span())
            }));
            a.drag_origin.map_or(-1, |drag| drag.index as i32)
        });
    }
    {
        let weak = ui.as_weak();
        let app = app.clone();
        ui.on_move_band(move |index, horizontal_delta, vertical_delta| {
            let Some(w) = weak.upgrade() else { return -1; };
            let mut a = app.borrow_mut();
            let Some(mut drag) = a.drag_origin else { return -1; };
            if a.listening_to_a || drag.index != index as usize { return -1; }
            let previous = a.undo_state();
            if !drag.update(&mut a.cfg, horizontal_delta.into(), vertical_delta.into()) {
                return index;
            }
            a.remember(previous, Some(EditGroup::GraphDrag));
            a.cfg.fit_target = None;
            w.set_fit_detail("".into());
            w.set_can_undo(true);
            a.drag_origin = Some(drag);
            if !a.edited {
                a.edited = true;
                w.set_graph_edited(true);
                w.set_can_assign(false);
            }
            refresh_band_rows(&w, &a.cfg);
            a.apply_audio();
            refresh_preamp(&w, &a, &a.cfg);
            draw_eq_graph(&w, &mut a);
            w.set_notice("".into());
            drag.index as i32
        });
    }
    on_ui_action!(on_choose_preset, |w, a, index| {
        if a.presets.get(index as usize).is_some() {
            w.invoke_clear_point_selection();
            a.remember_current(None);
            a.select_preset(index as usize);
            a.simple.active = false;
            w.set_simple_layout(false);
            w.set_notice("".into());
            apply_and_update_controls(&w, &mut a);
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
                        apply_and_update_controls(&w, &mut a);
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
                        update_controls(&w, &a);
                    }
                    "".into()
                }
                Err(error) => error.into(),
            }
        });
    }
    on_ui_action!(on_assign_preset, |w, a, assign| {
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
                    update_controls(&w, &a);
                }
                Err(error) => w.set_notice(error.into()),
            }
        }
    });
    on_ui_action!(on_change_headroom, |w, a, enabled| {
        let previous = a.undo_state();
        let changed = a.auto_headroom() != enabled;
        if let Err(error) = a.set_auto_headroom(enabled) {
            w.set_notice(error.into());
        } else if changed {
            a.remember(previous, None);
        }
        apply_and_update_controls(&w, &mut a);
    });
    on_ui_action!(on_capture_a, |w, a, | {
        a.reference = Some(a.cfg.clone());
        a.listening_to_a = false;
        apply_and_update_controls(&w, &mut a);
    });
    on_ui_action!(on_listen_a, |w, a, listen| {
        w.invoke_clear_point_selection();
        a.listening_to_a = listen && a.reference.is_some();
        apply_and_update_controls(&w, &mut a);
    });
    on_ui_action!(on_clear_reference, |w, a, | {
        a.reference = None;
        a.listening_to_a = false;
        apply_and_update_controls(&w, &mut a);
    });
    on_ui_action!(on_choose_output, |w, a, index| {
        let target = if index <= 0 { None } else { a.outputs.get(index as usize - 1).cloned() };
        a.engine.shared.lock().unwrap().target = Some(target);
        w.set_status("Switching output…".into());
    });
    on_ui_action!(on_import_apo, |w, a, | {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Import Equalizer APO")
            .add_filter("Equalizer APO", &["txt"])
            .pick_file()
        {
            let previous = a.undo_state();
            match a.import_apo(&path) {
                Ok(()) => {
                    a.remember(previous, None);
                    w.invoke_clear_point_selection();
                    w.set_simple_layout(false);
                    w.set_notice("".into());
                    refresh_preset_names(&w, &a);
                    apply_and_update_controls(&w, &mut a);
                }
                Err(error) => {
                    rfd::MessageDialog::new().set_title("Import Equalizer APO")
                        .set_level(rfd::MessageLevel::Error).set_description(error).show();
                }
            }
        }
    });
    on_ui_action!(on_save_config, |w, a, | {
        if let Some(path) = rfd::FileDialog::new().add_filter("Simple Modern Equalizer JSON", &["json"]).set_file_name("my-eq.json").save_file() {
            let result = serde_json::to_vec_pretty(&a.cfg).map_err(|e| e.to_string())
                .and_then(|text| storage::write_text(&path, &text).map_err(|e| e.to_string()));
            match result {
                Ok(()) => w.set_notice("Exported JSON.".into()),
                Err(e) => { w.set_notice(e.into()); }
            }
        }
    });
    on_ui_action!(on_export_apo, |w, a, | {
        if let Some(path) = rfd::FileDialog::new().add_filter("Equalizer APO", &["txt"]).set_file_name("simpledarkeq.txt").save_file() {
            match a.cfg.to_apo().and_then(|text| storage::write_text(&path, text.as_bytes()).map_err(|e| e.to_string())) {
                Ok(()) => w.set_notice("Exported APO. JSON retains the full editable config, including bypass.".into()),
                Err(e) => w.set_notice(e.into()),
            }
        }
    });

    let weak = ui.as_weak(); let timer_state = app.clone();
    let live_meters = RefCell::new(live::LiveMeters::default());
    let monitor_plots = RefCell::new(monitor::Plots::default());
    let mut loudness = monitor::History::default();
    let engine_state = app.borrow().engine.shared.clone();
    let mut stereo_levels = [-60.0; 2];
    let mut last_frame = Instant::now();
    let mut next_control = last_frame;
    let mut next_display_probe = last_frame;
    let mut display_interval = live::display_interval(None);
    let mut clip_until = Instant::now();
    let timer = Rc::new(slint::Timer::default());
    let timer_ref = Rc::downgrade(&timer);
    timer.start(slint::TimerMode::Repeated, display_interval, move || {
        let Some(w) = weak.upgrade() else { return; };
        let mut a = timer_state.borrow_mut();
        let now = Instant::now();
        let elapsed = now.duration_since(last_frame).min(Duration::from_millis(250));
        last_frame = now;
        let update_controls_now = now >= next_control;
        if update_controls_now {
            next_control = (next_control + live::CONTROL_INTERVAL).max(now + Duration::from_millis(1));
            poll_fit(&w, &mut a);
        }
        let mode = w.get_live_mode();
        let visible = !w.window().is_minimized();
        let show_live_view = visible && w.get_live_monitor_expanded();
        let show_eq_graph = visible && !a.simple.active;
        let eq_spectrum = show_eq_graph && w.get_show_eq_spectrum();
        let monitor_clipping = visible && w.get_show_clipping();
        let (control, stereo_rms, waveform, monitor_signal) = {
            let mut shared = engine_state.lock().unwrap();
            shared.monitor_mode = (show_live_view && matches!(mode, live::BEFORE_AFTER_MODE | live::STEREO_FIELD_MODE))
                .then_some(mode);
            shared.meter_capture_requested = cfg!(windows) && show_live_view && mode == live::STEREO_LEVEL_MODE;
            shared.scope_requested = show_live_view && mode == scope::MODE;
            shared.spectrum_requested = if show_live_view && mode == live::BARS_MODE {
                Some(spectrum::Resolution::Detailed)
            } else if eq_spectrum {
                Some(spectrum::Resolution::Standard)
            } else {
                None
            };
            let spectrum = shared.spectrum_levels.as_deref().map(spectrum::Levels::as_slice)
                .or_else(|| shared.state.as_ref().map(|state| state.spectrum.as_slice())).unwrap_or_default();
            if shared.spectrum_requested.is_some() {
                live::smooth_spectrum(&mut a.plot_spectrum, spectrum, elapsed);
            } else {
                a.plot_spectrum.clear();
            }
            let stereo_rms = shared.playback_levels.map(|(rms, _)| rms)
                .or_else(|| shared.state.as_ref().map(|state| state.meters.stereo_rms)).unwrap_or_default();
            let control = if update_controls_now {
                let clip_spectrum = if shared.clipped && monitor_clipping { spectrum.to_vec() } else { Vec::new() };
                let snapshot = shared.state.as_ref().map(|state| {
                    let outputs = (a.outputs != state.available_sinks).then(|| {
                        let names = state.available_sinks.iter().map(|name| state.sink_label(name)).collect::<Vec<_>>();
                        (state.available_sinks.clone(), names)
                    });
                    let label = state.active_output.as_ref().map(|id| state.sink_label(id));
                    let mut meters = state.meters;
                    if let Some((_, peak)) = shared.playback_levels {
                        meters.out_peak = peak;
                    }
                    (meters, engine::AudioFormat::from(state), outputs, state.preferred_output.clone(), label)
                });
                Some((snapshot, shared.status.clone(), std::mem::take(&mut shared.clipped), clip_spectrum))
            } else { None };
            let waveform = if shared.scope_requested { shared.scope_samples.clone() } else { None };
            let signal = shared.monitor_signal.as_ref()
                .filter(|signal| shared.monitor_mode.is_some_and(|mode| signal.has_mode(mode)))
                .cloned();
            (control, stereo_rms, waveform, signal)
        };
        if let Some((snapshot, status, clipped, clip_spectrum)) = control {
            let connection_changed = w.get_connected() != snapshot.is_some();
            w.set_status(status.into()); w.set_connected(snapshot.is_some());
            if let Some((meters, format, ..)) = &snapshot {
                if a.format.as_ref() != Some(format) {
                    loudness.clear();
                }
                loudness.update(now, Some(meters.out_rms));
            } else {
                loudness.update(now, None);
            }
            if let Some((meters, format, outputs, preferred, label)) = snapshot {
                w.set_output_peak_dbfs(live::amplitude_dbfs(meters.out_peak));
                let signal = if meters.out_peak > 0.00001 { "Audio detected" } else { "No signal detected yet" };
                w.set_signal(signal.into());
                if clipped {
                    clip_until = Instant::now() + Duration::from_secs(2);
                    if monitor_clipping {
                        let regions = spectrum::clip_regions(&clip_spectrum);
                        if a.simple.active {
                            let [bass, mid, treble] = simple::clipping_controls(&regions);
                            w.set_simple_clipping(ToneClipping { bass, mid, treble });
                        } else {
                            let regions = regions.into_iter().map(|(start, end)| FrequencyRegion { start, end }).collect::<Vec<_>>();
                            w.set_clip_regions(Rc::new(VecModel::from(regions)).into());
                        }
                    }
                }
                if let Some((ids, names)) = outputs {
                    a.outputs = ids;
                    w.set_outputs(text_model(std::iter::once("Follow system default".into()).chain(names)));
                }
                let index = preferred.as_ref().filter(|id| !cfg!(windows) || Some(*id) == format.output.as_ref())
                    .and_then(|id| a.outputs.iter().position(|x| x == id));
                w.set_output_index(index.map_or(0, |i| i as i32 + 1));
                let update = connection_changed || a.format.as_ref() != Some(&format);
                if a.active_output != format.output {
                    w.invoke_dismiss_presets();
                    w.invoke_clear_point_selection();
                    a.change_output(format.output.clone());
                    clip_until = Instant::now();
                    w.set_notice("".into());
                }
                a.format = Some(format);
                w.set_active_output_label(label.unwrap_or_default().into());
                if update {
                    a.apply_audio();
                    update_controls(&w, &a);
                }
                if w.get_active_output_label() == "" {
                    w.set_can_assign(false);
                    w.set_can_unassign(false);
                }
            } else {
                clip_until = Instant::now();
                w.set_output_peak_dbfs(live::SILENT_PEAK_DBFS);
                w.set_signal("Waiting for audio engine".into());
                w.set_active_output_label("".into());
                w.set_can_assign(false);
                w.set_can_unassign(false);
            }
            let clipping = w.get_connected() && Instant::now() < clip_until;
            w.set_clipping(clipping);
            if (!clipping || !monitor_clipping || !a.simple.active) && w.get_simple_clipping() != ToneClipping::default() {
                w.set_simple_clipping(ToneClipping::default());
            }
            if !clipping && w.get_clip_regions().row_count() != 0 {
                w.set_clip_regions(ModelRc::default());
            }
        }
        if show_live_view && mode == live::STEREO_LEVEL_MODE {
            let [left, right] = live::smooth_stereo(&mut stereo_levels, stereo_rms, elapsed);
            w.set_left_level_dbfs(left);
            w.set_right_level_dbfs(right);
        } else if show_live_view && matches!(mode, live::BEFORE_AFTER_MODE | live::LOUDNESS_HISTORY_MODE | live::STEREO_FIELD_MODE) {
            let status = match (mode, monitor_signal.as_deref()) {
                (live::LOUDNESS_HISTORY_MODE, _) => w.get_signal(),
                (_, Some(monitor::Signal::Stereo { correlation: Some(value), .. })) => format!("Correlation {value:+.2}").into(),
                (_, Some(monitor::Signal::Stereo { .. })) => "Correlation —".into(),
                (_, Some(monitor::Signal::Comparison { .. })) => "".into(),
                (live::BEFORE_AFTER_MODE, None) if !cfg!(target_os = "linux") => "Before / After EQ requires PipeWire".into(),
                (_, None) => w.get_signal(),
            };
            w.set_monitor_status(status);
            let extent = w.get_live_plot_extent();
            if let Some(size) = RasterSize::from_logical(extent.width, extent.height, w.window().scale_factor())
                && let Some(frame) = monitor_plots.borrow_mut().frame(mode, size, a.preferences.colors, monitor_signal.as_ref(), &loudness, now, elapsed)
            {
                w.set_monitor_image(slint::Image::from_rgba8_premultiplied(frame));
            }
        } else if show_live_view {
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
            let mut meters = live_meters.borrow_mut();
            meters.set_colors(a.preferences.colors);
            if let Some(size) = RasterSize::from_logical(extent.width, extent.height, w.window().scale_factor())
                && let Some(frame) = meters.frame(mode, &a.plot_spectrum, samples, size)
            {
                w.set_live_image(slint::Image::from_rgba8_premultiplied(frame));
            }
        }
        if show_eq_graph && (eq_spectrum || update_controls_now) {
            draw_eq_graph(&w, &mut a);
        }
        if !show_live_view || mode != live::STEREO_LEVEL_MODE {
            stereo_levels = [-60.0; 2];
        }
        let animating = show_live_view || eq_spectrum;
        if animating && now >= next_display_probe {
            display_interval = live::display_interval(live::display_millihertz(w.window()));
            next_display_probe = now + Duration::from_millis(500);
        }
        let interval = if animating { display_interval } else { live::IDLE_INTERVAL };
        if let Some(timer) = timer_ref.upgrade() && timer.interval() != interval {
            timer.set_interval(interval);
        }
    });
    fit_initial_window(ui.window());
    ui.run()?;
    timer.stop();
    let mut a = app.borrow_mut();
    let mut current = a.fallback.as_ref().map_or_else(|| a.cfg.clone(), |(config, _, _)| config.clone());
    current.bypass = a.cfg.bypass;
    current.show_live_view = a.cfg.show_live_view;
    storage::write_text(&a.directory.join("current.json"), &serde_json::to_vec_pretty(&current)?)?;
    storage::write_text(&a.directory.join("simple.json"), &serde_json::to_vec_pretty(&a.simple)?)?;
    a.preferences.display = DisplayPreferences {
        live_mode: ui.get_live_mode() + 2,
        band_list: ui.get_show_band_list(),
        levels: ui.get_show_levels(),
        clipping: ui.get_show_clipping(),
        preset_button: ui.get_show_preset_button(),
        eq_spectrum: ui.get_show_eq_spectrum(),
    };
    a.save_graph_heights(ui.get_eq_plot_height(), ui.get_live_view_height())?;
    a.preferences.save(&a.directory)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_for_preset_tests(standard: Config) -> App {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("simpledarkeq-preset-test-{}-{nonce}", std::process::id()));
        fs::create_dir_all(directory.join("presets")).unwrap();
        App {
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
            pending_band_removal: None,
            plot_spectrum: vec![],
            format: None,
            undo: VecDeque::new(),
            edit_group: None,
            pending_fit: None,
        }
    }

    #[test]
    fn undo_keeps_fifteen_actions_and_coalesces_a_drag() {
        let mut app = app_for_preset_tests(Config::default());
        let start = app.cfg.clone();
        for gain in 1..=6 {
            app.remember_current(Some(EditGroup::GraphDrag));
            app.cfg.bands[0].gain_db = f64::from(gain);
        }
        assert_eq!(app.undo.len(), 1);
        assert!(app.undo_edit().unwrap());
        assert_eq!(app.cfg, start);

        for gain in 1..=20 {
            app.remember_current(None);
            app.cfg.bands[0].gain_db = f64::from(gain);
        }
        assert_eq!(app.undo.len(), 15);
        for gain in (5..20).rev() {
            assert!(app.undo_edit().unwrap());
            assert_eq!(app.cfg.bands[0].gain_db, f64::from(gain));
        }
        assert!(!app.undo_edit().unwrap());
        fs::remove_dir_all(&app.directory).unwrap();
    }

    #[test]
    fn undo_restores_the_fit_mode_and_reference() {
        let mut app = app_for_preset_tests(Config::default());
        app.cfg = fit::convert(app.cfg.clone(), 48000).0;
        let before = app.cfg.clone();
        app.remember_current(None);
        app.cfg = fit::convert(before.clone(), 48000).0;
        assert!(!app.cfg.spline);
        assert!(app.undo_edit().unwrap());
        assert_eq!(app.cfg, before);
        fs::remove_dir_all(&app.directory).unwrap();
    }

    #[test]
    fn simple_preamp_is_saved_independently_and_auto_headroom_replaces_it() {
        let mut app = app_for_preset_tests(Config { preamp_db: -9.0, ..Config::default() });
        app.simple = serde_json::from_str(r#"{"active":true,"bass":4}"#).unwrap();
        assert_eq!(app.simple.preamp_db, 0.0);
        app.preferences.simple_auto_headroom = false;
        app.simple.preamp_db = -7.0;
        app.apply_audio();
        assert_eq!(app.effective.preamp_db, -7.0);

        app.set_auto_headroom(true).unwrap();
        assert_eq!(app.simple.preamp_db, -4.0);
        app.simple.bass = 6.0;
        app.set_auto_headroom(false).unwrap();
        app.apply_audio();
        assert_eq!(app.effective.preamp_db, -6.0);
        let restored: simple::SimpleSettings =
            serde_json::from_str(&serde_json::to_string(&app.simple).unwrap()).unwrap();
        assert_eq!(restored.preamp_db, -6.0);

        app.simple.active = false;
        app.apply_audio();
        assert_eq!(app.effective.preamp_db, -9.0);
        fs::remove_dir_all(&app.directory).unwrap();
    }

    #[test]
    fn toggling_auto_headroom_replaces_the_manual_value() {
        let mut config = Config { preamp_db: -9.0, ..Config::default() };
        config.bands[2].gain_db = 4.0;
        let mut app = app_for_preset_tests(config);
        app.set_auto_headroom(true).unwrap();
        app.apply_audio();
        assert!((app.cfg.preamp_db + 4.2).abs() < 0.01);
        assert_eq!(app.cfg.preamp_db, app.effective.preamp_db);
        assert!(app.edited);

        app.cfg.bands[2].gain_db = 6.0;
        app.apply_audio();
        let latest = app.effective.preamp_db;
        app.set_auto_headroom(false).unwrap();
        app.apply_audio();
        assert!((latest + 6.2).abs() < 0.01);
        assert_eq!(app.cfg.preamp_db, latest);
        assert_eq!(app.effective.preamp_db, latest);
        fs::remove_dir_all(&app.directory).unwrap();
    }

    #[test]
    fn apo_import_persists_and_selects_a_custom_preset() {
        let mut app = app_for_preset_tests(Config { bypass: true, show_live_view: false, ..Config::default() });
        app.simple.active = true;
        let source = app.directory.join("Headphones.txt");
        fs::write(&source, "Preamp: -4 dB\nFilter: ON PK Fc 1000 Hz Gain -6 dB Q 2\n").unwrap();
        app.import_apo(&source).unwrap();
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.cfg.name, "Headphones");
        assert_eq!(app.cfg.preamp_db, -4.0);
        assert_eq!(app.cfg.bands[0].gain_db, -6.0);
        assert!(app.cfg.bypass);
        assert!(!app.cfg.show_live_view);
        assert!(!app.simple.active);

        app.import_apo(&source).unwrap();
        assert_eq!(app.cfg.name, "Headphones (2)");
        assert_eq!(Preset::load_custom(&app.directory.join("presets")).unwrap().len(), 2);
        fs::write(&source, "Include: other-file.txt\n").unwrap();
        assert!(app.import_apo(&source).is_err());
        assert_eq!(app.presets.len(), 3);
        assert_eq!(app.selected, Some(2));
        fs::remove_dir_all(&app.directory).unwrap();
    }

    #[test]
    fn simple_tone_saves_preserve_standard_until_explicit_overwrite() {
        let mut standard = Config { preamp_db: -8.0, balance: 0.3, ..Config::default() };
        standard.bands[0].gain_db = -4.0;
        standard.bands[1].enabled = false;
        let mut app = app_for_preset_tests(standard.clone());
        let preset_directory = app.directory.join("presets");
        app.simple.active = true;
        app.simple.bass = 6.0;
        app.apply_audio();
        assert_eq!(app.effective, app.simple.config(false, true, standard.balance));
        assert_eq!(app.effective.gains(), (0.7, 1.0));
        app.save_preset("Simple bass boost").unwrap();
        let saved = Preset::load_custom(&preset_directory).unwrap();
        assert_eq!(saved[0].config.bands, app.effective.bands);
        assert_eq!(saved[0].config.balance, standard.balance);
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
