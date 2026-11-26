use crate::{config::Config, phase, presets::Crossfeed};
use resonance_ipc::{Command, DaemonState, EffectsState, FxEffectId, OutputEffects, Response, RoutingMatrix, transport::SyncClient};
use std::{path::{Path, PathBuf}, process::{Child, Command as Process}, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, thread, time::{Duration, Instant}};

// #######################################################
// share playback format and monitor readings with the UI
// #######################################################

#[derive(Clone, Debug, PartialEq)]
pub struct AudioFormat {
    pub channels: usize,
    pub sample_rate: f64,
    pub output: Option<String>,
}

impl From<&DaemonState> for AudioFormat {
    fn from(state: &DaemonState) -> Self {
        Self {
            channels: state.channels,
            sample_rate: state.sample_rate,
            output: state.active_output.clone(),
        }
    }
}

#[derive(Default)]
pub struct Shared {
    prepared: Option<(AudioFormat, Arc<PlaybackSettings>)>,
    pub target: Option<Option<String>>,
    pub state: Option<DaemonState>,
    pub status: String,
    pub clipped: bool,
    pub scope_requested: bool,
    pub meter_capture_requested: bool,
    pub playback_levels: Option<([f32; 2], f32)>,
    pub scope_samples: Option<Arc<[f32]>>,
    pub spectrum_requested: Option<crate::spectrum::Resolution>,
    pub spectrum_levels: Option<Arc<crate::spectrum::Levels>>,
    pub monitor_mode: Option<i32>,
    pub monitor_signal: Option<Arc<crate::monitor::Signal>>,
    pub true_peaks: Option<[f32; 2]>,
    pub reset_true_peak: bool,
    pub limiter_reduction_db: f32,
    pub limiter_overlay_requested: bool,
    pub limiter_hit: Option<LimiterHit>,
}

pub struct LimiterHit {
    pub time: Instant,
    pub reduction_db: f32,
    pub regions: Vec<(f32, f32)>,
}

#[derive(Clone, Copy, Default, PartialEq)]
pub struct PlaybackEffects {
    pub crossfeed: Crossfeed,
    pub output: OutputEffects,
}

#[derive(Default)]
struct PlaybackSettings {
    config: Config,
    phase_mode: phase::Mode,
    playback_effects: PlaybackEffects,
}

impl PlaybackSettings {
    fn effects(&self) -> EffectsState {
        let (crossfeed_intensity, crossfeed_enabled) = self.playback_effects.crossfeed.settings();
        EffectsState { crossfeed_intensity, crossfeed_enabled, ..EffectsState::default() }
    }
}

impl Shared {
    fn prepared_for(&self, format: &AudioFormat) -> Option<Arc<PlaybackSettings>> {
        self.prepared.as_ref()
            .filter(|(prepared_format, _)| prepared_format == format)
            .map(|(_, cfg)| cfg.clone())
    }
}

