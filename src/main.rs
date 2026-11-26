#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#[cfg(target_os = "linux")]
mod autostart;
mod config;
mod engine;
mod fit;
mod graph;
mod listening;
mod live;
mod matched;
mod monitor;
mod now_playing;
mod phase;
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
use presets::{Crossfeed, DisplayPreferences, Preferences, Preset};
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

enum PresetLoad {
    Saved(usize),
    Apo(PathBuf),
}

// #######################################################
// keep track of tuning, presets, and playback state
// #######################################################

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
    pending_preset_load: Option<PresetLoad>,
    pending_preset_deletion: Option<String>,
    plot_spectrum: Vec<f32>,
    format: Option<engine::AudioFormat>,
    undo: VecDeque<UndoState>,
    edit_group: Option<EditGroup>,
    pending_fit: Option<PendingFit>,
    fit_cache: fit::Cache,
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
        self.pending_preset_load = None;
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
            let automatic = listening::prepare(&active, None, true, rate, channels, self.preferences.phase_mode).0.preamp_db;
            let manual = automatic.max(-config::MANUAL_PREAMP_LIMIT_DB);
            if manual != self.cfg.preamp_db {
                self.cfg.preamp_db = manual;
                self.edited = true;
            }
        }
        self.preferences = preferences;
        Ok(())
    }

    fn save_graph_view(&mut self, ui: &MainWindow) -> Result<(), String> {
        let eq = ui.get_eq_plot_height().round().max(130.0) as i32;
        let live = ui.get_live_view_height().round().clamp(146.0, 420.0) as i32;
        let range = ui.get_eq_range_db().clamp(1, 24);
        let scale = graph::FrequencyScale::from_index(ui.get_eq_frequency_scale_index());
        if eq != self.preferences.eq_plot_height || live != self.preferences.live_view_height() as i32
            || range != self.preferences.eq_range_db || scale != self.preferences.display.eq_frequency_scale
        {
            let mut preferences = self.preferences.clone();
            preferences.eq_plot_height = eq;
            preferences.live_view_height = Some(live);
            preferences.eq_range_db = range;
            preferences.display.eq_frequency_scale = scale;
            preferences.save(&self.directory)?;
            self.preferences = preferences;
        }
        Ok(())
    }

    fn set_phase_mode(&mut self, mode: phase::Mode) -> Result<(), String> {
        if mode != self.preferences.phase_mode {
            let mut preferences = self.preferences.clone();
            preferences.phase_mode = mode;
            preferences.save(&self.directory)?;
            self.preferences = preferences;
            self.apply_audio();
        }
        Ok(())
    }

    fn crossfeed_output(&self) -> Option<&str> {
        let format = self.format.as_ref()?;
        format.output.as_deref().filter(|_| format.channels >= 2)
    }

    fn crossfeed(&self) -> Crossfeed {
        self.crossfeed_output().and_then(|output| self.preferences.headphone_crossfeed.get(output))
            .copied().unwrap_or_default()
    }

    fn room_output(&self) -> Option<&str> {
        self.crossfeed_output().filter(|_| self.format.as_ref().is_some_and(|format| format.channels == 2))
    }

    fn room_percent(&self) -> u8 {
        self.room_output().and_then(|output| self.preferences.headphone_room_percent.get(output))
            .copied().unwrap_or_default().min(100)
    }

    fn playback_effects(&self) -> engine::PlaybackEffects {
        engine::PlaybackEffects {
            crossfeed: self.crossfeed(),
            output: resonance_ipc::OutputEffects {
                room_amount: f64::from(self.room_percent()) / 100.0,
                limiter_enabled: self.preferences.limiter_enabled,
            },
        }
    }

    fn set_advanced_effects(&mut self, crossfeed: Crossfeed, room: u8, limiter: bool) -> bool {
        let before = self.playback_effects();
        if before.crossfeed != crossfeed && let Some(output) = self.crossfeed_output().map(str::to_owned) {
            if crossfeed.percent() == 0 {
                self.preferences.headphone_crossfeed.remove(&output);
            } else {
                self.preferences.headphone_crossfeed.insert(output, crossfeed);
            }
        }
        if self.room_percent() != room && let Some(output) = self.room_output().map(str::to_owned) {
            if room == 0 {
                self.preferences.headphone_room_percent.remove(&output);
            } else {
                self.preferences.headphone_room_percent.insert(output, room.min(100));
            }
        }
        self.preferences.limiter_enabled = limiter;

        let after = self.playback_effects();
        if before == after {
            return false;
        }
        if let Some(format) = &self.format {
            self.engine.submit(format, self.effective.clone(), self.preferences.phase_mode, after);
        }
        true
    }

    fn apply_audio(&mut self) {
        // Keep the saved Standard bands intact while only Simple tone filters are active.
        let (rate, channels) = self.format.as_ref().map_or((48000.0, 2), |format| (format.sample_rate, format.channels));
        let config = if self.simple.active {
            self.simple.config(self.cfg.bypass, self.preferences.simple_auto_headroom, self.cfg.balance)
        } else {
            let (b, a) = listening::prepare(&self.cfg, self.reference.as_ref(),
                self.preferences.auto_headroom, rate, channels, self.preferences.phase_mode);
            if self.listening_to_a { a.unwrap_or(b) } else { b }
        };
        if let Some(format) = &self.format {
            self.engine.submit(format, config.clone(), self.preferences.phase_mode, self.playback_effects());
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

    fn has_unsaved_preset_edits(&self) -> bool {
        self.edited && self.selected.and_then(|index| self.presets.get(index))
            .is_none_or(|preset| !preset_matches_config(&preset.config, &self.cfg))
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

    fn remove_preset(&mut self, id: &str) -> Result<Option<String>, String> {
        let index = self.presets.iter().position(|preset| preset.id() == id)
            .ok_or("This preset is no longer available.")?;
        let custom = self.presets[index].is_custom();
        let unlink_active = self.active_output.as_ref().and_then(|output| self.preferences.outputs.get(output))
            .is_some_and(|linked| linked == id);
        let mut preferences = self.preferences.clone();
        preferences.outputs.retain(|_, linked| linked != id);
        let links_changed = preferences.outputs != self.preferences.outputs;
        if custom {
            self.presets[index].delete_file()?;
        } else {
            preferences.hidden_builtin_presets.insert(id.to_owned());
            preferences.save(&self.directory)?;
        }

        self.preferences = preferences;
        self.presets.remove(index);
        remap_preset_selection(index, &mut self.selected, &mut self.edited);
        if unlink_active {
            self.fallback = None;
        } else if let Some((_, selected, edited)) = &mut self.fallback {
            remap_preset_selection(index, selected, edited);
        }
        for previous in &mut self.undo {
            remap_preset_selection(index, &mut previous.selected, &mut previous.edited);
        }
        self.pending_preset_load = None;
        // The custom file is already gone. Keep the editor and list consistent
        // even if persisting the now-inactive output links fails.
        Ok(if custom && links_changed { self.preferences.save(&self.directory).err() } else { None })
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
        self.pending_preset_load = None;
        self.pending_preset_deletion = None;
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

fn remap_preset_selection(removed: usize, selection: &mut Option<usize>, edited: &mut bool) {
    match *selection {
        Some(index) if index == removed => {
            *selection = None;
            *edited = true;
        }
        Some(index) if index > removed => *selection = Some(index - 1),
        _ => {}
    }
}

fn preset_matches_config(preset: &Config, current: &Config) -> bool {
    let mut saved = preset.clone();
    saved.bypass = current.bypass;
    saved.show_live_view = current.show_live_view;
    // Band edit history only affects removal warnings.
    for (saved_band, current_band) in saved.bands.iter_mut().zip(&current.bands) {
        saved_band.edited = current_band.edited;
    }
    &saved == current
}

fn text_model(values: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    Rc::new(VecModel::from(values.into_iter().map(Into::into).collect::<Vec<_>>())).into()
}

fn band_row(cfg: &Config, index: usize, decimals: usize) -> BandRow {
    let band = &cfg.bands[index];
    BandRow {
        frequency: format!("{:.1}", band.frequency_hz).into(),
        gain: band.gain_db as f32,
        gain_label: format!("{:+.*} dB", decimals, band.gain_db).into(),
        q: format!("{:.2}", band.q).into(),
        kind: match band.kind.as_str() { "LS" => 1, "HS" => 2, _ => 0 },
        enabled: band.enabled,
        locked: cfg.locked_point(index),
    }
}

// #######################################################
// draw the graph and update its appearance
// #######################################################

fn frequency_tick(frequency: u32, position: f32) -> FrequencyTick {
    let label = match frequency {
        20 => "20 Hz".into(),
        20000 => "20 kHz".into(),
        1000.. => format!("{}k", frequency / 1000).into(),
        _ => frequency.to_string().into(),
    };
    FrequencyTick { position, label }
}

fn update_eq_frequency_ticks(ui: &MainWindow) {
    let scale = graph::FrequencyScale::from_index(ui.get_eq_frequency_scale_index());
    let ticks = scale.labeled_frequencies().into_iter()
        .map(|frequency| frequency_tick(frequency, scale.x(f64::from(frequency)) as f32)).collect::<Vec<_>>();
    ui.set_eq_frequency_ticks(Rc::new(VecModel::from(ticks)).into());
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
    app.plot.set_range(ui.get_eq_range_db());
    app.plot.set_frequency_scale(graph::FrequencyScale::from_index(ui.get_eq_frequency_scale_index()));
    app.plot.set_phase_mode(app.preferences.phase_mode);
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

// #######################################################
// keep the controls aligned with the active tuning
// #######################################################

fn update_advanced_controls(ui: &MainWindow, app: &App) {
    ui.set_crossfeed_available(app.crossfeed_output().is_some());
    ui.set_crossfeed_percent(f32::from(app.crossfeed().percent()));
    ui.set_room_available(app.room_output().is_some());
    ui.set_room_percent(f32::from(app.room_percent()));
    ui.set_limiter_enabled(app.preferences.limiter_enabled);
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
    update_advanced_controls(ui, app);
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

fn update_phase_setting(ui: &MainWindow, app: &App) {
    let rate = app.fit_rate();
    let details = phase::Mode::OPTIONS.map(|mode| {
        if mode == phase::Mode::Matched {
            let delay = matched::latency_ms(rate);
            format!("Adds about {delay:.1} ms of EQ buffering.")
        } else {
            mode.delay_budget_ms().map(|delay| format!("Adds up to {delay} ms of EQ buffering.")).unwrap_or_default()
        }
    });
    ui.set_phase_mode_details(text_model(details));
    ui.set_phase_mode_index(app.preferences.phase_mode.index());
}

fn refresh_band_rows(ui: &MainWindow, cfg: &Config) {
    let decimals = graph::GainAxis::new(ui.get_eq_range_db()).gain_decimals();
    let rows = ui.get_bands();
    if rows.row_count() == cfg.bands.len() {
        for index in 0..cfg.bands.len() {
            let row = band_row(cfg, index, decimals);
            if rows.row_data(index).as_ref() != Some(&row) {
                rows.set_row_data(index, row);
            }
        }
    } else {
        ui.set_bands(Rc::new(VecModel::from((0..cfg.bands.len()).map(|index| band_row(cfg, index, decimals)).collect::<Vec<_>>())).into());
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

// #######################################################
// load and protect preset edits
// #######################################################

fn load_preset(ui: &MainWindow, app: &mut App, load: PresetLoad) {
    let previous = app.undo_state();
    match load {
        PresetLoad::Saved(index) => app.select_preset(index),
        PresetLoad::Apo(path) => {
            if let Err(error) = app.import_apo(&path) {
                rfd::MessageDialog::new().set_title("Import Equalizer APO")
                    .set_level(rfd::MessageLevel::Error).set_description(error).show();
                return;
            }
            refresh_preset_names(ui, app);
        }
    }
    app.remember(previous, None);
    app.simple.active = false;
    ui.invoke_clear_point_selection();
    ui.set_simple_layout(false);
    ui.set_notice("".into());
    apply_and_update_controls(ui, app);
}

fn request_preset_load(ui: &MainWindow, app: &mut App, load: PresetLoad) {
    let name: SharedString = match &load {
        PresetLoad::Saved(index) => {
            let Some(preset) = app.presets.get(*index) else { return; };
            if preset_matches_config(&preset.config, &app.cfg) {
                load_preset(ui, app, load);
                return;
            }
            preset.config.name.as_str().into()
        }
        PresetLoad::Apo(path) => path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().as_ref().into(),
    };
    if app.has_unsaved_preset_edits() {
        let importing = matches!(&load, PresetLoad::Apo(_));
        app.pending_preset_load = Some(load);
        ui.invoke_warn_preset_load(name, importing);
    } else {
        load_preset(ui, app, load);
    }
}

fn resolve_preset_load(ui: &MainWindow, app: &mut App, confirmed: bool) {
    if let Some(load) = app.pending_preset_load.take() && confirmed {
        load_preset(ui, app, load);
    }
}

fn request_preset_deletion(ui: &MainWindow, app: &mut App, index: usize) {
    if let Some(preset) = app.presets.get(index) {
        app.pending_preset_deletion = Some(preset.id());
        ui.invoke_warn_preset_deletion(preset.config.name.as_str().into(), preset.is_custom());
    }
}

fn resolve_preset_deletion(ui: &MainWindow, app: &mut App, confirmed: bool) -> Result<(), String> {
    let Some(id) = app.pending_preset_deletion.take().filter(|_| confirmed) else { return Ok(()) };
    match app.remove_preset(&id) {
        Ok(link_error) => {
            refresh_preset_names(ui, app);
            update_controls(ui, app);
            let message = link_error.map_or_else(|| "Preset removed.".to_owned(), |error|
                format!("Preset removed. Could not save the output links ({error})."));
            ui.set_notice(message.into());
            Ok(())
        }
        Err(error) => {
            app.pending_preset_deletion = Some(id);
            Err(error)
        }
    }
}

// #######################################################
// apply an edit and finish any pending curve fit
// #######################################################

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
            finish_fit(ui, app, candidate, error);
        }
        Err(mpsc::TryRecvError::Empty) => app.pending_fit = Some(job),
        Err(mpsc::TryRecvError::Disconnected) => {
            ui.set_fitting_eq(false);
            ui.set_notice("Could not fit the EQ curve.".into());
        }
    }
}

fn finish_fit(ui: &MainWindow, app: &mut App, candidate: Config, error: f64) {
    if commit_edit(ui, app, candidate, None) {
        app.fit_cache.remember(&app.cfg, error);
        ui.set_fit_detail(format!("Fit Error {error:.2} dB").into());
        if error > fit::TOLERANCE_DB {
            ui.set_notice(format!("The fitted curve differs by up to {error:.2} dB. Use Undo to restore the previous EQ.").into());
        }
    }
}

fn delete_band(ui: &MainWindow, app: &mut App, index: usize) {
    if app.listening_to_a || app.simple.active { return; }
    let Some(band) = app.cfg.bands.get(index) else { return; };
    if app.cfg.locked_point(index) { return; }
    ui.invoke_clear_point_selection();
    let mut candidate = app.cfg.clone();
    candidate.bands.remove(index);
    if band.needs_removal_confirmation() {
        app.pending_band_removal = Some((app.cfg.clone(), candidate));
        ui.invoke_warn_single_band_removal((index + 1) as i32);
    } else {
        app.pending_band_removal = None;
        commit(ui, app, candidate);
    }
}

fn delete_band_at(ui: &MainWindow, app: &mut App, x: f32, y: f32, width: f32, height: f32) {
    if let Some(index) = graph::hit_band(&app.plot.dots, x.into(), y.into(), width.into(), height.into()) {
        delete_band(ui, app, index);
    }
}

fn resolve_band_removal(ui: &MainWindow, app: &mut App, confirmed: bool) {
    if let Some((original, candidate)) = app.pending_band_removal.take()
        && confirmed && !app.listening_to_a && !app.simple.active && app.cfg == original
    {
        commit(ui, app, candidate);
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

// #######################################################
// find window bounds and load local settings
// #######################################################

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

// #######################################################
// start the app and connect it to the sound engine
// #######################################################

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Resolve desktop autostart before isolating the engine's config.
    #[cfg(target_os = "linux")]
    let login_entry = config_home().join("autostart/simple-modern-equalizer.desktop");
    #[cfg(target_os = "linux")]
    let flatpak_id = std::env::var("FLATPAK_ID").ok();
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
    let engine_socket = directory.join("engine.sock");
    #[cfg(target_os = "linux")]
    let engine_socket = match (flatpak_id.as_deref(), std::env::var_os("XDG_RUNTIME_DIR")) {
        (Some(id), Some(runtime)) => {
            // Flatpak config paths can exceed the Unix socket path limit.
            let runtime = PathBuf::from(runtime).join("app").join(id);
            fs::create_dir_all(&runtime)?;
            runtime.join("engine.sock")
        }
        _ => engine_socket,
    };
    // Before any UI/worker threads exist: isolate our IPC endpoint and engine
    // preferences. Windows APO registration is still the vendored engine's ID.
    unsafe {
        std::env::set_var("RESONANCE_SOCKET", engine_socket);
        std::env::set_var("XDG_CONFIG_HOME", directory.join("engine-config"));
    }
    let mut presets = Preset::bundled().map_err(std::io::Error::other)?;
    let flat = presets[0].config.clone();
    let mut cfg = read_config(&directory.join("current.json")).unwrap_or_else(|_| flat.clone());
    if cfg.bands == Config::default().bands && cfg.name == flat.name && !cfg.spline {
        cfg.bands = flat.bands;
        cfg.spline = true;
    }
    Preset::restore_builtin_name(&presets, &mut cfg);
    let preferences: Preferences = storage::read_text(&directory.join("preferences.json")).ok()
        .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()).unwrap_or_default();
    presets.retain(|preset| !preferences.hidden_builtin_presets.contains(&preset.id()));
    presets.extend(Preset::load_custom(&directory.join("presets"))?);
    let selected = presets.iter().position(|preset| preset_matches_config(&preset.config, &cfg));
    let edited = selected.is_none();
    let simple: simple::SimpleSettings = storage::read_text(&directory.join("simple.json")).ok()
        .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()).unwrap_or_default();
    let initial = if simple.active { simple.config(cfg.bypass, preferences.simple_auto_headroom, cfg.balance) } else {
        listening::prepare(&cfg, None, preferences.auto_headroom, 48000.0, 2, preferences.phase_mode).0
    };
    let app = Rc::new(RefCell::new(App { engine: engine::Engine::start(&directory), cfg, simple, presets,
        selected, preferences, active_output: None, fallback: None, reference: None, listening_to_a: false,
        effective: initial,
        outputs: vec![], directory, edited, plot: graph::GraphCache::default(), drag_origin: None,
        pending_band_removal: None, pending_preset_load: None, pending_preset_deletion: None,
        plot_spectrum: vec![], format: None,
        undo: VecDeque::new(), edit_group: None, pending_fit: None, fit_cache: fit::Cache::default() }));
    let ui = MainWindow::new()?;
    #[cfg(target_os = "linux")]
    {
        slint::set_xdg_app_id(flatpak_id.as_deref().unwrap_or("simple-modern-equalizer"))?;
        if flatpak_id.is_none() {
            ui.set_has_login_start(true);
            ui.set_login_start(autostart::enabled(&login_entry));
            ui.on_change_login_start(move |enabled| {
                match autostart::set_enabled(&login_entry, enabled) {
                    Ok(()) => "".into(),
                    Err(error) => format!("Could not update login startup. {error}").into(),
                }
            });
        }
    }
    configure_color_picker(&ui);
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_source_license(env!("CARGO_PKG_LICENSE").into());
    ui.set_license_text(include_str!("../LICENSE").into());
    apply_colors(&ui, app.borrow().preferences.colors);
    let _widget_accent = keep_widget_accent(&ui);
    let frequency_ticks = live::frequency_ticks().map(|(frequency, position)| frequency_tick(frequency, position))
        .collect::<Vec<_>>();
    ui.set_live_frequency_ticks(Rc::new(VecModel::from(frequency_ticks)).into());
    ui.set_eq_range_db(app.borrow().preferences.eq_range_db());
    ui.set_eq_frequency_scale_index(app.borrow().preferences.display.eq_frequency_scale.index());
    update_eq_frequency_ticks(&ui);
    update_controls(&ui, &app.borrow());
    update_phase_setting(&ui, &app.borrow());
    draw_eq_graph(&ui, &mut app.borrow_mut());
    {
        let state = app.borrow();
        ui.set_live_view_height(state.preferences.live_view_height());
        ui.set_eq_plot_height(state.preferences.eq_plot_height());
        let display = &state.preferences.display;
        ui.set_live_mode(display.live_mode_index());
        ui.set_show_band_list(display.band_list);
        ui.set_show_levels(display.levels);
        ui.set_show_advanced(display.advanced);
        ui.set_show_clipping(display.clipping);
        ui.set_show_limiter_overlay(display.limiter_overlay);
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

    // #######################################################
    // connect layout and EQ editor controls
    // #######################################################

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
        ui.on_save_graph_view(move || {
            let weak = weak.clone();
            let app = app.clone();
            timer.start(slint::TimerMode::SingleShot, Duration::from_millis(250), move || {
                if let Some(w) = weak.upgrade()
                    && let Err(error) = app.borrow_mut().save_graph_view(&w)
                {
                    w.set_notice(error.into());
                }
            });
        });
    }
    on_ui_action!(on_change_eq_range, |w, a, | {
        a.drag_origin = None;
        w.invoke_cancel_graph_drag();
        let cfg = a.reference.as_ref().filter(|_| a.listening_to_a).unwrap_or(&a.cfg);
        refresh_band_rows(&w, cfg);
        draw_eq_graph(&w, &mut a);
    });
    on_ui_action!(on_change_eq_frequency_scale, |w, a, | {
        a.drag_origin = None;
        w.invoke_cancel_graph_drag();
        update_eq_frequency_ticks(&w);
        draw_eq_graph(&w, &mut a);
    });
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
    {
        let save_timer = slint::Timer::default();
        let save_state = app.clone();
        on_ui_action!(on_change_advanced_effects, |w, a, | {
            let crossfeed = Crossfeed::new(w.get_crossfeed_percent().round() as u8);
            let room = w.get_room_percent().round().clamp(0.0, 100.0) as u8;
            let changed = a.set_advanced_effects(crossfeed, room, w.get_limiter_enabled());
            update_advanced_controls(&w, &a);
            if changed {
                // Apply each slider change live and save after the slider settles.
                let weak = w.as_weak();
                let state = save_state.clone();
                save_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(250), move || {
                    let a = state.borrow();
                    if let Err(error) = a.preferences.save(&a.directory) && let Some(w) = weak.upgrade() {
                        w.set_notice(error.into());
                    }
                });
            }
        });
    }
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
        if let Some((candidate, error)) = a.fit_cache.reuse(&a.cfg, rate) {
            finish_fit(&w, &mut a, candidate, error);
            return;
        }
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
        delete_band(&w, &mut a, index as usize);
    });
    on_ui_action!(on_delete_band_at, |w, a, x, y, width, height| {
        delete_band_at(&w, &mut a, x, y, width, height);
    });
    on_ui_action!(on_resolve_band_removal, |w, a, confirmed| {
        resolve_band_removal(&w, &mut a, confirmed);
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
        let (frequency, gain) = a.plot.axis.band_values(a.plot.frequency_scale, x.into(), y.into());
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
                graph::BandDrag::new(index, band).with_axes(a.plot.axis, a.plot.frequency_scale)
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
        request_preset_load(&w, &mut a, PresetLoad::Saved(index as usize));
    });
    on_ui_action!(on_resolve_preset_load, |w, a, confirmed| {
        resolve_preset_load(&w, &mut a, confirmed);
    });
    on_ui_action!(on_request_preset_deletion, |w, a, index| {
        request_preset_deletion(&w, &mut a, index as usize);
    });
    {
        let weak = ui.as_weak();
        let app = app.clone();
        ui.on_resolve_preset_deletion(move |confirmed| {
            let Some(w) = weak.upgrade() else { return "".into() };
            resolve_preset_deletion(&w, &mut app.borrow_mut(), confirmed).err().unwrap_or_default().into()
        });
    }
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

    // #######################################################
    // connect A/B comparison and file actions
    // #######################################################

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
    on_ui_action!(on_change_phase_mode, |w, a, index| {
        if let Some(&mode) = phase::Mode::OPTIONS.get(index as usize) {
            let error = a.set_phase_mode(mode).err().unwrap_or_default();
            w.set_phase_mode_error(error.into());
        }
        update_phase_setting(&w, &a);
        update_controls(&w, &a);
        draw_eq_graph(&w, &mut a);
    });
    on_ui_action!(on_import_apo, |w, a, | {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Import Equalizer APO")
            .add_filter("Equalizer APO", &["txt"])
            .pick_file()
        {
            request_preset_load(&w, &mut a, PresetLoad::Apo(path));
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

    // #######################################################
    // refresh the UI from the audio and media workers
    // #######################################################

    let weak = ui.as_weak(); let timer_state = app.clone();
    let live_meters = RefCell::new(live::LiveMeters::default());
    let monitor_plots = RefCell::new(monitor::Plots::default());
    let mut loudness = monitor::History::default();
    ui.set_has_now_playing(cfg!(any(windows, target_os = "linux")));
    let media_updates = now_playing::subscribe();
    let engine_state = app.borrow().engine.shared.clone();

    // #######################################################
    // connect the true-peak reset control
    // #######################################################

    let true_peak_display = Rc::new(RefCell::new(None::<live::TruePeakDisplay>));
    ui.on_reset_true_peak({
        let display = true_peak_display.clone();
        let engine = engine_state.clone();
        move || {
            if let Some(display) = display.borrow_mut().as_mut() {
                display.reset_maximum();
            }
            // Clear readings that were collected before the reset but have yet to be displayed.
            let mut shared = engine.lock().unwrap();
            shared.true_peaks = None;
            shared.reset_true_peak = true;
        }
    });
    let mut stereo_levels = [-60.0; 2];
    let mut last_frame = Instant::now();
    let mut next_control = last_frame;
    let mut next_display_probe = last_frame;
    let mut next_media_refresh = last_frame;
    let mut display_interval = live::display_interval(None);
    let mut clip_until = Instant::now();
    let mut limiter_until = None;
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
        let limiter_overlay = show_eq_graph && w.get_show_limiter_overlay()
            && w.get_limiter_enabled() && !a.cfg.bypass;
        let true_peak_active = show_live_view && mode == live::TRUE_PEAK_MODE;
        let (control, stereo_rms, waveform, monitor_signal, true_peaks) = {
            let mut shared = engine_state.lock().unwrap();
            shared.monitor_mode = (show_live_view && matches!(mode, live::BEFORE_AFTER_MODE | live::STEREO_FIELD_MODE | live::TRUE_PEAK_MODE))
                .then_some(mode);
            shared.meter_capture_requested = cfg!(windows) && show_live_view && mode == live::STEREO_LEVEL_MODE;
            shared.scope_requested = show_live_view && mode == scope::MODE;
            shared.limiter_overlay_requested = limiter_overlay;
            if !limiter_overlay {
                shared.limiter_hit = None;
            }
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
                Some((snapshot, shared.status.clone(), std::mem::take(&mut shared.clipped), clip_spectrum,
                    shared.limiter_reduction_db, shared.limiter_hit.take()))
            } else { None };
            let waveform = if shared.scope_requested { shared.scope_samples.clone() } else { None };
            let signal = shared.monitor_signal.as_ref()
                .filter(|signal| shared.monitor_mode.is_some_and(|mode| signal.has_mode(mode)))
                .cloned();
            (control, stereo_rms, waveform, signal, shared.true_peaks.take())
        };
        if let Some((snapshot, status, clipped, clip_spectrum, limiter_reduction, limiter_hit)) = control {
            let connection_changed = w.get_connected() != snapshot.is_some();
            w.set_status(status.into()); w.set_connected(snapshot.is_some());
            let reduction = if w.get_connected() && w.get_limiter_enabled() && !a.cfg.bypass {
                limiter_reduction.abs()
            } else { 0.0 };
            w.set_limiter_reduction_label(format!("{:.1} dB reduction", reduction).into());
            if !limiter_overlay || !snapshot.as_ref().is_some_and(|(_, format, ..)| a.format.as_ref() == Some(format)) {
                limiter_until = None;
            } else if let Some(hit) = limiter_hit {
                limiter_until = Some(hit.time + Duration::from_millis(900));
                w.set_limiter_graph_label(format!("Limiter −{:.1} dB", hit.reduction_db).into());
                let regions = hit.regions.into_iter().map(|(start, end)| FrequencyRegion { start, end }).collect::<Vec<_>>();
                w.set_limiter_regions(Rc::new(VecModel::from(regions)).into());
            }
            if limiter_until.is_none_or(|until| now >= until) && w.get_limiter_graph_label() != "" {
                w.set_limiter_graph_label("".into());
                w.set_limiter_regions(ModelRc::default());
            }
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
                        let regions = spectrum::dominant_regions(&clip_spectrum);
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
                    update_phase_setting(&w, &a);
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
        if w.get_has_now_playing() && now >= next_media_refresh {
            next_media_refresh = now + Duration::from_millis(500);
            if let Ok(track) = media_updates.try_recv() {
                let track = track.as_ref();
                w.set_playback_details(PlaybackDetails {
                    title: track.map_or("No track reported", |track| track.title.as_str()).into(),
                    artist: track.map_or("", |track| track.artist.as_str()).into(),
                });
            }
        }
        // A new visit to True Peak starts a new maximum.
        if !true_peak_active {
            true_peak_display.borrow_mut().take();
        }
        if true_peak_active {
            let mut display = true_peak_display.borrow_mut();
            let ([left, right], [max_left, max_right]) = display.get_or_insert_with(live::TruePeakDisplay::default)
                .update(true_peaks, elapsed);
            w.set_true_peak_left(left);
            w.set_true_peak_right(right);
            w.set_true_peak_max_left(max_left);
            w.set_true_peak_max_right(max_right);
        } else if show_live_view && mode == live::STEREO_LEVEL_MODE {
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
        advanced: ui.get_show_advanced(),
        clipping: ui.get_show_clipping(),
        limiter_overlay: ui.get_show_limiter_overlay(),
        preset_button: ui.get_show_preset_button(),
        eq_spectrum: ui.get_show_eq_spectrum(),
        eq_frequency_scale: graph::FrequencyScale::from_index(ui.get_eq_frequency_scale_index()),
    };
    a.save_graph_view(&ui)?;
    a.preferences.save(&a.directory)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headphone_effects_are_saved_for_each_output() {
        let mut app = app_for_preset_tests(Config::default());
        let mut format = engine::AudioFormat { channels: 2, sample_rate: 48_000.0, output: Some("Headphones".into()) };
        app.format = Some(format.clone());
        assert_eq!(app.crossfeed().percent(), 0);
        assert_eq!(app.room_percent(), 0);
        assert!(!app.preferences.limiter_enabled);
        assert!(app.set_advanced_effects(Crossfeed::new(37), 44, true));
        app.preferences.save(&app.directory).unwrap();
        let saved: Preferences = serde_json::from_str(&storage::read_text(&app.directory.join("preferences.json")).unwrap()).unwrap();
        assert_eq!(saved.headphone_crossfeed["Headphones"].percent(), 37);
        assert_eq!(saved.headphone_room_percent["Headphones"], 44);
        assert!(saved.limiter_enabled);

        format.output = Some("HDMI".into());
        app.format = Some(format.clone());
        assert_eq!(app.crossfeed().percent(), 0);
        assert_eq!(app.room_percent(), 0);
        assert!(app.playback_effects().output.limiter_enabled);
        format.output = Some("Headphones".into());
        format.channels = 1;
        app.format = Some(format.clone());
        assert_eq!(app.crossfeed().percent(), 0);
        assert!(!app.set_advanced_effects(Crossfeed::new(25), 100, true));
        format.channels = 2;
        app.format = Some(format);
        assert_eq!(app.crossfeed().percent(), 37);
        assert_eq!(app.room_percent(), 44);
        assert!(app.set_advanced_effects(Crossfeed::default(), 0, false));
        assert!(app.preferences.headphone_crossfeed.is_empty());
        assert!(app.preferences.headphone_room_percent.is_empty());
        fs::remove_dir_all(&app.directory).unwrap();
    }

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
            pending_preset_load: None,
            pending_preset_deletion: None,
            plot_spectrum: vec![],
            format: None,
            undo: VecDeque::new(),
            edit_group: None,
            pending_fit: None,
            fit_cache: fit::Cache::default(),
        }
    }

    #[test]
    fn phase_selection_persists_outside_the_preset() {
        let mut app = app_for_preset_tests(Config::default());
        let old: Preferences = serde_json::from_str("{\"auto_headroom\":true}").unwrap();
        assert_eq!(old.phase_mode, phase::Mode::Minimum);
        assert!(old.auto_headroom);

        for mode in [phase::Mode::Zero, phase::Mode::Matched] {
            app.set_phase_mode(mode).unwrap();
            let saved: Preferences = serde_json::from_str(
                &storage::read_text(&app.directory.join("preferences.json")).unwrap(),
            ).unwrap();
            assert_eq!(saved.phase_mode, mode);
        }
        assert_eq!(app.cfg, Config::default());
        fs::remove_dir_all(&app.directory).unwrap();
    }

    #[test]
    fn deleting_a_preset_requires_confirmation_and_repairs_saved_selections() {
        use i_slint_backend_testing::{AccessibleRole, ElementHandle};

        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(683.0, 384.0));
        let mut app = app_for_preset_tests(Config::default());
        app.save_preset("Desk").unwrap();
        let desk = app.presets[1].id();
        app.remember_current(None);
        app.fallback = Some((app.cfg.clone(), app.selected, app.edited));
        app.save_preset("Keep").unwrap();
        let keep = app.presets[2].id();
        app.remember_current(None);
        let playing = app.cfg.clone();
        app.preferences.outputs.insert("Desk DAC".into(), desk.clone());
        app.preferences.outputs.insert("Other DAC".into(), keep.clone());
        app.preferences.save(&app.directory).unwrap();
        refresh_preset_names(&ui, &app);
        update_controls(&ui, &app);
        let app = Rc::new(RefCell::new(app));
        let weak = ui.as_weak();
        let state = app.clone();
        ui.on_request_preset_deletion(move |index| {
            request_preset_deletion(&weak.unwrap(), &mut state.borrow_mut(), index as usize);
        });
        let weak = ui.as_weak();
        let state = app.clone();
        ui.on_resolve_preset_deletion(move |confirmed| {
            resolve_preset_deletion(&weak.unwrap(), &mut state.borrow_mut(), confirmed)
                .err().unwrap_or_default().into()
        });
        ui.show().unwrap();
        let button = |name: &str| ElementHandle::find_by_accessible_label(&ui, name)
            .find(|element| element.accessible_role() == Some(AccessibleRole::Button)).unwrap();
        let custom = || Preset::load_custom(&app.borrow().directory.join("presets")).unwrap();

        button("Select Preset").invoke_accessible_default_action();
        let name = button("Desk");
        let delete = button("Delete Desk");
        assert!(delete.absolute_position().x >= name.absolute_position().x + name.size().width);
        delete.mock_single_click(slint::platform::PointerEventButton::Left);
        assert_eq!(app.borrow().pending_preset_deletion.as_deref(), Some(desk.as_str()));
        assert_eq!(custom().len(), 2);
        button("Cancel").invoke_accessible_default_action();
        assert!(app.borrow().pending_preset_deletion.is_none());
        assert_eq!(custom().len(), 2);

        button("Delete Desk").invoke_accessible_default_action();
        button("Delete preset").invoke_accessible_default_action();
        assert!(ElementHandle::find_by_accessible_label(&ui, "Delete Desk").next().is_none());
        assert!(ElementHandle::find_by_accessible_label(&ui, "Keep (selected)").next().is_some());
        assert_eq!(custom().into_iter().map(|preset| preset.id()).collect::<Vec<_>>(), [keep.clone()]);
        let app = app.borrow();
        assert_eq!(app.cfg, playing);
        assert_eq!(app.selected, Some(1));
        assert!(!app.edited);
        assert!(matches!(&app.fallback, Some((_, None, true))));
        assert_eq!((app.undo[0].selected, app.undo[0].edited), (None, true));
        assert_eq!(app.undo[1].selected, Some(1));
        let saved: Preferences = serde_json::from_str(
            &storage::read_text(&app.directory.join("preferences.json")).unwrap(),
        ).unwrap();
        assert!(!saved.outputs.contains_key("Desk DAC"));
        assert_eq!(saved.outputs["Other DAC"], keep);
        fs::remove_dir_all(&app.directory).unwrap();
    }

    #[test]
    fn deleting_flat_hides_it_for_later_sessions_and_preserves_the_active_eq() {
        let flat = Config::flat_spline();
        let mut app = app_for_preset_tests(flat.clone());
        app.presets = Preset::bundled().unwrap();
        let preset_count = app.presets.len();
        let flat_id = app.presets[0].id();
        app.active_output = Some("DAC".into());
        app.fallback = Some((Config::default(), None, true));
        app.preferences.outputs.insert("DAC".into(), flat_id.clone());
        assert!(app.remove_preset(&flat_id).unwrap().is_none());
        assert_eq!(app.cfg, flat);
        assert_eq!(app.selected, None);
        assert!(app.edited);
        assert!(app.fallback.is_none());
        let saved: Preferences = serde_json::from_str(
            &storage::read_text(&app.directory.join("preferences.json")).unwrap(),
        ).unwrap();
        assert!(saved.hidden_builtin_presets.contains(&flat_id));
        assert!(saved.outputs.is_empty());
        assert_eq!(app.presets.len(), preset_count - 1);
        fs::remove_dir_all(&app.directory).unwrap();
    }

    #[test]
    fn right_click_removes_the_hit_point_and_respects_confirmation_and_endpoints() {
        use i_slint_backend_testing::{AccessibleRole, ElementHandle};
        use slint::{LogicalPosition, LogicalSize, platform::{PointerEventButton, WindowEvent}};

        i_slint_backend_testing::init_no_event_loop();
        let mut config = Config::flat_spline();
        config.show_live_view = false;
        assert!(config.add_peak(500.0, 4.0));
        assert!(config.add_peak(2000.0, -6.0));
        config.bands[2].edited = Some(false);
        let app = Rc::new(RefCell::new(app_for_preset_tests(config.clone())));
        let ui = MainWindow::new().unwrap();
        ui.window().set_size(LogicalSize::new(1056.0, 860.0));
        ui.show().unwrap();
        let weak = ui.as_weak();
        let state = app.clone();
        ui.on_delete_band_at(move |x, y, width, height| {
            delete_band_at(&weak.unwrap(), &mut state.borrow_mut(), x, y, width, height);
        });
        let weak = ui.as_weak();
        let state = app.clone();
        ui.on_resolve_band_removal(move |confirmed| {
            resolve_band_removal(&weak.unwrap(), &mut state.borrow_mut(), confirmed);
        });
        apply_and_update_controls(&ui, &mut app.borrow_mut());

        let plot = ElementHandle::find_by_element_id(&ui, "MainWindow::plot").next().unwrap();
        let origin = plot.absolute_position();
        let size = plot.size();
        let right_click = |x: f64, y: f64| {
            let position = LogicalPosition::new(origin.x + (size.width * x as f32).max(1.0),
                origin.y + size.height * y as f32);
            ui.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Right });
            ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Right });
        };
        let dots = app.borrow().plot.dots.clone();
        right_click(0.8, 0.1);
        right_click(dots[0].x, dots[0].y);
        assert_eq!(app.borrow().cfg, config);
        assert!(app.borrow().pending_band_removal.is_none());

        for button in ["Cancel", "Remove"] {
            right_click(dots[1].x, dots[1].y);
            assert_eq!(app.borrow().cfg, config);
            ElementHandle::find_by_accessible_label(&ui, button)
                .find(|element| element.accessible_role() == Some(AccessibleRole::Button))
                .unwrap().invoke_accessible_default_action();
        }
        assert_eq!(app.borrow().cfg.bands, [config.bands[0].clone(), config.bands[2].clone(), config.bands[3].clone()]);
        right_click(dots[2].x, dots[2].y);
        assert_eq!(app.borrow().cfg.bands, [config.bands[0].clone(), config.bands[3].clone()]);
        assert!(app.borrow().pending_band_removal.is_none());
        assert!(app.borrow_mut().undo_edit().unwrap());
        assert_eq!(app.borrow().cfg.bands.len(), 3);
        fs::remove_dir_all(&app.borrow().directory).unwrap();
    }

    #[test]
    fn loading_presets_only_replaces_unsaved_eq_after_confirmation() {
        use i_slint_backend_testing::{AccessibleRole, ElementHandle};
        use slint::platform::{Key, WindowEvent};

        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        ui.show().unwrap();
        let app = Rc::new(RefCell::new(app_for_preset_tests(Config::default())));
        let weak = ui.as_weak();
        let state = app.clone();
        ui.on_resolve_preset_load(move |confirmed| {
            resolve_preset_load(&weak.unwrap(), &mut state.borrow_mut(), confirmed);
        });
        let weak = ui.as_weak();
        let state = app.clone();
        ui.on_choose_preset(move |index| {
            request_preset_load(&weak.unwrap(), &mut state.borrow_mut(), PresetLoad::Saved(index as usize));
        });
        let press = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .find(|element| element.accessible_role() == Some(AccessibleRole::Button))
                .unwrap().invoke_accessible_default_action();
        };
        {
            let mut app = app.borrow_mut();
            app.presets.push(Preset::builtin(Config { name: "Another preset".into(), ..Config::default() }));
            app.cfg.bypass = true;
            app.cfg.show_live_view = false;
            app.cfg.bands[0].edited = Some(true);
            refresh_preset_names(&ui, &app);
            update_controls(&ui, &app);
        }
        let choose = |index: usize| {
            let label = {
                let app = app.borrow();
                let suffix = if app.selected == Some(index) { " (selected)" } else { "" };
                format!("{}{suffix}", app.presets[index].config.name)
            };
            press("Select Preset");
            press(&label);
        };
        choose(1);
        assert_eq!(app.borrow().selected, Some(1));
        assert!(app.borrow().pending_preset_load.is_none());
        let edited = {
            let mut app = app.borrow_mut();
            app.cfg.bands[0].gain_db = 5.0;
            app.edited = true;
            app.cfg.clone()
        };
        choose(0);
        assert_eq!(app.borrow().cfg, edited);
        assert!(ElementHandle::find_by_accessible_label(&ui, "Unsaved EQ Changes").next().is_some());
        ui.window().dispatch_event(WindowEvent::KeyPressed { text: Key::Escape.into() });
        ui.window().dispatch_event(WindowEvent::KeyReleased { text: Key::Escape.into() });
        assert!(app.borrow().pending_preset_load.is_none());
        assert_eq!(app.borrow().cfg, edited);

        let source = app.borrow().directory.join("test.txt");
        fs::write(&source, "Filter: ON PK Fc 1000 Hz Gain -3 dB Q 1").unwrap();
        request_preset_load(&ui, &mut app.borrow_mut(), PresetLoad::Apo(source));
        press("Cancel");
        assert!(app.borrow().pending_preset_load.is_none());
        assert_eq!(app.borrow().cfg, edited);
        assert_eq!(fs::read_dir(app.borrow().directory.join("presets")).unwrap().count(), 0);

        choose(0);
        press("Load Preset");
        assert_eq!(app.borrow().selected, Some(0));
        assert!(!app.borrow().edited);
        assert!(app.borrow_mut().undo_edit().unwrap());
        assert_eq!(app.borrow().cfg, edited);
        fs::remove_dir_all(&app.borrow().directory).unwrap();
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
