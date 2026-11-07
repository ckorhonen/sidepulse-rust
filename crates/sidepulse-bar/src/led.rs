//! LED worker thread: owns the per-device controllers and does every SD-card write, so a slow or
//! sleeping card never blocks the menu bar (Python used one worker per sync and *dropped* syncs
//! that arrived while one was in flight; here the latest request always wins and none are lost).
//!
//! Also handles what Python did on the 15 s refresh: keepalive touches (≤ once per 60 s per
//! volume) and the charging-pulse rewrite, which now follows the intended wattage-based cadence
//! (`max(0.9, 3.0 − 2.1·ratio)` s) instead of being capped by the refresh interval.

use std::collections::HashMap;
use std::path::PathBuf;
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

pub struct LedWorker {
    tx: Sender<Message>,
}

impl LedWorker {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("sidepulse-leds".into())
            .spawn(move || run(rx))
            .expect("spawn LED worker");
        Self { tx }
    }

    pub fn send(&self, message: Message) {
        let _ = self.tx.send(message);
    }
}

const KEEPALIVE_CHECK: Duration = Duration::from_secs(20);

fn run(rx: Receiver<Message>) {
    let mut states: HashMap<PathBuf, DeviceState> = HashMap::new();
    let mut toucher = KeepaliveToucher::new(touch, false);
    let mut latest: Option<LedRequest> = None;
    loop {
        let wait = latest
            .as_ref()
            .and_then(next_battery_rewrite)
            .unwrap_or(KEEPALIVE_CHECK)
            .min(KEEPALIVE_CHECK);
        let message = match rx.recv_timeout(wait) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Coalesce a burst: keep the newest sync, but never skip an animation.
        let mut pending = message.into_iter().collect::<Vec<_>>();
        pending.extend(rx.try_iter());
        for message in pending {
            match message {
                Message::Sync(request) => latest = Some(*request),
                Message::Animate { program, duration } => {
                    if let Some(request) = &latest {
                        animate(request, &program);
                    }
                    std::thread::sleep(duration + Duration::from_millis(150));
                    // `restore_led_display`: force a full rewrite after the animation.
                    states.clear();
                }
            }
        }
        if let Some(request) = &latest {
            apply(request, &mut states, &mut toucher);
        }
    }
}

fn animate(request: &LedRequest, program: &str) {
    for cfg in &request.devices {
        if let Ok(text) = lid_animation_program(program, cfg.brightness) {
            let _ = write_program_to(&cfg.device.target(), &text);
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
                .sync(snapshot, cfg.brightness, now),
            _ => state
                .agent
                .get_or_insert_with(|| LedController::new(cfg.device.clone()))
                .sync(request.mode, cfg.brightness, now),
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
    let devices: Vec<Device> = request.devices.iter().map(|c| c.device.clone()).collect();
    for target in keepalive_targets(&devices) {
        toucher.poke(&target, now);
    }
}
