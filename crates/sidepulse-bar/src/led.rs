//! LED worker thread: owns the per-device controllers and does every SD-card write, so a slow or
//! sleeping card never blocks the menu bar (Python used one worker per sync and *dropped* syncs
//! that arrived while one was in flight; here the latest request always wins and none are lost).
//!
//! Also handles what Python did on the 15 s refresh: keepalive touches (≤ once per 60 s per
//! volume) and the charging-pulse rewrite, which now follows the intended wattage-based cadence
//! (`max(0.9, 3.0 − 2.1·ratio)` s) instead of being capped by the refresh interval. The thread
//! sleeps exactly until the next of those is due (or a new request arrives), never on a fixed tick.
//!
//! With `SIDEPULSE_DRY_RUN=1` the worker logs each write and touch instead of doing it, so the app
//! can run against the real `/Volumes` without touching a device another app owns.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use sidepulse_core::battery::{charging_update_interval, BatteryLedController, BatterySnapshot};
use sidepulse_core::devices::{write_program_to, Device, LedController};
use sidepulse_core::lid::lid_animation_program;
use sidepulse_core::models::AgentMode;
use sidepulse_core::power::{keepalive_targets, touch, KeepaliveToucher};

use crate::settings::LedDisplay;

#[derive(Clone, Debug)]
pub struct DeviceConfig {
    pub device: Device,
    pub display: LedDisplay,
    pub brightness: u8,
}

#[derive(Clone, Debug)]
pub struct LedRequest {
    pub devices: Vec<DeviceConfig>,
    pub mode: AgentMode,
    pub battery: Option<BatterySnapshot>,
    /// Plug/unplug preview: show battery on every device regardless of its display setting.
    pub battery_preview: bool,
    pub full_charge_ratio: f64,
}

pub enum Message {
    Sync(Box<LedRequest>),
    /// Lid open/close animation: program plus how long to suppress normal output.
    Animate {
        program: String,
        duration: Duration,
    },
}

#[derive(Default)]
struct DeviceState {
    agent: Option<LedController>,
    battery: Option<BatteryLedController>,
    showing_battery: Option<bool>,
}

type WriteFn = fn(&Path, &str) -> io::Result<()>;
type TouchFn = fn(&Path) -> io::Result<()>;

/// How the worker reaches the card: real writes, or log-only for `SIDEPULSE_DRY_RUN`.
#[derive(Clone, Copy)]
struct Io {
    write: WriteFn,
    touch: TouchFn,
}

impl Io {
    fn new(dry_run: bool) -> Self {
        if dry_run {
            Self {
                write: dry_write,
                touch: dry_touch,
            }
        } else {
            Self {
                write: write_program_to,
                touch,
            }
        }
    }
}

fn dry_write(target: &Path, text: &str) -> io::Result<()> {
    let preview: String = text.chars().take(80).collect();
    crate::log(&format!(
        "dry run: would write {} bytes to {}: {preview:?}",
        text.len(),
        target.display()
    ));
    Ok(())
}

fn dry_touch(path: &Path) -> io::Result<()> {
    crate::log(&format!("dry run: would touch {}", path.display()));
    Ok(())
}

/// `SIDEPULSE_DRY_RUN` set to anything but empty or `0`.
pub fn dry_run_from_env() -> bool {
    std::env::var("SIDEPULSE_DRY_RUN").is_ok_and(|v| !v.is_empty() && v != "0")
}

pub struct LedWorker {
    tx: Sender<Message>,
}

impl LedWorker {
    pub fn start(dry_run: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let io = Io::new(dry_run);
        std::thread::Builder::new()
            .name("sidepulse-leds".into())
            .spawn(move || run(rx, io))
            .expect("spawn LED worker");
        Self { tx }
    }

    pub fn send(&self, message: Message) {
        let _ = self.tx.send(message);
    }
}

fn run(rx: Receiver<Message>, io: Io) {
    let mut states: HashMap<PathBuf, DeviceState> = HashMap::new();
    let mut toucher = KeepaliveToucher::new(io.touch, false);
    let mut latest: Option<LedRequest> = None;
    loop {
        let message = match next_wake(latest.as_ref(), &toucher, Instant::now()) {
            // Nothing scheduled: block until the next request.
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            },
            Some(wait) => match rx.recv_timeout(wait) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };
        // Coalesce a burst: keep the newest sync, but never skip an animation.
        let mut pending = message.into_iter().collect::<Vec<_>>();
        pending.extend(rx.try_iter());
        for message in pending {
            match message {
                Message::Sync(request) => latest = Some(*request),
                Message::Animate { program, duration } => {
                    if let Some(request) = &latest {
                        animate(request, &program, io.write);
                    }
                    std::thread::sleep(duration + Duration::from_millis(150));
                    // `restore_led_display`: force a full rewrite after the animation.
                    states.clear();
                }
            }
        }
        if let Some(request) = &latest {
            apply(request, &mut states, &mut toucher, io.write);
        }
    }
}