pub struct Engine {
    pub shared: Arc<Mutex<Shared>>,
    running: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

fn request(client: &mut SyncClient, cmd: Command) -> Result<(), String> {
    match client.send_recv(cmd).map_err(|e| e.to_string())? {
        Response::Ok => Ok(()),
        Response::Error(e) => Err(e),
        _ => Err("Unexpected audio-engine reply".into()),
    }
}

pub fn balance_matrix(channels: usize, cfg: &Config) -> RoutingMatrix {
    let (l, r) = cfg.gains();
    let mut gains = vec![0.0; channels * channels];
    for ch in 0..channels { gains[ch * channels + ch] = match ch { 0 => l, 1 => r, _ => 1.0 }; }
    RoutingMatrix { in_ch: channels, out_ch: channels, gains }
}

// #######################################################
// turn EQ edits into engine commands
// #######################################################

fn update_plan(previous: Option<&Config>, cfg: &Config, channels: usize, sample_rate: f64) -> Result<Vec<Command>, String> {
    cfg.validate()?;
    if !(1..=64).contains(&channels) {
        return Err("Unsupported audio channel count".into());
    }
    let routing = || {
        if channels == 1 || cfg.gains() == (1.0, 1.0) {
            Command::ClearRouting
        } else {
            Command::SetChannelRouting { matrix: balance_matrix(channels, cfg) }
        }
    };
    let supported = |frequency: f64| frequency < sample_rate / 2.0;
    let bands = cfg.parametric_bands();
    let Some(old) = previous.filter(|old| {
        let old_bands = old.parametric_bands();
        old_bands.len() == bands.len() && old_bands.iter().zip(bands).all(|(a, b)| {
            // A preset may omit a band above Nyquist. Do not assume its
            // indices still identify the other bands in that situation.
            supported(a.frequency_hz) == supported(b.frequency_hz)
                && (!supported(a.frequency_hz) || a.kind == b.kind)
        })
    }) else {
        return Ok(vec![
            Command::ApplyState {
                preamp_db: cfg.preamp_db,
                enabled: !cfg.bypass,
                bands: bands.iter().filter(|band| supported(band.frequency_hz)).map(|band| band.state()).collect(),
                effects: EffectsState::default(),
            },
            routing(),
        ]);
    };

    let mut commands = Vec::new();
    if !old.bypass && cfg.bypass {
        commands.push(Command::SetPower { enabled: false });
    }
    if cfg.preamp_db < old.preamp_db {
        commands.push(Command::SetPreamp { db: cfg.preamp_db });
    }
    for (index, (before, after)) in old.parametric_bands().iter().zip(bands)
        .filter(|(_, band)| supported(band.frequency_hz)).enumerate()
    {
        if before.enabled && !after.enabled {
            commands.push(Command::SetBandEnabled { index, enabled: false });
        }
        if (before.frequency_hz, before.gain_db, before.q) != (after.frequency_hz, after.gain_db, after.q) {
            commands.push(Command::SetBand { index, freq: after.frequency_hz, gain_db: after.gain_db, q: after.q });
        }
        if !before.enabled && after.enabled {
            commands.push(Command::SetBandEnabled { index, enabled: true });
        }
    }
    if cfg.preamp_db > old.preamp_db {
        commands.push(Command::SetPreamp { db: cfg.preamp_db });
    }
    if channels > 1 && old.gains() != cfg.gains() {
        commands.push(routing());
    }
    if old.bypass && !cfg.bypass {
        commands.push(Command::SetPower { enabled: true });
    }
    Ok(commands)
}

// #######################################################
// order filter and gain changes for playback
// #######################################################

fn playback_plan(
    previous: Option<&PlaybackSettings>,
    desired: &PlaybackSettings,
    format: &AudioFormat,
    convolution: Option<Command>,
) -> Result<Vec<Command>, String> {
    let before = previous.map(|settings| settings.phase_mode.biquad_config(&settings.config));
    let after = desired.phase_mode.biquad_config(&desired.config);
    let mut commands = update_plan(before.as_deref(), &after, format.channels, format.sample_rate)?;
    if let Some(command) = commands.first_mut()
        && let Command::ApplyState { preamp_db, enabled, bands, .. } = command
    {
        *command = Command::ApplyStateWithOutputEffects {
            preamp_db: *preamp_db, enabled: *enabled, bands: std::mem::take(bands),
            effects: desired.effects(), output_effects: desired.playback_effects.output,
        };
    } else if let Some(old) = previous {
        let before = old.playback_effects;
        let after = desired.playback_effects;
        if before.crossfeed != after.crossfeed {
            // Set the amount first so enabling crossfeed starts at the selected strength.
            let (value, enabled) = after.crossfeed.settings();
            commands.push(Command::SetEffectIntensity { effect: FxEffectId::Crossfeed, value });
            if enabled != before.crossfeed.settings().1 {
                commands.push(Command::SetEffectEnabled { effect: FxEffectId::Crossfeed, enabled });
            }
        }
        if before.output != after.output {
            commands.push(Command::SetOutputEffects(after.output));
        }
    }
    if let Some(convolution) = convolution {
        let mut deferred = Vec::new();
        // A full sync swaps the IIR bands. Keep the chain bypassed until its
        // convolution is in place, and raise preamp only after the IR change.
        commands.retain_mut(|command| {
            match command {
                Command::ApplyStateWithOutputEffects { enabled, .. } if *enabled => {
                    *enabled = false;
                    deferred.push(Command::SetPower { enabled: true });
                }
                Command::SetPower { enabled: true } => {
                    deferred.push(Command::SetPower { enabled: true });
                    return false;
                }
                Command::SetPreamp { db } if before.as_ref().is_some_and(|cfg| *db > cfg.preamp_db) => {
                    deferred.push(Command::SetPreamp { db: *db });
                    return false;
                }
                _ => {}
            }
            true
        });
        commands.push(convolution);
        commands.extend(deferred);
    }
    Ok(commands)
}

fn apply_playback(
    settings: &PlaybackSettings,
    format: &AudioFormat,
    convolution: Option<String>,
) -> Result<Command, String> {
    if !(1..=64).contains(&format.channels) {
        return Err("Unsupported audio channel count".into());
    }
    let cfg = settings.phase_mode.biquad_config(&settings.config);
    let bands = cfg.parametric_bands()
        .iter()
        .filter(|band| band.frequency_hz < format.sample_rate / 2.0)
        .map(|band| band.state())
        .collect();
    let routing = (format.channels > 1 && cfg.gains() != (1.0, 1.0))
        .then(|| balance_matrix(format.channels, &cfg));
    Ok(Command::ApplyAdvancedPlayback {
        preamp_db: cfg.preamp_db,
        enabled: !cfg.bypass,
        bands,
        routing,
        convolution,
        effects: settings.effects(),
        output_effects: settings.playback_effects.output,
    })
}

fn route(client: &mut SyncClient, output: Option<&str>) -> Result<Option<String>, String> {
    match client.send_recv(match output {
        Some(name) => Command::SetOutputTarget { node_name: name.into() },
        None => Command::FollowSystemOutput,
    }).map_err(|e| e.to_string())? {
        Response::Ok => Ok(None),
        Response::Error(error) => Ok(Some(error)),
        _ => Err("Unexpected audio-engine reply".into()),
    }
}

// #######################################################
// keep the audio engine connected and in sync
// #######################################################

impl Engine {
    #[cfg(test)]
    pub(crate) fn idle_for_test() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared::default())),
            running: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    pub fn start(directory: &Path) -> Self {
        let ir_path = directory.join("live-eq.wav");
        let shared = Arc::new(Mutex::new(Shared { status: "Starting audio engine…".into(), ..Shared::default() }));
        let running = Arc::new(AtomicBool::new(true));
        let state = shared.clone();
        let alive = running.clone();
        let worker = thread::spawn(move || {
            let mut child: Option<Child> = None;
            let mut pinned_output: Option<String> = None;
            let mut next_launch = Instant::now();
            while alive.load(Ordering::Relaxed) {
                let mut client = match SyncClient::connect_with_timeout(Duration::from_millis(500)) {
                    Ok(c) => c,
                    Err(_) => {
                        let exited = child.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_some());
                        if exited {
                            child = None;
                            next_launch = Instant::now() + Duration::from_secs(2);
                        }
                        let message = if child.is_none() && Instant::now() >= next_launch {
                                match spawn_engine() {
                                    Ok(c) => { child = Some(c); "Starting bundled audio engine…" }
                                    Err(_) => "Bundled audio engine missing. Build/package resonanced beside the app."
                                }
                            } else if child.is_none() {
                                "Audio engine exited · retrying…"
                            } else { "Waiting for the audio engine…" };
                        let mut s = state.lock().unwrap();
                        s.state = None; s.status = message.into();
                        drop(s); thread::sleep(Duration::from_millis(750)); continue;
                    }
                };
                let session = (|| -> Result<(), String> {
                    // The app renders its buffered modes into one IR, including
                    // both the parametric and spline parts of the curve.
                    #[cfg(not(target_os = "linux"))]
                    {
                        request(&mut client, Command::ClearConvolutionIr)?;
                        request(&mut client, Command::SetDither { bits: None })?;
                        request(&mut client, Command::SetPhaseMode { linear: false })?;
                    }
                    let mut applied: Option<(AudioFormat, Arc<PlaybackSettings>)> = None;
                    let mut installed_filter: Option<phase::Filter> = None;
                    let mut last_filter_edit = None::<Instant>;
                    let mut route_pending = if cfg!(windows) { None } else { Some(pinned_output.clone()) };
                    let mut route_error = None;
                    let mut capture = crate::scope::Capture::default();
                    let mut analyzer = crate::spectrum::Analyzer::default();
                    let mut previous_resolution = None;
                    let mut next_analysis = Instant::now();
                    let mut next_limiter_analysis = next_analysis;
                    #[cfg(target_os = "linux")]
                    let mut next_limiter_capture = next_analysis;
                    let mut previous_monitor_mode = None;
                    let mut next_monitor_poll = Instant::now();
                    let mut next_poll = Instant::now();
                    loop {
                        if !alive.load(Ordering::Relaxed) { return Ok(()); }
                        let target = state.lock().unwrap().target.take();
                        if let Some(target) = target {
                            route_pending = Some(target);
                        }
                        if let Some(target) = route_pending.take() {
                            let response = route(&mut client, target.as_deref());
                            if response.is_err() {
                                let mut shared = state.lock().unwrap();
                                if shared.target.is_none() { shared.target = Some(target.clone()); }
                            }
                            route_error = response?;
                            if route_error.is_none() && !cfg!(windows) {
                                pinned_output = target;
                            }
                        }
                        let snapshot = client.get_state().map_err(|e| e.to_string())?;
                        let format = AudioFormat::from(&snapshot);
                        let (scope_requested, spectrum_requested, meter_capture_requested, monitor_mode, reset_peak_request,
                             prepared, limiter_overlay_requested) = {
                            let mut shared = state.lock().unwrap();
                            (shared.scope_requested, shared.spectrum_requested, shared.meter_capture_requested,
                             shared.monitor_mode, std::mem::take(&mut shared.reset_true_peak),
                             shared.prepared_for(&format), shared.limiter_overlay_requested)
                        };
                        let watch_limiter = prepared.as_ref().is_some_and(|settings| {
                            settings.playback_effects.output.limiter_enabled && !settings.config.bypass
                        });
                        let reduction = if watch_limiter {
                            match client.send_recv(Command::ReadLimiterReduction) {
                                Ok(Response::LimiterReduction(db)) => db,
                                _ => 0.0,
                            }
                        } else { 0.0 };
                        let limiter_overlay = watch_limiter && limiter_overlay_requested;
                        let stereo_requested = cfg!(windows) && monitor_mode == Some(crate::live::STEREO_FIELD_MODE);
                        let peak_requested = monitor_mode == Some(crate::live::TRUE_PEAK_MODE);
                        let peak_was_requested = previous_monitor_mode == Some(crate::live::TRUE_PEAK_MODE);
                        let reset_peak = reset_peak_request || !peak_was_requested;
                        let now = Instant::now();
                        let analyze_limiter = limiter_overlay && reduction >= 0.05 && now >= next_limiter_analysis;
                        if analyze_limiter {
                            next_limiter_analysis = now + Duration::from_millis(64);
                        }
                        let analyze = spectrum_requested != previous_resolution
                            || (spectrum_requested.is_some() && now >= next_analysis);
                        if analyze {
                            previous_resolution = spectrum_requested;
                            next_analysis = (next_analysis + crate::live::ANALYSIS_INTERVAL)
                                .max(now + crate::live::ANALYSIS_INTERVAL / 2);
                        }
                        let frames = if analyze && spectrum_requested.is_some() {
                            crate::scope::CAPTURE_FRAMES
                        } else if scope_requested {
                            crate::scope::waveform_capture_frames(snapshot.sample_rate)
                        } else {
                            0
                        };
                        let stereo_frames = if cfg!(windows) && analyze_limiter {
                            crate::scope::CAPTURE_FRAMES as usize
                        } else if stereo_requested {
                            crate::monitor::FIELD_FRAMES
                        } else { 0 };
                        let audio = capture.read(
                            &mut client, &snapshot,
                            frames > 0 || meter_capture_requested || stereo_requested
                                || (cfg!(windows) && (peak_requested || limiter_overlay)),
                            frames,
                            stereo_frames,
                        );
                        let waveform = audio.as_ref().filter(|_| scope_requested)
                            .map(|audio| Arc::from(crate::scope::window(&audio.samples, audio.rate)));
                        let spectrum = audio.as_ref().filter(|_| analyze).zip(spectrum_requested)
                            .map(|(audio, resolution)| analyzer.levels(&audio.samples, audio.rate, resolution)
                                .unwrap_or_else(|| crate::spectrum::Levels::silent(resolution)));
                        #[cfg(target_os = "linux")]
                        let limiter_capture = if limiter_overlay && (analyze_limiter || now >= next_limiter_capture) {
                            next_limiter_capture = now + Duration::from_millis(120);
                            // Keep recent stereo history available and fetch samples only on a hit.
                            let frames = if analyze_limiter { crate::scope::CAPTURE_FRAMES as u16 } else { 0 };
                            match client.send_recv(Command::CaptureMonitor { frames }) {
                                Ok(Response::MonitorCapture { rate, frames }) if f64::from(rate) == format.sample_rate => {
                                    Some((rate, frames))
                                }
                                _ => None,
                            }
                        } else { None };
                        let limiter_hit = if analyze_limiter {
                            let resolution = spectrum_requested.unwrap_or(crate::spectrum::Resolution::Standard);
                            #[cfg(target_os = "linux")]
                            let levels = limiter_capture.as_ref().and_then(|(rate, frames)| {
                                analyzer.stereo_levels(frames.iter().map(|&[_, _, left, right]| [left, right]),
                                    f64::from(*rate), resolution)
                            });
                            #[cfg(windows)]
                            let levels = audio.as_ref().and_then(|audio| {
                                analyzer.stereo_levels(audio.stereo.iter().copied(), audio.rate, resolution)
                            });
                            #[cfg(not(any(target_os = "linux", windows)))]
                            let levels = analyzer.levels(&[], format.sample_rate, resolution);
                            // A stopped stream can leave a gain-reduction reading behind.
                            // Only refresh the highlight when its audio history is current.
                            levels.map(|levels| LimiterHit {
                                time: now,
                                reduction_db: reduction,
                                regions: crate::spectrum::dominant_regions(levels.as_slice()),
                            })
                        } else { None };
                        // Read the peaks from PipeWire on Linux and WASAPI loopback on Windows.
                        #[cfg(target_os = "linux")]
                        let true_peaks = if peak_requested {
                            match client.send_recv(Command::ReadTruePeak { reset: reset_peak }) {
                                Ok(Response::TruePeak { peaks }) => peaks,
                                _ => None,
                            }
                        } else { None };
                        #[cfg(windows)]
                        let true_peaks = if peak_requested || peak_was_requested {
                            capture.true_peak(peak_requested, reset_peak)
                        } else { None };
                        #[cfg(not(any(target_os = "linux", windows)))]
                        let true_peaks: Option<[f32; 2]> = None;
                        let monitor_changed = monitor_mode != previous_monitor_mode;
                        let refresh_monitor = monitor_changed || (monitor_mode.is_some() && Instant::now() >= next_monitor_poll);
                        let monitor_signal = if refresh_monitor {
                            let period = if monitor_mode == Some(crate::live::STEREO_FIELD_MODE) {
                                crate::live::CAPTURE_INTERVAL
                            } else {
                                Duration::from_millis(32)
                            };
                            next_monitor_poll = if monitor_changed {
                                now + period
                            } else {
                                (next_monitor_poll + period).max(now + period / 2)
                            };
                            previous_monitor_mode = monitor_mode;
                            #[cfg(windows)]
                            { audio.as_ref().filter(|_| stereo_requested)
                                .map(|audio| {
                                    let start = audio.stereo.len().saturating_sub(crate::monitor::FIELD_FRAMES);
                                    crate::monitor::Signal::stereo(audio.stereo[start..].to_vec())
                                }) }
                            #[cfg(target_os = "linux")]
                            { monitor_mode.filter(|&mode| mode != crate::live::TRUE_PEAK_MODE).and_then(|mode| {
                                if snapshot.meters.in_peak.max(snapshot.meters.out_peak) <= 0.00001 {
                                    return None;
                                }
                                let frames = if mode == crate::live::BEFORE_AFTER_MODE { 8192 } else { crate::monitor::FIELD_FRAMES as u16 };
                                let Response::MonitorCapture { rate, frames } = client.send_recv(Command::CaptureMonitor { frames }).ok()? else {
                                    return None;
                                };
                                if frames.is_empty() {
                                    return None;
                                }
                                if mode == crate::live::BEFORE_AFTER_MODE {
                                    crate::monitor::Signal::comparison(&frames, rate, &mut analyzer)
                                } else {
                                    let samples = frames.iter().map(|&[_, _, l, r]| [l, r]).collect();
                                    Some(crate::monitor::Signal::stereo(samples))
                                }
                            }) }
                            #[cfg(not(any(target_os = "linux", windows)))]
                            { None::<crate::monitor::Signal> }
                        } else {
                            None
                        };
                        {
                            let mut s = state.lock().unwrap();
                            s.clipped |= snapshot.meters.clip || snapshot.meters.out_peak >= 1.0;
                            #[cfg(windows)]
                            { s.playback_levels = audio.as_ref().and_then(|audio| audio.levels); }
                            s.state = Some(snapshot);
                            s.scope_samples = waveform;
                            if analyze {
                                s.spectrum_levels = spectrum.map(Arc::new);
                            }
                            if refresh_monitor {
                                s.monitor_signal = monitor_signal.map(Arc::new);
                            }
                            if !peak_requested || reset_peak {
                                s.true_peaks = None;
                            }
                            // Keep the highest unread peaks in case the UI skips a refresh.
                            if let Some(peaks) = true_peaks.filter(|_| s.monitor_mode == Some(crate::live::TRUE_PEAK_MODE) && !s.reset_true_peak) {
                                let unread = s.true_peaks.get_or_insert([0.0; 2]);
                                for (peak, next) in unread.iter_mut().zip(peaks) {
                                    *peak = peak.max(next);
                                }
                            }
                            s.limiter_reduction_db = reduction;
                            if limiter_hit.is_some() && s.limiter_overlay_requested {
                                s.limiter_hit = limiter_hit;
                            }
                        }
                        let status = if let Some(settings) = prepared {
                            let previous = applied.as_ref().filter(|(on, _)| *on == format)
                                .map(|(_, settings)| settings);
                            if previous.is_none_or(|old| !Arc::ptr_eq(old, &settings)) {
                                settings.config.validate()?;
                                let rate = format.sample_rate.round() as u32;
                                let filter = phase::Filter::new(settings.phase_mode, &settings.config, rate);
                                let filter_changed = installed_filter != filter;
                                let same_mode_and_rate = installed_filter.as_ref().zip(filter.as_ref())
                                    .is_some_and(|(old, new)| old.mode == new.mode && old.sample_rate == new.sample_rate);
                                let should_wait = filter_changed && same_mode_and_rate
                                    && last_filter_edit.is_some_and(|last| last.elapsed() < Duration::from_millis(80));
                                if !should_wait {
                                    let path = filter.as_ref().map(|_| ir_path.to_string_lossy().into_owned());
                                    if let Some(filter) = filter.as_ref().filter(|_| filter_changed) {
                                        crate::storage::write_text(&ir_path, &filter.wav()?)
                                            .map_err(|error| format!("Could not save EQ filter: {error}"))?;
                                    }
                                    // Send the filter, gain, and routing together for a Linux playback transition.
                                    let commands = if cfg!(target_os = "linux") && (previous.is_none() || filter_changed) {
                                        vec![apply_playback(&settings, &format, path)?]
                                    } else {
                                        let convolution = filter_changed.then(|| match path {
                                            Some(path) => Command::SetConvolutionIr { path },
                                            None => Command::ClearConvolutionIr,
                                        });
                                        playback_plan(previous.map(Arc::as_ref), &settings, &format, convolution)?
                                    };
                                    for command in commands {
                                        if !alive.load(Ordering::Relaxed) { return Ok(()); }
                                        request(&mut client, command)?;
                                    }
                                    if filter_changed {
                                        installed_filter = filter;
                                        last_filter_edit = Some(Instant::now());
                                    }
                                    applied = Some((format, settings.clone()));
                                }
                            }
                            if settings.config.bypass { "Bypassed · original audio" } else { "Engine connected · settings sent" }
                        } else {
                            // The UI selects the linked preset and calculates headroom
                            // for this format before we send any settings for it.
                            applied = None;
                            "Preparing settings for output…"
                        };
                        state.lock().unwrap().status = route_error.as_ref()
                            .map_or_else(|| status.into(), |error| format!("Output switch: {error}"));
                        // Keep the cadence across small timer delays and skip missed ticks.
                        let now = Instant::now();
                        let interval = if scope_requested || meter_capture_requested || monitor_mode == Some(crate::live::STEREO_FIELD_MODE) {
                            crate::live::CAPTURE_INTERVAL
                        } else {
                            crate::live::CONTROL_INTERVAL
                        };
                        next_poll = (next_poll + interval).max(now);
                        thread::sleep(next_poll.duration_since(now));
                    }
                })();
                if let Err(e) = session {
                    let mut s = state.lock().unwrap(); s.state = None; s.scope_samples = None;
                    s.spectrum_levels = None;
                    s.monitor_signal = None;
                    s.true_peaks = None;
                    s.limiter_hit = None;
                    s.status = format!("Audio connection: {e}");
                    drop(s); thread::sleep(Duration::from_millis(750));
                }
            }
            // Restore passthrough before dropping our own engine. Never kill a
            // process that was already running before this app.
            if let Some(mut process) = child {
                if process.try_wait().ok() == Some(None) {
                    if let Ok(mut c) = SyncClient::connect_with_timeout(Duration::from_millis(500)) {
                        let _ = request(&mut c, Command::SetPower { enabled: false });
                        let _ = request(&mut c, Command::ClearConvolutionIr);
                        let _ = request(&mut c, Command::ClearRouting);
                        let _ = request(&mut c, Command::Shutdown);
                    }
                    let deadline = Instant::now() + Duration::from_millis(500);
                    while process.try_wait().ok() == Some(None) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(20));
                    }
                    let _ = process.kill();
                    let _ = process.wait();
                }
            }
            let _ = std::fs::remove_file(ir_path);
        });
        Self { shared, running, worker: Some(worker) }
    }

    pub fn submit(&self, format: &AudioFormat, config: Config, phase_mode: phase::Mode, playback_effects: PlaybackEffects) {
        let mut shared = self.shared.lock().unwrap();
        shared.prepared = Some((format.clone(), Arc::new(PlaybackSettings { config, phase_mode, playback_effects })));
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}

