use crate::config::{Band, Config};
use resonance_ipc::{Command, DaemonState, EffectsState, Response, RoutingMatrix, transport::SyncClient};
use std::{path::{Path, PathBuf}, process::{Child, Command as Process}, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, thread, time::{Duration, Instant}};

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
    prepared: Option<(AudioFormat, Config)>,
    pub target: Option<Option<String>>,
    pub state: Option<DaemonState>,
    pub status: String,
    pub clipped: bool,
    pub scope_requested: bool,
    pub scope_samples: Option<Vec<f32>>,
    pub spectrum_requested: Option<crate::spectrum::Resolution>,
    pub spectrum_levels: Option<crate::spectrum::Levels>,
}

impl Shared {
    fn prepared_for(&self, format: &AudioFormat) -> Option<Config> {
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
    let Some(old) = previous.filter(|old| {
        if old.spline != cfg.spline {
            return false;
        }
        cfg.spline || (old.bands.len() == cfg.bands.len() && old.bands.iter().zip(&cfg.bands).all(|(a, b)| {
            // A preset may omit a band above Nyquist. Do not assume its
            // indices still identify the other bands in that situation.
            supported(a.frequency_hz) == supported(b.frequency_hz)
                && (!supported(a.frequency_hz) || a.kind == b.kind)
        }))
    }) else {
        return Ok(vec![
            Command::ApplyState {
                preamp_db: cfg.preamp_db,
                enabled: !cfg.bypass,
                bands: cfg.bands.iter().filter(|band| !cfg.spline && supported(band.frequency_hz)).map(|band| band.state()).collect(),
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
    for (index, (before, after)) in old.bands.iter().zip(&cfg.bands)
        .filter(|(_, band)| !cfg.spline && supported(band.frequency_hz)).enumerate()
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

fn active_spline(config: &Config) -> Option<&[Band]> {
    (config.spline && !config.bypass && config.bands.iter().any(|band| band.gain_db != 0.0))
        .then_some(config.bands.as_slice())
}

fn update_spline(
    client: &mut SyncClient,
    path: &Path,
    points: Option<&[Band]>,
    sample_rate: u32,
) -> Result<(), String> {
    if let Some(points) = points {
        crate::storage::write_text(path, &crate::spline::kernel_wav(points, sample_rate))
            .map_err(|error| format!("Could not save spline filter: {error}"))?;
        request(client, Command::SetConvolutionIr { path: path.to_string_lossy().into_owned() })
    } else {
        request(client, Command::ClearConvolutionIr)
    }
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
        let spline_path = directory.join("live-spline.wav");
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
                    // This UI deliberately exposes only EQ, preamp and balance.
                    request(&mut client, Command::ClearConvolutionIr)?;
                    request(&mut client, Command::SetDither { bits: None })?;
                    request(&mut client, Command::SetPhaseMode { linear: false })?;
                    let mut applied: Option<(AudioFormat, Config)> = None;
                    let mut installed_spline: Option<(u32, Vec<Band>)> = None;
                    let mut last_spline_edit = None::<Instant>;
                    let mut route_pending = if cfg!(windows) { None } else { Some(pinned_output.clone()) };
                    let mut route_error = None;
                    let mut capture = crate::scope::Capture::default();
                    let mut analyzer = crate::spectrum::Analyzer::default();
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
                        let (scope_requested, spectrum_requested) = {
                            let shared = state.lock().unwrap();
                            (shared.scope_requested, shared.spectrum_requested)
                        };
                        let audio = capture.read(&mut client, &snapshot, scope_requested || spectrum_requested.is_some());
                        let waveform = audio.as_ref().filter(|_| scope_requested)
                            .map(|audio| crate::scope::window(&audio.samples, audio.rate));
                        let spectrum = audio.as_ref().zip(spectrum_requested)
                            .and_then(|(audio, resolution)| analyzer.levels(&audio.samples, audio.rate, resolution));
                        let format = AudioFormat::from(&snapshot);
                        let prepared = {
                            let mut s = state.lock().unwrap();
                            s.clipped |= snapshot.meters.clip || snapshot.meters.out_peak >= 1.0;
                            s.state = Some(snapshot);
                            s.scope_samples = waveform;
                            s.spectrum_levels = spectrum;
                            s.prepared_for(&format)
                        };
                        let status = if let Some(cfg) = prepared {
                            let previous = applied.as_ref().filter(|(on, _)| *on == format).map(|(_, cfg)| cfg);
                            let rate = format.sample_rate.round() as u32;
                            let new_spline = active_spline(&cfg).map(|points| (rate, points));
                            let old_spline = installed_spline.as_ref().map(|(rate, points)| (*rate, points.as_slice()));
                            let spline_changed = old_spline != new_spline;
                            let should_wait = spline_changed && old_spline.zip(new_spline)
                                .is_some_and(|(old, new)| old.0 == new.0)
                                && last_spline_edit.is_some_and(|last| last.elapsed() < Duration::from_millis(80));
                            if (previous != Some(&cfg) || spline_changed) && !should_wait {
                                if spline_changed && new_spline.is_none() {
                                    update_spline(&mut client, &spline_path, None, rate)?;
                                }
                                for command in update_plan(previous, &cfg, format.channels, format.sample_rate)? {
                                    if !alive.load(Ordering::Relaxed) { return Ok(()); }
                                    request(&mut client, command)?;
                                }
                                if spline_changed {
                                    if let Some((rate, points)) = new_spline {
                                        update_spline(&mut client, &spline_path, Some(points), rate)?;
                                    }
                                    installed_spline = new_spline.map(|(rate, points)| (rate, points.to_vec()));
                                    last_spline_edit = Some(Instant::now());
                                }
                                applied = Some((format, cfg.clone()));
                            }
                            if cfg.bypass { "Bypassed · original audio" } else { "Engine connected · settings sent" }
                        } else {
                            // The UI selects the linked preset and calculates headroom
                            // for this format before we send any settings for it.
                            applied = None;
                            "Preparing settings for output…"
                        };
                        state.lock().unwrap().status = route_error.as_ref().map_or_else(|| status.into(), |error| format!("Output switch: {error}"));
                        // Keep the cadence across small timer delays and skip missed ticks.
                        let now = Instant::now();
                        let interval = if scope_requested || spectrum_requested.is_some() {
                            crate::live::FRAME_INTERVAL
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
            let _ = std::fs::remove_file(spline_path);
        });
        Self { shared, running, worker: Some(worker) }
    }

    pub fn submit(&self, format: &AudioFormat, cfg: Config) {
        let mut shared = self.shared.lock().unwrap();
        shared.prepared = Some((format.clone(), cfg));
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}

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
            prepared: Some((old.clone(), Config::default())),
            ..Shared::default()
        };
        let new = AudioFormat { output: Some("Speakers".into()), ..old.clone() };
        assert!(state.prepared_for(&new).is_none());
        let other_rate = AudioFormat { sample_rate: 96000.0, ..new.clone() };
        assert!(state.prepared_for(&other_rate).is_none());
        let new_cfg = Config { preamp_db: -6.0, ..Config::default() };
        state.prepared = Some((new.clone(), new_cfg.clone()));
        assert_eq!(state.prepared_for(&new), Some(new_cfg));
        assert!(state.prepared_for(&old).is_none());
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

        let tones = crate::simple::SimpleSettings { bass: 6.0, ..Default::default() }.config(false, true);
        apply(&mut stream, Some(&edited), &tones);
        let Response::State(simple) = exchange(&mut stream, Command::GetState) else { panic!("missing Simple state"); };
        assert_eq!(simple.bands, tones.bands.iter().map(crate::config::Band::state).collect::<Vec<_>>());
        assert!(simple.routing.is_none());

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