/// How long to sleep before the next keepalive touch or charging-pulse rewrite is due, or `None`
/// to wait for the next request. Keepalive is due exactly 60 s after each volume's last touch.
fn next_wake(
    request: Option<&LedRequest>,
    toucher: &KeepaliveToucher,
    now: Instant,
) -> Option<Duration> {
    let request = request?;
    let keepalive = toucher
        .next_due(&request_keepalive_targets(request), now)
        .map(|due| due.saturating_duration_since(now));
    match (keepalive, next_battery_rewrite(request)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn request_keepalive_targets(request: &LedRequest) -> Vec<PathBuf> {
    let devices: Vec<Device> = request.devices.iter().map(|c| c.device.clone()).collect();
    keepalive_targets(&devices)
}

fn animate(request: &LedRequest, program: &str, write: WriteFn) {
    for cfg in &request.devices {
        if let Ok(text) = lid_animation_program(program, cfg.brightness) {
            let _ = write(&cfg.device.target(), &text);
        }
    }
}

fn shows_battery(request: &LedRequest, cfg: &DeviceConfig) -> bool {
    request.battery.is_some() && (cfg.display == LedDisplay::Battery || request.battery_preview)
}

fn next_battery_rewrite(request: &LedRequest) -> Option<Duration> {
    let battery = request.battery.as_ref()?;
    if !battery.is_charging
        || !request
            .devices
            .iter()
            .any(|cfg| shows_battery(request, cfg))
    {
        return None;
    }
    Some(Duration::from_secs_f64(charging_update_interval(
        request.full_charge_ratio,
    )))
}

fn apply(
    request: &LedRequest,
    states: &mut HashMap<PathBuf, DeviceState>,
    toucher: &mut KeepaliveToucher,
    write: WriteFn,
) {
    let now = Instant::now();
    states.retain(|root, _| request.devices.iter().any(|c| &c.device.root == root));
    for cfg in &request.devices {
        let state = states.entry(cfg.device.root.clone()).or_default();
        let battery_mode = shows_battery(request, cfg);
        if state.showing_battery != Some(battery_mode) {
            // A change of display kind resets both controllers so the new program is written.
            state.agent = None;
            state.battery = None;
            state.showing_battery = Some(battery_mode);
        }
        let result = match (&request.battery, battery_mode) {
            (Some(snapshot), true) => state
                .battery
                .get_or_insert_with(|| BatteryLedController::new(cfg.device.clone()))
                .sync_with(snapshot, cfg.brightness, now, write),
            _ => state
                .agent
                .get_or_insert_with(|| LedController::new(cfg.device.clone()))
                .sync_with(request.mode, cfg.brightness, now, write),
        };
        match result {
            Ok(Some(_)) => crate::log(&format!("LEDs updated on {}", cfg.device.root.display())),
            Ok(None) => {}
            Err(e) => crate::log(&format!(
                "LED write failed on {}: {e}",
                cfg.device.root.display()
            )),
        }
    }
    for target in request_keepalive_targets(request) {
        toucher.poke(&target, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_touch(_: &Path) -> io::Result<()> {
        Ok(())
    }

    fn request(root: &Path) -> LedRequest {
        LedRequest {
            devices: vec![DeviceConfig {
                device: test_device(root),
                display: LedDisplay::Agent,
                brightness: 255,
            }],
            mode: AgentMode::IdleReady,
            battery: None,
            battery_preview: false,
            full_charge_ratio: 0.0,
        }
    }

    fn test_device(root: &Path) -> Device {
        let dir = tempfile::tempdir().unwrap();
        let dev = dir.path().join("SidePulsePro");
        std::fs::create_dir(&dev).unwrap();
        std::fs::write(dev.join("LEDS.LED"), b"").unwrap();
        let mut d = sidepulse_core::devices::discover_in(dir.path())
            .into_iter()
            .next()
            .expect("fake device");
        d.root = root.to_path_buf();
        d
    }

    #[test]
    fn sleeps_until_the_keepalive_is_due_and_blocks_without_requests() {
        let t0 = Instant::now();
        let mut toucher = KeepaliveToucher::new(ok_touch, false);
        assert_eq!(next_wake(None, &toucher, t0), None);
        let req = request(Path::new("/tmp/fake/SidePulsePro"));
        // Never touched: due now.
        assert_eq!(next_wake(Some(&req), &toucher, t0), Some(Duration::ZERO));
        for target in request_keepalive_targets(&req) {
            toucher.poke(&target, t0);
        }
        let later = t0 + Duration::from_secs(25);
        assert_eq!(
            next_wake(Some(&req), &toucher, later),
            Some(Duration::from_secs(35))
        );
    }
}