// #######################################################
// start the bundled audio daemon
// #######################################################

fn spawn_engine() -> std::io::Result<Child> {
    let dir: PathBuf = std::env::current_exe()?.parent().unwrap().into();
    let name = if cfg!(windows) { "resonanced.exe" } else { "resonanced" };
    let mut cmd = Process::new(dir.join(name));
    cmd.env("RESONANCE_MANAGED_CHILD", "1")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn balance_keeps_other_channels_and_bypass_restores_identity() {
        let mut cfg = Config::default(); cfg.balance = 0.5;
        let m = balance_matrix(6, &cfg);
        assert_eq!(m.gains[0], 0.5); assert_eq!(m.gains[7], 1.0);
        assert_eq!(m.gains[14], 1.0); assert_eq!(m.gains[1], 0.0);
        cfg.bypass = true; assert_eq!(balance_matrix(2, &cfg).gains, vec![1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn switching_to_spline_keeps_the_live_filters_and_headroom() {
        let mut before = Config::default();
        before.bands[2].gain_db = 9.0;
        before.bands[2].q = 18.0;
        let mut after = before.clone();
        after.convert_to_spline(48000.0);
        let after = Config::from_json(&serde_json::to_string(&after).unwrap()).unwrap();
        let (before, _) = crate::listening::prepare(&before, None, true, 48000.0, 2, phase::Mode::Minimum);
        let (mut after, _) = crate::listening::prepare(&after, None, true, 48000.0, 2, phase::Mode::Minimum);
        assert!(update_plan(Some(&before), &after, 2, 48000.0).unwrap().is_empty());
        assert!(phase::Filter::new(phase::Mode::Minimum, &after, 48000).is_none());
        after.bands[3].gain_db -= 2.0;
        let (after, _) = crate::listening::prepare(&after, None, true, 48000.0, 2, phase::Mode::Minimum);
        let mut parametric = after.clone();
        parametric.convert_to_parametric();
        let (parametric, _) = crate::listening::prepare(&parametric, None, true, 48000.0, 2, phase::Mode::Minimum);
        assert!(!parametric.spline);
        let spline = phase::Filter::new(phase::Mode::Minimum, &after, 48000);
        assert!(spline.is_some());
        assert!(phase::Filter::new(phase::Mode::Minimum, &parametric, 48000) == spline);
        assert_eq!(parametric.preamp_db, after.preamp_db);
        assert!(update_plan(Some(&after), &parametric, 2, 48000.0).unwrap().is_empty());
    }

    #[test]
    fn live_band_edits_do_not_toggle_power_or_replace_the_chain() {
        let before = Config::default();
        let mut after = before.clone();
        after.bands[2].gain_db = 3.0;
        after.bands[2].frequency_hz = 950.0;
        let commands = update_plan(Some(&before), &after, 2, 48000.0).unwrap();
        assert!(matches!(commands.as_slice(), [Command::SetBand { index: 2, freq: 950.0, gain_db: 3.0, .. }]));
    }

    #[test]
    fn headroom_precedes_boost_and_bypass_clears_routing_before_reenable() {
        let mut before = Config::default();
        before.bypass = true;
        let mut after = before.clone();
        after.bypass = false;
        after.balance = 0.4;
        after.preamp_db = -5.0;
        after.bands[0].gain_db = 4.0;
        let commands = update_plan(Some(&before), &after, 2, 48000.0).unwrap();
        assert!(matches!(commands.as_slice(), [
            Command::SetPreamp { db: -5.0 }, Command::SetBand { index: 0, .. },
            Command::SetChannelRouting { .. }, Command::SetPower { enabled: true }
        ]));
        let commands = update_plan(Some(&after), &before, 2, 48000.0).unwrap();
        assert!(matches!(commands.first(), Some(Command::SetPower { enabled: false })));
        assert!(matches!(commands.last(), Some(Command::ClearRouting)));
    }

    #[test]
    fn full_sync_handles_mono_and_rebuilds_at_nyquist() {
        let mut cfg = Config::default();
        cfg.balance = 1.0;
        let commands = update_plan(None, &cfg, 1, 48000.0).unwrap();
        assert!(matches!(commands.as_slice(), [Command::ApplyState { enabled: true, .. }, Command::ClearRouting]));
        let mut moved = cfg.clone();
        moved.bands.last_mut().unwrap().frequency_hz = 4000.0;
        let commands = update_plan(Some(&cfg), &moved, 2, 16000.0).unwrap();
        assert!(matches!(commands.first(), Some(Command::ApplyState { .. })));
    }

    #[test]
    fn indices_for_incremental_edits_skip_out_of_range_bands() {
        let mut before = Config::default();
        before.bands[0].frequency_hz = 12000.0;
        let mut after = before.clone();
        after.bands[1].gain_db = 3.0;
        after.bands[0].gain_db = 7.0;
        let commands = update_plan(Some(&before), &after, 2, 16000.0).unwrap();
        assert!(matches!(commands.as_slice(), [Command::SetBand { index: 0, gain_db: 3.0, .. }]));
        let commands = update_plan(None, &before, 2, 16000.0).unwrap();
        let Command::ApplyState { bands, .. } = &commands[0] else { panic!("expected full sync"); };
        assert_eq!(bands[0].freq, before.bands[1].frequency_hz);
        assert!(bands.iter().all(|band| band.freq < 8000.0));
    }

    #[test]
    fn a_prepared_preset_is_scoped_to_the_output_and_audio_format() {
        let old = AudioFormat { channels: 2, sample_rate: 48000.0, output: Some("USB DAC".into()) };
        let mut state = Shared {
            prepared: Some((old.clone(), Arc::new(PlaybackSettings::default()))),
            ..Shared::default()
        };
        let new = AudioFormat { output: Some("Speakers".into()), ..old.clone() };
        assert!(state.prepared_for(&new).is_none());
        let other_rate = AudioFormat { sample_rate: 96000.0, ..new.clone() };
        assert!(state.prepared_for(&other_rate).is_none());
        let new_cfg = Arc::new(PlaybackSettings {
            config: Config { preamp_db: -6.0, ..Config::default() }, phase_mode: phase::Mode::Zero,
            ..PlaybackSettings::default()
        });
        state.prepared = Some((new.clone(), new_cfg.clone()));
        assert!(Arc::ptr_eq(&state.prepared_for(&new).unwrap(), &new_cfg));
        assert!(state.prepared_for(&old).is_none());
    }

    #[test]
    fn phase_switch_installs_only_the_selected_filter_before_enabling_eq() {
        let format = AudioFormat { channels: 2, sample_rate: 48000.0, output: None };
        let mut cfg = Config::default();
        cfg.bands[0].gain_db = 6.0;
        let minimum = PlaybackSettings { config: cfg.clone(), ..PlaybackSettings::default() };
        let matched = PlaybackSettings { config: cfg.clone(), phase_mode: phase::Mode::Matched, ..PlaybackSettings::default() };
        let zero = PlaybackSettings { config: cfg.clone(), phase_mode: phase::Mode::Zero, ..PlaybackSettings::default() };
        let load = Command::SetConvolutionIr { path: "live-eq.wav".into() };
        let matched_commands = playback_plan(Some(&minimum), &matched, &format, Some(load.clone())).unwrap();
        assert!(matches!(matched_commands.as_slice(), [Command::SetConvolutionIr { .. }]));
        let commands = playback_plan(Some(&minimum), &zero, &format, Some(load)).unwrap();
        assert!(matches!(commands.as_slice(), [
            Command::ApplyStateWithOutputEffects { enabled: false, bands, .. }, Command::ClearRouting,
            Command::SetConvolutionIr { .. }, Command::SetPower { enabled: true },
        ] if bands.is_empty()));

        let commands = playback_plan(Some(&zero), &minimum, &format, Some(Command::ClearConvolutionIr)).unwrap();
        assert!(matches!(commands.as_slice(), [
            Command::ApplyStateWithOutputEffects { enabled: false, bands, .. }, Command::ClearRouting,
            Command::ClearConvolutionIr, Command::SetPower { enabled: true },
        ] if bands.len() == cfg.bands.len()));

        cfg.bypass = true;
        let bypass = PlaybackSettings { config: cfg, phase_mode: phase::Mode::Zero, ..PlaybackSettings::default() };
        assert!(matches!(playback_plan(Some(&zero), &bypass, &format, Some(Command::ClearConvolutionIr)).unwrap().as_slice(),
            [Command::SetPower { enabled: false }, Command::ClearConvolutionIr]));
    }

    #[test]
    fn advanced_effects_survive_full_updates_and_change_without_replacing_the_filter() {
        let format = AudioFormat { channels: 2, sample_rate: 48_000.0, output: Some("Headphones".into()) };
        let off = PlaybackSettings::default();
        let output = OutputEffects { room_amount: 0.44, limiter_enabled: true };
        let high = PlaybackSettings {
            playback_effects: PlaybackEffects { crossfeed: Crossfeed::new(100), output },
            ..PlaybackSettings::default()
        };
        let load = apply_playback(&high, &format, Some("spline.wav".into())).unwrap();
        assert!(matches!(load, Command::ApplyAdvancedPlayback { effects, output_effects, convolution: Some(_), .. }
            if output_effects == output && effects.get(FxEffectId::Crossfeed) == (1.0, true)));
        let full = playback_plan(None, &high, &format, None).unwrap();
        assert!(matches!(&full[0], Command::ApplyStateWithOutputEffects { effects, output_effects, .. }
            if *output_effects == output && effects.get(FxEffectId::Crossfeed) == (1.0, true)));
        let live = playback_plan(Some(&off), &high, &format, None).unwrap();
        assert!(matches!(live.as_slice(), [
            Command::SetEffectIntensity { effect: FxEffectId::Crossfeed, value: 1.0 },
            Command::SetEffectEnabled { effect: FxEffectId::Crossfeed, enabled: true },
            Command::SetOutputEffects(installed),
        ] if *installed == output));
        let level = PlaybackSettings {
            playback_effects: PlaybackEffects { crossfeed: Crossfeed::new(37), output },
            ..PlaybackSettings::default()
        };
        let adjusted = playback_plan(Some(&high), &level, &format, None).unwrap();
        assert!(matches!(adjusted.as_slice(), [
            Command::SetEffectIntensity { effect: FxEffectId::Crossfeed, value: 0.37 },
        ]));
    }

    #[test]
    fn display_only_changes_do_not_touch_audio() {
        let before = Config::default();
        let mut after = before.clone();
        after.show_live_view = !before.show_live_view;
        after.name = "Renamed".into();
        assert!(update_plan(Some(&before), &after, 2, 48000.0).unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "run with SIMPLEDARKEQ_TEST_ENGINE pointing to a compiled Windows daemon"]
    fn windows_daemon_accepts_full_sync_and_incremental_edits() {
        use resonance_ipc::transport::{read_response, write_command};
        use std::{fs, net::TcpStream, path::Path, process::Stdio, time::SystemTime};

        struct TestDaemon {
            child: Child,
            directory: PathBuf,
        }
        impl Drop for TestDaemon {
            fn drop(&mut self) {
                if self.child.try_wait().ok() == Some(None) {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                }
                let expected_parent = std::env::temp_dir().canonicalize().ok();
                if let Ok(actual) = self.directory.canonicalize() {
                    if actual.parent() == expected_parent.as_deref() &&
                        actual.file_name().is_some_and(|name| name.to_string_lossy().starts_with("simpledarkeq-daemon-test-")) {
                        let _ = std::fs::remove_dir_all(actual);
                    }
                }
            }
        }

        fn exchange(stream: &mut TcpStream, command: Command) -> Response {
            write_command(stream, &command).unwrap();
            read_response(stream).unwrap()
        }
        let exe = PathBuf::from(std::env::var_os("SIMPLEDARKEQ_TEST_ENGINE").expect("set SIMPLEDARKEQ_TEST_ENGINE"));
        let nonce = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("simpledarkeq-daemon-test-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let port_file = directory.join("engine.port");
        let child = Process::new(exe)
            .current_dir(&directory)
            .env("RESONANCE_SOCKET", &port_file)
            .env("XDG_CONFIG_HOME", &directory)
            .env("XDG_DATA_HOME", &directory)
            .env("XDG_RUNTIME_DIR", &directory)
            .env("ProgramData", &directory)
            .env("RESONANCE_MANAGED_CHILD", "1")
            .env("RESONANCE_NO_AUTO_ATTACH", "1")
            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().expect("start isolated daemon");
        let mut daemon = TestDaemon { child, directory };
        fn connect(port_file: &Path) -> Option<TcpStream> {
            let port = fs::read_to_string(port_file).ok()?.trim().parse::<u16>().ok()?;
            TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200)).ok()
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            if let Some(stream) = connect(&port_file) { break stream; }
            assert!(Instant::now() < deadline, "isolated daemon did not start");
            thread::sleep(Duration::from_millis(20));
        };
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();

        let Response::State(initial) = exchange(&mut stream, Command::GetState) else { panic!("missing initial state"); };
        assert!(matches!(exchange(&mut stream, Command::SetOutputTarget { node_name: "missing SimpleDarkEQ test device".into() }), Response::Error(_)));
        let Response::State(unchanged_output) = exchange(&mut stream, Command::GetState) else { panic!("missing output state"); };
        assert_eq!(unchanged_output.preferred_output, initial.preferred_output);
        let mut cfg = Config::default();
        cfg.preamp_db = -5.0;
        cfg.balance = 0.25;
        let apply = |stream: &mut TcpStream, previous: Option<&Config>, config: &Config| {
            for command in update_plan(previous, config, initial.channels, initial.sample_rate).unwrap() {
                assert!(matches!(exchange(stream, command), Response::Ok));
            }
        };
        apply(&mut stream, None, &cfg);
        let mut edited = cfg.clone();
        edited.bands[1].frequency_hz = 240.0;
        edited.bands[1].gain_db = 4.0;
        edited.bands[2].enabled = false;
        apply(&mut stream, Some(&cfg), &edited);
        let Response::State(final_state) = exchange(&mut stream, Command::GetState) else { panic!("missing updated state"); };
        assert!(final_state.enabled);
        assert_eq!(final_state.preamp_db, -5.0);
        assert_eq!(final_state.bands[1].freq, 240.0);
        assert_eq!(final_state.bands[1].gain_db, 4.0);
        assert!(final_state.bands[1].enabled);
        assert!(!final_state.bands[2].enabled);
        assert_eq!(final_state.routing.as_ref().unwrap().gains[0], 0.75);

        let tones = crate::simple::SimpleSettings { bass: 6.0, ..Default::default() }
            .config(false, true, edited.balance);
        apply(&mut stream, Some(&edited), &tones);
        let Response::State(simple) = exchange(&mut stream, Command::GetState) else { panic!("missing Simple state"); };
        assert_eq!(simple.bands, tones.bands.iter().map(crate::config::Band::state).collect::<Vec<_>>());
        assert_eq!(simple.routing, final_state.routing);

        apply(&mut stream, Some(&tones), &edited);
        let Response::State(restored) = exchange(&mut stream, Command::GetState) else { panic!("missing restored Standard state"); };
        assert!(restored.enabled);
        assert_eq!(restored.bands, final_state.bands);
        assert_eq!(restored.preamp_db, final_state.preamp_db);
        assert_eq!(restored.routing, final_state.routing);

        let mut spline = Config::flat_spline();
        spline.add_peak(240.0, 5.0);
        apply(&mut stream, Some(&edited), &spline);
        let kernel = daemon.directory.join("test-spline.wav");
        fs::write(&kernel, crate::spline::kernel_wav(&spline.bands, initial.sample_rate.round() as u32)).unwrap();
        assert!(matches!(exchange(&mut stream, Command::SetConvolutionIr {
            path: kernel.to_string_lossy().into_owned(),
        }), Response::Ok));
        let Response::State(splined) = exchange(&mut stream, Command::GetState) else { panic!("missing spline state"); };
        assert!(splined.bands.is_empty());
        assert!(splined.convolution.unwrap().enabled);
        assert!(matches!(exchange(&mut stream, Command::ClearConvolutionIr), Response::Ok));
        let Response::State(cleared) = exchange(&mut stream, Command::GetState) else { panic!("missing cleared spline state"); };
        assert!(cleared.convolution.is_none());
        drop(daemon.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(exit) = daemon.child.try_wait().unwrap() {
                assert!(exit.success());
                break;
            }
            assert!(Instant::now() < deadline, "managed daemon did not exit after its parent pipe closed");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!port_file.exists(), "managed daemon left its IPC port file behind");
    }
}
