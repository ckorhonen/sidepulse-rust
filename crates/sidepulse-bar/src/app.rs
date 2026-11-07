//! Main-thread app state: port of `StatusBarController` in `status_bar.py` (minus the Setup and
//! Settings windows and the Screen Bar, which stay in the Python CLI for now).

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use block2::RcBlock;
use chrono::Utc;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_foundation::NSTimer;
use serde_json::Value;

use sidepulse_core::battery::{self, BatterySnapshot, PowerChangePreview};
use sidepulse_core::devices::{discover, discover_in, Device};
use sidepulse_core::lid::{self, ClosedLidAwake};
use sidepulse_core::models::{AgentMode, AgentStatus};
use sidepulse_core::monitor::{LiveMonitor, MonitorConfig, Snapshot};
use sidepulse_core::power::KeepAwake;
use sidepulse_core::providers::default_state_dir;

use crate::led::{DeviceConfig, LedRequest, LedWorker, Message};
use crate::session::{
    applescript_quote, format_age, menu_title, recent_statuses, resolve_open, OpenTarget,
};
use crate::settings::Settings;
use crate::ui::{self, Action, DeviceRow, MenuModel, SessionRow, StatusUi};

/// Lines of each provider hook log replayed at startup (`STATUS_BAR_STARTUP_REPLAY_LINES`).
pub const STARTUP_REPLAY_LINES: usize = 200;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` against the app on the main thread (no-op before startup or after quit).
pub fn with_app(f: impl FnOnce(&mut App)) {
    APP.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            if let Some(app) = guard.as_mut() {
                f(app);
            }
        }
    });
}

pub fn install(app: App) {
    APP.with(|cell| *cell.borrow_mut() = Some(app));
}

pub struct App {
    mtm: MainThreadMarker,
    monitor: LiveMonitor,
    settings: Settings,
    ui: StatusUi,
    leds: LedWorker,
    keep_awake: KeepAwake,
    closed_lid: ClosedLidAwake,
    preview: PowerChangePreview,
    snapshot: Option<Snapshot>,
    devices: Vec<Device>,
    device_signature: Vec<(PathBuf, u64, u64)>,
    lid_closed: Option<bool>,
    refresh_pending: bool,
    flush_pending: bool,
    /// One-shot timer for the next time-based refresh (see [`App::schedule_next_refresh`]).
    refresh_timer: Option<Retained<NSTimer>>,
    keep_awake_detail: String,
}

/// Slack added after a predicted transition, because the monitor's thresholds are strict.
const TRANSITION_MARGIN: f64 = 0.25;
/// Timer tolerance for a predicted transition: the menu may change up to a second late.
const TRANSITION_TOLERANCE: f64 = 1.0;
/// Safety refresh when nothing is predicted (battery details IOKit may not notify about, and
/// any clock change). Coarse, with a large tolerance so the system can batch it.
const SAFETY_REFRESH: f64 = 300.0;
const SAFETY_TOLERANCE: f64 = 60.0;
/// `latest.json` is written at most this long after the first change of a burst.
const FLUSH_DELAY: f64 = 2.0;

/// Devices under `$SIDEPULSE_VOLUMES` when set (used for side-by-side testing), else `/Volumes`.
fn discover_devices() -> Vec<Device> {
    match std::env::var_os("SIDEPULSE_VOLUMES") {
        Some(root) => discover_in(std::path::Path::new(&root)),
        None => discover(),
    }
}

fn device_signature(devices: &[Device]) -> Vec<(PathBuf, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    devices
        .iter()
        .map(|d| {
            let (dev, ino) = std::fs::metadata(&d.root)
                .map(|m| (m.dev(), m.ino()))
                .unwrap_or((0, 0));
            (d.root.clone(), dev, ino)
        })
        .collect()
}

impl App {
    pub fn new(mtm: MainThreadMarker) -> Self {
        let state_dir = default_state_dir();
        let mut monitor = LiveMonitor::new(state_dir.join("latest.json"), MonitorConfig::default());
        let replayed = monitor.replay_logs(STARTUP_REPLAY_LINES);
        crate::log(&format!(
            "started; replayed {replayed} hook records, tracking {} sessions",
            monitor.len()
        ));
        let handler: Box<dyn Fn(Action)> = Box::new(|action| with_app(|app| app.handle(action)));
        let devices = discover_devices();
        Self {
            mtm,
            monitor,
            settings: Settings::load_default(),
            ui: StatusUi::new(mtm, handler),
            leds: LedWorker::start(crate::led::dry_run_from_env()),
            keep_awake: KeepAwake::default(),
            closed_lid: ClosedLidAwake::default(),
            preview: PowerChangePreview::default(),
            snapshot: None,
            device_signature: device_signature(&devices),
            devices,
            lid_closed: None,
            refresh_pending: false,
            flush_pending: false,
            refresh_timer: None,
            keep_awake_detail: String::new(),
        }
    }

    /// Hook event from the socket (`handle_hook_event_message`).
    pub fn on_event(&mut self, provider: &str, line: &Value) {
        if self.monitor.ingest_line(provider, line) && !self.refresh_pending {
            // Coalesce bursts: refresh once after the queued events are processed.
            self.refresh_pending = true;
            dispatch2::DispatchQueue::main().exec_async(|| with_app(|app| app.refresh()));
        }
        self.schedule_flush();
    }

    /// Debounced `latest.json` write: one flush [`FLUSH_DELAY`] after state first became dirty,
    /// replacing the 2 s flush timer. Shutdown flushes anything still pending.
    pub fn schedule_flush(&mut self) {
        if self.flush_pending || !self.monitor.is_dirty() {
            return;
        }
        self.flush_pending = true;
        dispatch2::DispatchQueue::main()
            .after(dispatch_time(FLUSH_DELAY), || {
                with_app(|app| {
                    app.flush_pending = false;
                    app.flush_state();
                    // Still dirty only if the write failed: retry like the old 2 s timer did.
                    app.schedule_flush();
                })
            })
            .ok();
    }

    /// Arms a single one-shot timer for the next time-based change instead of refreshing every
    /// 15 s: the next agent status transition (completed hiding, post-tool working turning
    /// completed, staleness), the end of the keep-awake grace period, or else a coarse safety
    /// refresh. Events, power-source and device notifications refresh on their own.
    fn schedule_next_refresh(&mut self, now: Instant) {
        let mut wait = SAFETY_REFRESH;
        let mut tolerance = SAFETY_TOLERANCE;
        let utc_now = Utc::now();
        let transition = self
            .monitor
            .next_transition_at(utc_now)
            .map(|at| (at - utc_now).num_milliseconds() as f64 / 1000.0);
        let grace = self
            .keep_awake
            .grace_deadline(now)
            .map(|at| (at - now).as_secs_f64());
        for secs in transition.into_iter().chain(grace) {
            let secs = secs.max(0.0) + TRANSITION_MARGIN;
            if secs < wait {
                wait = secs;
                tolerance = TRANSITION_TOLERANCE;
            }
        }
        if let Some(old) = self.refresh_timer.take() {
            old.invalidate();
        }
        let block = RcBlock::new(|_timer: std::ptr::NonNull<NSTimer>| {
            with_app(|app| {
                app.refresh_timer = None;
                app.refresh();
            })
        });
        // SAFETY: called on the main thread (the app lives there); the block is copied by the
        // timer, and a non-repeating timer is scheduled on the current (main) run loop.
        let timer =
            unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(wait, false, &block) };
        timer.setTolerance(tolerance);
        self.refresh_timer = Some(timer);
    }

    /// `refresh_`: snapshot, status item, LEDs, keep-awake, menu.
    pub fn refresh(&mut self) {
        self.refresh_pending = false;
        let now = Instant::now();
        let snapshot = self.monitor.snapshot(Utc::now());
        let mode = snapshot.aggregate.mode;
        self.ui.set_mode(mode);

        let battery = battery::read_with(self.settings.full_charge_watts());
        if let Some(b) = &battery {
            if self.preview.observe(
                b.is_plugged,
                self.settings.show_on_power_change(),
                Duration::from_secs_f64(self.settings.power_change_preview_seconds()),
                now,
            ) {
                crate::log("power source changed; previewing battery on LEDs");
                // Return to the agent display when the preview ends.
                let secs = self.settings.power_change_preview_seconds() + 0.1;
                dispatch2::DispatchQueue::main()
                    .after(dispatch_time(secs), || with_app(|app| app.refresh()))
                    .ok();
            }
        }
        self.sync_leds(mode, battery.as_ref(), now);

        self.keep_awake.update(mode, now);
        // Log transitions only (refreshes are now per event, so logging every held refresh
        // would repeat the same line). The grace countdown is not a transition.
        let detail = self.keep_awake.detail(now);
        let kind = detail.split(':').next().unwrap_or_default().to_string();
        if kind != self.keep_awake_detail {
            crate::log(&format!("keep awake: {detail}"));
            self.keep_awake_detail = kind;
        }
        let policy = lid::ClosedLidPolicy::parse(self.settings.closed_lid_policy().as_str())
            .unwrap_or(lid::ClosedLidPolicy::Never);
        // `update` reports whether the hold is active, not whether it changed: log transitions only.
        let was_active = self.closed_lid.active();
        let active = self
            .closed_lid
            .update(policy, self.keep_awake.holding_requested);
        if active != was_active {
            crate::log(&format!(
                "closed-lid awake: {}",
                if active { "holding" } else { "released" }
            ));
        }

        let model = self.menu_model(&snapshot);
        self.ui.set_menu(&model);
        self.snapshot = Some(snapshot);
        self.schedule_next_refresh(now);
    }

    fn sync_leds(&mut self, mode: AgentMode, battery: Option<&BatterySnapshot>, now: Instant) {
        let mut changed = false;
        for d in &self.devices {
            changed |= self
                .settings
                .remember_device(&d.root.to_string_lossy(), &d.name);
        }
        if changed {
            self.save_settings();
        }
        let devices = self
            .devices
            .iter()
            .map(|d| {
                let path = d.root.to_string_lossy();
                DeviceConfig {
                    device: d.clone(),
                    display: self.settings.device_display(&path),
                    brightness: self.settings.device_brightness(&path),
                }
            })
            .collect();
        let full_charge_ratio = battery
            .map(|b| b.charge_speed_ratio(self.settings.full_charge_watts()))
            .unwrap_or(0.0);
        self.leds.send(Message::Sync(Box::new(LedRequest {
            devices,
            mode,
            battery: battery.cloned(),
            battery_preview: self.preview.active(now),
            full_charge_ratio,
        })));
    }

    fn menu_model(&self, snapshot: &Snapshot) -> MenuModel {
        let now = Utc::now();
        let sessions = recent_statuses(&snapshot.statuses)
            .iter()
            .map(|s| SessionRow {
                title: menu_title(s),
                mode: s.mode,
                agent_id: s.agent_id.clone(),
                tooltip: session_detail(s, now),
            })
            .collect();
        let connected: Vec<String> = self
            .devices
            .iter()
            .map(|d| d.root.to_string_lossy().into_owned())
            .collect();
        let mut devices: Vec<DeviceRow> = self
            .devices
            .iter()
            .map(|d| (d.root.to_string_lossy().into_owned(), d.name.clone()))
            .chain(
                self.settings
                    .remembered_devices()
                    .into_iter()
                    .filter(|(p, _)| !connected.contains(p)),
            )
            .map(|(path, name)| DeviceRow {
                connected: connected.contains(&path),
                display: self.settings.device_display(&path),
                brightness: self.settings.device_brightness(&path),
                path,
                name,
            })
            .collect();
        devices.dedup_by(|a, b| a.path == b.path);
        MenuModel {
            sessions,
            devices,
            lid_policy: self.settings.closed_lid_policy(),
            sleep_warning: self.closed_lid.last_error.clone(),
        }
    }

    /// `poll_devices_once`, run on volume mount/unmount/rename notifications instead of every
    /// 2 s: refresh only when the set of volumes changed.
    pub fn rescan_devices(&mut self) {
        let devices = discover_devices();
        let signature = device_signature(&devices);
        if signature != self.device_signature {
            crate::log(&format!("devices changed: {} connected", devices.len()));
            self.devices = devices;
            self.device_signature = signature;
            self.refresh();
        }
    }

    /// `pollLid:`, run once at startup and then on each clamshell notification instead of every
    /// second: the first reading is a baseline; changes play an animation.
    pub fn poll_lid(&mut self) {
        let Some(closed) = lid::clamshell_closed() else {
            return;
        };
        let previous = self.lid_closed.replace(closed);
        if previous.is_none() || previous == Some(closed) || self.devices.is_empty() {
            return;
        }
        let animation = if closed {
            self.settings
                .lid_closed_animation(&lid::default_closed_program(255))
        } else {
            self.settings
                .lid_open_animation(&lid::default_open_program(255))
        };
        crate::log(&format!("lid {}", if closed { "closed" } else { "opened" }));
        self.leds.send(Message::Animate {
            program: animation.program,
            duration: Duration::from_secs_f64(animation.duration_seconds),
        });
    }

    pub fn flush_state(&mut self) {
        if let Err(e) = self.monitor.flush() {
            crate::log(&format!("could not write latest.json: {e}"));
        }
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            crate::log(&format!("could not save settings: {e}"));
        }
    }

    fn find_status(&self, agent_id: &str) -> Option<AgentStatus> {
        let snapshot = self.snapshot.as_ref()?;
        snapshot
            .statuses
            .iter()
            .chain(snapshot.stale.iter())
            .find(|s| s.agent_id == agent_id)
            .cloned()
    }

    pub fn handle(&mut self, action: Action) {
        match action {
            Action::OpenSession(id) => {
                let Some(status) = self.find_status(&id) else {
                    return;
                };
                match resolve_open(&status, &self.settings) {
                    Some(OpenTarget::Url(url)) => ui::open_url(&url),
                    Some(OpenTarget::Terminal(command)) => open_terminal_command(&command),
                    None => crate::log(&format!("no open action for {}", status.display_name)),
                }
                return;
            }
            Action::SetDisplay(path, display) => {
                let name = self.device_name(&path);
                self.settings.set_device_display(&path, &name, display);
            }
            Action::SetBrightness(path, value) => {
                let name = self.device_name(&path);
                self.settings.set_device_brightness(&path, &name, value);
            }
            Action::RemoveDevice(path) => self.settings.forget_device(&path),
            Action::SetLidPolicy(policy) => self.settings.set_closed_lid_policy(policy),
            Action::Quit => {
                self.shutdown();
                ui::terminate(self.mtm);
                return;
            }
            Action::MenuOpening => {
                // Session ages in tooltips are only refreshed when someone looks at them.
                self.refresh();
                return;
            }
        }
        self.save_settings();
        self.refresh();
    }

    fn device_name(&self, path: &str) -> String {
        self.devices
            .iter()
            .find(|d| d.root.to_string_lossy() == path)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| {
                std::path::Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
    }

    /// Release power assertions and persist state (Quit releases both caffeinate holds in Python).
    pub fn shutdown(&mut self) {
        self.keep_awake.release();
        self.closed_lid.release();
        self.flush_state();
        crate::log("stopped");
    }
}

fn session_detail(status: &AgentStatus, now: chrono::DateTime<Utc>) -> String {
    let age = (now - status.updated_at).num_milliseconds() as f64 / 1000.0;
    let mut parts = vec![
        crate::session::state_for_mode(status.mode)
            .label
            .to_string(),
        format_age(age),
    ];
    parts.extend(status.origin.clone().filter(|s| !s.is_empty()));
    parts.extend(status.tool_name.clone().filter(|s| !s.is_empty()));
    parts.join(" · ")
}

fn open_terminal_command(command: &str) {
    let script = format!(
        "tell application \"Terminal\"\n  activate\n  do script {}\nend tell",
        applescript_quote(command)
    );
    let _ = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub fn dispatch_time(seconds: f64) -> dispatch2::DispatchTime {
    dispatch2::DispatchTime::NOW.time((seconds * 1e9) as i64)
}
