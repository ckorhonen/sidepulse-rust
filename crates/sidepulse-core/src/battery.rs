//! Battery state and battery LED programs (`battery.py`).
//!
//! API:
//! - [`BatterySnapshot`], [`PdProfile`]; [`read`] / [`read_with`] read `AppleSmartBattery`
//!   natively via IOKit (`None` on a Mac without a battery or off macOS).
//! - [`snapshot_from_props`]: `snapshot_from_ioreg_props` over a JSON object, so parsing is
//!   testable without hardware.
//! - [`battery_program`] / [`battery_program_with`]: `program_for_battery`; [`battery_fill`],
//!   [`battery_color`], [`scale_hex_color`], [`charging_pulse_length_ms`].
//! - [`should_rewrite`], [`charging_update_interval`]: charging re-animation cadence.
//! - [`BatteryLedController`]: Python's `BatteryLedController.sync_snapshot`.
//! - [`PowerChangePreview`]: `update_battery_power_preview` / `active_led_display_kind` timing.
//! - [`default_full_charge_watts`], [`full_charge_watts_for_product`], [`adapter_family_name`].
//!
//! Differences from Python: no `ioreg -a` subprocess (properties come straight from
//! `IORegistryEntryCreateCFProperties`), and model detection reads the `IODeviceTree:/product`
//! entry directly without the `system_profiler` fallback (which never matched a 14/16-inch
//! model string that the device tree lacked).

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::devices::{
    apply_brightness, py_round, write_program_to, Device, SyncResult, ERROR_RETRY,
};
use crate::pyjson::truthy;

pub const BATTERY_LOW_RED: &str = "#FF2600";
pub const BATTERY_MID_AMBER: &str = "#FFB000";
pub const BATTERY_HIGH_GREEN: &str = "#00FF66";
pub const BATTERY_CHARGING_MINT: &str = "#80FFC8";
pub const BATTERY_OFF: &str = "#000000";
/// `program_for_battery(transition_ms=360)`.
pub const DEFAULT_TRANSITION_MS: i64 = 360;
/// `DEFAULT_POWER_CHANGE_PREVIEW_SECONDS`.
pub const DEFAULT_POWER_CHANGE_PREVIEW: Duration = Duration::from_secs(7);

/// `PDProfile`: one USB-PD source capability.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PdProfile {
    pub voltage: f64,
    pub current: f64,
}

impl PdProfile {
    pub fn watts(&self) -> f64 {
        self.voltage * self.current
    }
}

/// `BatterySnapshot`; field defaults match the Python dataclass.
#[derive(Clone, Debug, PartialEq)]
pub struct BatterySnapshot {
    pub percent: i64,
    pub is_charging: bool,
    pub is_charged: bool,
    pub is_plugged: bool,
    pub battery_present: bool,
    pub voltage: f64,
    pub amperage: f64,
    pub battery_watts: f64,
    pub time_to_full: i64,
    pub time_to_empty: i64,
    pub cycle_count: i64,
    pub temperature_c: Option<f64>,
    pub design_capacity_mah: i64,
    pub max_capacity_mah: i64,
    pub current_capacity_mah: i64,
    pub health_percent: i64,
    pub condition: String,
    pub adapter_connected: bool,
    pub adapter_watts: i64,
    pub adapter_voltage: f64,
    pub adapter_current: f64,
    pub adapter_name: String,
    pub adapter_manufacturer: String,
    pub adapter_model: String,
    pub adapter_serial: String,
    pub adapter_family: String,
    pub pd_profiles: Vec<PdProfile>,
    pub full_charge_watts: f64,
}

impl Default for BatterySnapshot {
    fn default() -> Self {
        Self {
            percent: 0,
            is_charging: false,
            is_charged: false,
            is_plugged: false,
            battery_present: true,
            voltage: 0.0,
            amperage: 0.0,
            battery_watts: 0.0,
            time_to_full: -1,
            time_to_empty: -1,
            cycle_count: -1,
            temperature_c: None,
            design_capacity_mah: -1,
            max_capacity_mah: -1,
            current_capacity_mah: -1,
            health_percent: -1,
            condition: String::new(),
            adapter_connected: false,
            adapter_watts: -1,
            adapter_voltage: 0.0,
            adapter_current: 0.0,
            adapter_name: String::new(),
            adapter_manufacturer: String::new(),
            adapter_model: String::new(),
            adapter_serial: String::new(),
            adapter_family: String::new(),
            pd_profiles: Vec::new(),
            full_charge_watts: 100.0,
        }
    }
}

impl BatterySnapshot {
    /// `adapter_power`: rated watts, else negotiated V x I, else the best PD profile.
    pub fn adapter_power(&self) -> f64 {
        if self.adapter_watts > 0 {
            return self.adapter_watts as f64;
        }
        let negotiated = self.adapter_voltage * self.adapter_current;
        if negotiated > 0.0 {
            return negotiated;
        }
        self.pd_profiles
            .iter()
            .map(PdProfile::watts)
            .fold(None, |best: Option<f64>, w| {
                Some(best.map_or(w, |b| b.max(w)))
            })
            .unwrap_or(0.0)
    }

    /// `charge_speed_ratio(full_charge_watts)`; `None` or `0` fall back to the snapshot's value
    /// (Python `full_charge_watts or self.full_charge_watts`).
    pub fn charge_speed_ratio(&self, full_charge_watts: Option<f64>) -> f64 {
        let baseline = full_charge_watts
            .filter(|w| *w != 0.0)
            .unwrap_or(self.full_charge_watts);
        if baseline <= 0.0 {
            return 0.0;
        }
        clamp(self.adapter_power() / baseline)
    }

    /// `battery_state_label`.
    pub fn state_label(&self) -> &'static str {
        if self.is_charged {
            "charged"
        } else if self.is_charging {
            "charging"
        } else if self.is_plugged {
            "plugged in"
        } else {
            "on battery"
        }
    }
}

/// The top-level `AppleSmartBattery` keys `snapshot_from_ioreg_props` uses.
pub const BATTERY_KEYS: [&str; 19] = [
    "Voltage",
    "Amperage",
    "InstantAmperage",
    "AppleRawMaxCapacity",
    "AppleRawCurrentCapacity",
    "DesignCapacity",
    "MaxCapacity",
    "CurrentCapacity",
    "AdapterDetails",
    "StateOfCharge",
    "PermanentFailureStatus",
    "BatteryInstalled",
    "IsCharging",
    "FullyCharged",
    "ExternalConnected",
    "AvgTimeToFull",
    "AvgTimeToEmpty",
    "CycleCount",
    "Temperature",
];

/// Power-source change notifications (`IOPSNotificationCreateRunLoopSource`): plug, unplug,
/// charge level and time-remaining changes, delivered on the main run loop. Replaces re-reading
/// the battery on a fixed refresh timer. Removed on drop.
pub struct PowerSourceWatch {
    #[cfg(target_os = "macos")]
    _inner: crate::power::iokit::PowerSourceWatch,
}

impl PowerSourceWatch {
    /// Calls `on_change` on the main thread after each power-source change. `None` when
    /// notifications are unavailable. Must be called on the main thread.
    #[cfg(target_os = "macos")]
    pub fn new(on_change: impl FnMut() + 'static) -> Option<Self> {
        let inner = crate::power::iokit::PowerSourceWatch::new(Box::new(on_change))?;
        Some(Self { _inner: inner })
    }

    #[cfg(not(target_os = "macos"))]
    pub fn new(_on_change: impl FnMut() + 'static) -> Option<Self> {
        None
    }
}

/// `read_battery_snapshot()` with the model's default full-charge watts.
pub fn read() -> Option<BatterySnapshot> {
    read_with(None)
}

/// `read_battery_snapshot(full_charge_watts=...)`: reads only [`BATTERY_KEYS`] from the
/// `AppleSmartBattery` service's property dictionary.
pub fn read_with(full_charge_watts: Option<f64>) -> Option<BatterySnapshot> {
    let props = read_props()?;
    let mut snapshot = snapshot_from_props(&props);
    snapshot.full_charge_watts = full_charge_watts
        .unwrap_or_else(default_full_charge_watts)
        .max(1.0);
    Some(snapshot)
}

#[cfg(target_os = "macos")]
fn read_props() -> Option<Map<String, Value>> {
    use crate::power::iokit::{dict_get, IoObject};
    let service = IoObject::matching_service("AppleSmartBattery")?;
    let dict = service.properties()?;
    let mut props = Map::new();
    for key in BATTERY_KEYS {
        if let Some(value) = dict_get(&dict, key) {
            props.insert(key.to_string(), value);
        }
    }
    Some(props)
}

#[cfg(not(target_os = "macos"))]
fn read_props() -> Option<Map<String, Value>> {
    None
}

/// `int_value(value, default)`: bools and ints as-is, floats truncated, anything else `default`.
fn int_value(value: Option<&Value>, default: i64) -> i64 {
    match value {
        Some(Value::Bool(b)) => i64::from(*b),
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| u as i64))
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .unwrap_or(default),
        _ => default,
    }
}

fn str_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// `bool(props.get(key, default))`.
fn bool_or(props: &Map<String, Value>, key: &str, default: bool) -> bool {
    props.get(key).map_or(default, |v| truthy(Some(v)))
}

/// `temperature_celsius`.
fn temperature_celsius(value: Option<&Value>) -> Option<f64> {
    let raw = int_value(value, -1);
    (raw >= 0).then(|| raw as f64 / 100.0)
}

/// `snapshot_from_ioreg_props`. `full_charge_watts` is left at the default 100; [`read_with`]
/// fills it in (Python sets it in `snapshot_with_full_charge_watts`).
pub fn snapshot_from_props(props: &Map<String, Value>) -> BatterySnapshot {
    let voltage = int_value(props.get("Voltage"), 0) as f64 / 1000.0;
    let mut amperage_ma = int_value(props.get("Amperage"), 0);
    if amperage_ma == 0 {
        amperage_ma = int_value(props.get("InstantAmperage"), 0);
    }
    let amperage = amperage_ma as f64 / 1000.0;

    let raw_max = int_value(props.get("AppleRawMaxCapacity"), -1);
    let raw_current = int_value(props.get("AppleRawCurrentCapacity"), -1);
    let design = int_value(props.get("DesignCapacity"), -1);
    let max_capacity = if raw_max > 0 {
        raw_max
    } else {
        int_value(props.get("MaxCapacity"), -1)
    };
    let current_capacity = if raw_current >= 0 {
        raw_current
    } else {
        int_value(props.get("CurrentCapacity"), -1)
    };
    let health_percent = if raw_max > 0 && design > 0 {
        py_round(raw_max as f64 / design as f64 * 100.0)
    } else {
        -1
    };

    let empty = Map::new();
    let details = match props.get("AdapterDetails") {
        Some(Value::Object(map)) => map,
        _ => &empty,
    };
    let adapter = parse_adapter_details(details);

    let mut percent = int_value(props.get("CurrentCapacity"), 0);
    if !(0..=100).contains(&percent) {
        percent = int_value(props.get("StateOfCharge"), 0);
    }

    let condition = if int_value(props.get("PermanentFailureStatus"), 0) != 0 {
        "Service Recommended"
    } else if props.get("BatteryInstalled") != Some(&Value::Bool(false)) {
        "Normal"
    } else {
        ""
    };

    BatterySnapshot {
        percent: percent.clamp(0, 100),
        is_charging: bool_or(props, "IsCharging", false),
        is_charged: bool_or(props, "FullyCharged", false),
        is_plugged: bool_or(props, "ExternalConnected", false),
        battery_present: bool_or(props, "BatteryInstalled", true),
        voltage,
        amperage,
        battery_watts: voltage * amperage,
        time_to_full: int_value(props.get("AvgTimeToFull"), -1),
        time_to_empty: int_value(props.get("AvgTimeToEmpty"), -1),
        cycle_count: int_value(props.get("CycleCount"), -1),
        temperature_c: temperature_celsius(props.get("Temperature")),
        design_capacity_mah: design,
        max_capacity_mah: max_capacity,
        current_capacity_mah: current_capacity,
        health_percent,
        condition: condition.to_string(),
        ..adapter
    }
}

/// `parse_adapter_details`, returned as a snapshot carrying only the adapter fields.
fn parse_adapter_details(details: &Map<String, Value>) -> BatterySnapshot {
    let mut profiles: Vec<PdProfile> = match details.get("UsbHvcMenu") {
        Some(Value::Array(menu)) => menu
            .iter()
            .filter_map(Value::as_object)
            .map(|entry| PdProfile {
                voltage: int_value(entry.get("MaxVoltage"), 0) as f64 / 1000.0,
                current: int_value(entry.get("MaxCurrent"), 0) as f64 / 1000.0,
            })
            .filter(|p| p.voltage > 0.0 && p.current > 0.0)
            .collect(),
        _ => Vec::new(),
    };
    // `list.sort` is stable, like `sort_by`.
    profiles.sort_by(|a, b| a.watts().total_cmp(&b.watts()));
    let voltage_mv = int_value(details.get("AdapterVoltage").or(details.get("Voltage")), 0);
    BatterySnapshot {
        adapter_connected: !details.is_empty(),
        adapter_watts: int_value(details.get("Watts"), -1),
        adapter_voltage: voltage_mv as f64 / 1000.0,
        adapter_current: int_value(details.get("Current"), 0) as f64 / 1000.0,
        adapter_name: str_value(details.get("Name")),
        adapter_manufacturer: str_value(details.get("Manufacturer")),
        adapter_model: str_value(details.get("Model")),
        adapter_serial: str_value(details.get("SerialString").or(details.get("SerialNumber"))),
        adapter_family: adapter_family_name(int_value(details.get("FamilyCode"), -1)),
        pd_profiles: profiles,
        ..BatterySnapshot::default()
    }
}

/// `adapter_family_name(code)`.
pub fn adapter_family_name(code: i64) -> String {
    match code {
        0xE000_4000..=0xE000_4FFF => "USB-C Power Delivery".into(),
        0x0000_0003 => "MagSafe".into(),
        0x0000_0004 => "MagSafe 2".into(),
        0x0000_FF00 => "USB-C".into(),
        c if c >= 0 => format!("0x{c:08x}"),
        _ => String::new(),
    }
}

/// `clamp(value)` to 0..=1.
fn clamp(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

/// `BatteryFill`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BatteryFill {
    pub full: usize,
    pub partial: f64,
    pub count: usize,
}

/// `battery_fill(percent, led_count)`.
pub fn battery_fill(percent: i64, led_count: u8) -> BatteryFill {
    let count = led_count.clamp(1, 8) as usize;
    let clamped = percent.clamp(0, 100);
    let raw = count as f64 * (clamped as f64 / 100.0);
    let full = count.min(raw.floor() as usize);
    let partial = if full >= count {
        0.0
    } else {
        raw - full as f64
    };
    BatteryFill {
        full,
        partial,
        count,
    }
}

/// `battery_fill_color`.
pub fn battery_fill_color(index: usize, fill: &BatteryFill, color: &str) -> String {
    if index < fill.full {
        color.to_string()
    } else if index == fill.full && fill.partial > 0.0 {
        scale_hex_color(color, fill.partial)
    } else {
        BATTERY_OFF.to_string()
    }
}

/// `scale_hex_color`: scales each channel of `#RRGGBB`; other strings pass through.
pub fn scale_hex_color(color: &str, ratio: f64) -> String {
    let bytes = color.as_bytes();
    if bytes.len() != 7 || bytes[0] != b'#' || !bytes[1..].iter().all(u8::is_ascii_hexdigit) {
        return color.to_string();
    }
    let amount = ratio.clamp(0.0, 1.0);
    let mut out = String::from("#");
    for i in [1, 3, 5] {
        let channel = u8::from_str_radix(&color[i..i + 2], 16).unwrap_or(0);
        out.push_str(&format!("{:02X}", py_round(f64::from(channel) * amount)));
    }
    out
}

/// `battery_segment`.
fn battery_segment(index: usize, color: &str, transition_ms: i64) -> String {
    if transition_ms <= 0 {
        format!("{index}:{color}")
    } else {
        format!("{index}:{color} {transition_ms}ms ease")
    }
}

/// `battery_color(percent)`.
pub fn battery_color(percent: i64) -> &'static str {
    if percent <= 20 {
        BATTERY_LOW_RED
    } else if percent <= 50 {
        BATTERY_MID_AMBER
    } else {
        BATTERY_HIGH_GREEN
    }
}

/// `charging_pulse_length_ms(ratio)`: `round(180 + 1220 * ratio)`.
pub fn charging_pulse_length_ms(ratio: f64) -> i64 {
    py_round(180.0 + 1220.0 * clamp(ratio))
}

/// `charging_update_interval_seconds(ratio)`: `max(0.9, 3.0 - 2.1 * ratio)` seconds.
pub fn charging_update_interval(ratio: f64) -> f64 {
    (3.0 - 2.1 * clamp(ratio)).max(0.9)
}

/// `should_rewrite_battery_program(snapshot, elapsed_seconds)`.
pub fn should_rewrite(snapshot: &BatterySnapshot, elapsed_seconds: f64) -> bool {
    if !snapshot.is_plugged || snapshot.is_charged {
        return false;
    }
    elapsed_seconds >= charging_update_interval(snapshot.charge_speed_ratio(None))
}

/// `program_for_battery(snapshot, led_count=..., full_charge_watts=..., brightness=...)`.
pub fn battery_program(
    snapshot: &BatterySnapshot,
    led_count: u8,
    full_charge_watts: Option<f64>,
    brightness: u8,
) -> String {
    battery_program_with(
        snapshot,
        led_count,
        full_charge_watts,
        DEFAULT_TRANSITION_MS,
        brightness,
    )
}

/// [`battery_program`] with an explicit `transition_ms`.
pub fn battery_program_with(
    snapshot: &BatterySnapshot,
    led_count: u8,
    full_charge_watts: Option<f64>,
    transition_ms: i64,
    brightness: u8,
) -> String {
    let count = led_count.clamp(1, 8) as usize;
    let percent = snapshot.percent.clamp(0, 100);
    let fill = battery_fill(percent, count as u8);
    let pulse_index = fill.full.min(count - 1);
    let transition = transition_ms.max(0);
    let row = |color_for: &dyn Fn(usize) -> String| {
        (0..count)
            .map(|i| battery_segment(i, &color_for(i), transition))
            .collect::<Vec<_>>()
            .join(";")
    };

    if !snapshot.is_plugged || snapshot.is_charged {
        if snapshot.is_charged || percent >= 100 {
            return apply_brightness(&row(&|_| BATTERY_HIGH_GREEN.to_string()), brightness);
        }
        let color = battery_color(percent);
        return apply_brightness(&row(&|i| battery_fill_color(i, &fill, color)), brightness);
    }

    let color = BATTERY_CHARGING_MINT;
    let pulse_ms = charging_pulse_length_ms(snapshot.charge_speed_ratio(full_charge_watts));
    let program = format!(
        "{}\n{pulse_index}:{color} {pulse_ms}ms pulse",
        row(&|i| battery_fill_color(i, &fill, color))
    );
    apply_brightness(&program, brightness)
}

/// `BatteryLedController` for one device: rewrites when the program changes, re-animates the
/// charging pulse on the [`should_rewrite`] cadence, and backs off for [`ERROR_RETRY`] after a
/// failed write of the same program.
#[derive(Debug)]
pub struct BatteryLedController {
    pub device: Device,
    pub error_retry: Duration,
    last_program: Option<String>,
    last_error: Option<String>,
    last_attempt: Option<Instant>,
    last_write: Option<Instant>,
}

impl BatteryLedController {
    pub fn new(device: Device) -> Self {
        Self {
            device,
            error_retry: ERROR_RETRY,
            last_program: None,
            last_error: None,
            last_attempt: None,
            last_write: None,
        }
    }

    /// `reset()`.
    pub fn reset(&mut self) {
        self.last_program = None;
        self.last_error = None;
        self.last_attempt = None;
        self.last_write = None;
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// `sync_snapshot(snapshot)`. Like Python, the program uses the snapshot's own
    /// `full_charge_watts`.
    pub fn sync(&mut self, snapshot: &BatterySnapshot, brightness: u8, now: Instant) -> SyncResult {
        self.sync_with(snapshot, brightness, now, write_program_to)
    }

    /// [`BatteryLedController::sync`] with an injected writer (tests, dry runs).
    pub fn sync_with(
        &mut self,
        snapshot: &BatterySnapshot,
        brightness: u8,
        now: Instant,
        write: impl FnOnce(&Path, &str) -> std::io::Result<()>,
    ) -> SyncResult {
        let program = battery_program(snapshot, self.device.led_count, None, brightness);
        let same = self.last_program.as_deref() == Some(program.as_str());
        if same && self.last_error.is_none() {
            let elapsed = self.last_write.map_or(f64::INFINITY, |t| {
                now.saturating_duration_since(t).as_secs_f64()
            });
            if !should_rewrite(snapshot, elapsed) {
                return Ok(None);
            }
        }
        if same {
            if let Some(error) = &self.last_error {
                let recent = self
                    .last_attempt
                    .is_some_and(|t| now.saturating_duration_since(t) < self.error_retry);
                if recent {
                    return Err(error.clone());
                }
            }
        }
        self.last_attempt = Some(now);
        self.last_program = Some(program.clone());
        match write(&self.device.target(), &program) {
            Ok(()) => {
                self.last_error = None;
                self.last_write = Some(now);
                Ok(Some(program))
            }
            Err(e) => {
                let message = e.to_string();
                self.last_error = Some(message.clone());
                Err(message)
            }
        }
    }
}

/// `update_battery_power_preview` + the preview half of `active_led_display_kind`
/// (status_bar.py): show the battery for a few seconds after the charger is (un)plugged.
#[derive(Debug, Default)]
pub struct PowerChangePreview {
    last_connected: Option<bool>,
    until: Option<Instant>,
}

impl PowerChangePreview {
    /// Records the current plug state; starts a preview of `duration` when it flipped and
    /// `enabled`. Returns whether a preview started.
    pub fn observe(
        &mut self,
        plugged: bool,
        enabled: bool,
        duration: Duration,
        now: Instant,
    ) -> bool {
        let flipped = self.last_connected.is_some_and(|last| last != plugged);
        self.last_connected = Some(plugged);
        if flipped && enabled {
            self.until = Some(now + duration);
            return true;
        }
        false
    }

    /// Whether the battery display should currently override the agent display.
    pub fn active(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }
}

/// `default_full_charge_watts()`, cached for the process lifetime.
pub fn default_full_charge_watts() -> f64 {
    static CACHE: OnceLock<f64> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let (product, chip) = read_product_tree();
        full_charge_watts_for_product(&product, &chip)
    })
}

/// The model rule in `default_full_charge_watts`: 16-inch MacBook Pro 140 W, 14-inch with a Pro
/// or Max chip 96 W, other 14-inch 70 W, anything else 100 W.
pub fn full_charge_watts_for_product(product: &str, chip: &str) -> f64 {
    let product = product.to_lowercase();
    let chip = chip.to_lowercase();
    if product.contains("macbook pro") {
        if product.contains("16-inch") {
            return 140.0;
        }
        if product.contains("14-inch") {
            return if chip.contains("pro") || chip.contains("max") {
                96.0
            } else {
                70.0
            };
        }
    }
    100.0
}

/// Reads `product-name`, `product-description` and `product-soc-name` from
/// `IODeviceTree:/product` (Python greps `ioreg -p IODeviceTree -n product` text).
#[cfg(target_os = "macos")]
fn read_product_tree() -> (String, String) {
    use crate::power::iokit::{data_bytes, IoObject};
    let Some(entry) = IoObject::from_path("IODeviceTree:/product") else {
        return (String::new(), String::new());
    };
    let text = |key: &str| -> String {
        entry
            .property(key)
            .and_then(|v| data_bytes(&v))
            .map(|bytes| {
                let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                String::from_utf8_lossy(&bytes[..end]).into_owned()
            })
            .unwrap_or_default()
    };
    let product = format!("{} {}", text("product-name"), text("product-description"));
    (product, text("product-soc-name"))
}

#[cfg(not(target_os = "macos"))]
fn read_product_tree() -> (String, String) {
    (String::new(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;

    fn props(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn charging(percent: i64, adapter_watts: i64, full: f64) -> BatterySnapshot {
        BatterySnapshot {
            percent,
            is_plugged: true,
            is_charging: true,
            adapter_watts,
            full_charge_watts: full,
            ..Default::default()
        }
    }

    fn unplugged(percent: i64) -> BatterySnapshot {
        BatterySnapshot {
            percent,
            ..Default::default()
        }
    }

    /// `test_battery_parser_uses_adapter_watts_and_raw_capacity`.
    #[test]
    fn parser_uses_adapter_watts_and_raw_capacity() {
        let s = snapshot_from_props(&props(json!({
            "CurrentCapacity": 50, "ExternalConnected": true, "IsCharging": true,
            "FullyCharged": false, "Voltage": 12000, "Amperage": 1000,
            "AppleRawCurrentCapacity": 4000, "AppleRawMaxCapacity": 8000,
            "DesignCapacity": 10000, "CycleCount": 12,
            "AdapterDetails": {
                "Watts": 96, "AdapterVoltage": 20000, "Current": 4800,
                "UsbHvcMenu": [
                    {"MaxVoltage": 20000, "MaxCurrent": 4800},
                    {"MaxVoltage": 5000, "MaxCurrent": 3000}
                ]
            }
        })));
        assert_eq!(s.percent, 50);
        assert!(s.is_plugged && s.is_charging && !s.is_charged);
        assert_eq!(s.adapter_power(), 96.0);
        assert_eq!(s.health_percent, 80);
        assert_eq!(s.current_capacity_mah, 4000);
        assert_eq!(s.max_capacity_mah, 8000);
        assert_eq!(s.pd_profiles.len(), 2);
        assert_eq!(s.pd_profiles[0].voltage, 5.0, "sorted by watts");
        assert_eq!(s.battery_watts, 12.0);
        assert_eq!(s.cycle_count, 12);
        assert_eq!(s.condition, "Normal");
        assert!(s.adapter_connected);
        assert_eq!(s.temperature_c, None);
    }

    #[test]
    fn parser_fallbacks() {
        let s = snapshot_from_props(&props(json!({
            "CurrentCapacity": 150, "StateOfCharge": 42, "Amperage": 0,
            "InstantAmperage": -1500, "Voltage": 12000, "MaxCapacity": 90,
            "PermanentFailureStatus": 1, "Temperature": 3012, "IsCharging": 1,
            "AdapterDetails": {"Voltage": 9000, "Current": 2000, "FamilyCode": -536854518,
                               "SerialNumber": "abc"}
        })));
        assert_eq!(s.percent, 42);
        assert_eq!(s.amperage, -1.5);
        assert_eq!(s.battery_watts, -18.0);
        assert_eq!(s.max_capacity_mah, 90);
        assert_eq!(s.current_capacity_mah, 150);
        assert_eq!(s.health_percent, -1);
        assert_eq!(s.condition, "Service Recommended");
        assert_eq!(s.temperature_c, Some(30.12));
        assert!(s.is_charging);
        assert_eq!(s.adapter_watts, -1);
        assert_eq!(s.adapter_power(), 18.0);
        assert_eq!(s.adapter_serial, "abc");
        // Negative FamilyCode (as CF reports PD chargers) yields "" just like Python.
        assert_eq!(s.adapter_family, "");

        let none = snapshot_from_props(&props(json!({"BatteryInstalled": false})));
        assert_eq!(none.condition, "");
        assert!(!none.battery_present);
        assert!(!none.adapter_connected);
        assert_eq!(none.adapter_power(), 0.0);
    }

    #[test]
    fn adapter_power_falls_back_to_pd_profiles() {
        let s = BatterySnapshot {
            pd_profiles: vec![
                PdProfile {
                    voltage: 5.0,
                    current: 3.0,
                },
                PdProfile {
                    voltage: 20.0,
                    current: 3.25,
                },
            ],
            ..Default::default()
        };
        assert_eq!(s.adapter_power(), 65.0);
        assert_eq!(s.charge_speed_ratio(Some(0.0)), 0.65);
        assert_eq!(s.charge_speed_ratio(Some(50.0)), 1.0);
    }

    #[test]
    fn family_names() {
        assert_eq!(adapter_family_name(0xE000_4005), "USB-C Power Delivery");
        assert_eq!(adapter_family_name(3), "MagSafe");
        assert_eq!(adapter_family_name(4), "MagSafe 2");
        assert_eq!(adapter_family_name(0xFF00), "USB-C");
        assert_eq!(adapter_family_name(0x12), "0x00000012");
        assert_eq!(adapter_family_name(-1), "");
    }

    /// Byte-exact against `program_for_battery` output captured from the Python source.
    #[test]
    fn programs_match_python() {
        let mint_half = "0:#80FFC8 360ms ease;1:#80FFC8 360ms ease;2:#80FFC8 360ms ease;\
            3:#80FFC8 360ms ease;4:#000000 360ms ease;5:#000000 360ms ease;6:#000000 360ms ease;\
            7:#000000 360ms ease\n4:#80FFC8 790ms pulse";
        let cases: Vec<(BatterySnapshot, u8, &str)> = vec![
            (charging(50, 70, 140.0), 2, "0:#80FFC8 360ms ease;1:#000000 360ms ease\n1:#80FFC8 790ms pulse"),
            (charging(50, 70, 140.0), 8, mint_half),
            (unplugged(50), 2, "0:#FFB000 360ms ease;1:#000000 360ms ease"),
            (unplugged(50), 8, "0:#FFB000 360ms ease;1:#FFB000 360ms ease;2:#FFB000 360ms ease;3:#FFB000 360ms ease;4:#000000 360ms ease;5:#000000 360ms ease;6:#000000 360ms ease;7:#000000 360ms ease"),
            (unplugged(57), 2, "0:#00FF66 360ms ease;1:#00240E 360ms ease"),
            (unplugged(57), 8, "0:#00FF66 360ms ease;1:#00FF66 360ms ease;2:#00FF66 360ms ease;3:#00FF66 360ms ease;4:#008F39 360ms ease;5:#000000 360ms ease;6:#000000 360ms ease;7:#000000 360ms ease"),
            (charging(80, 140, 140.0), 2, "0:#80FFC8 360ms ease;1:#4D9978 360ms ease\n1:#80FFC8 1400ms pulse"),
            (charging(80, 140, 140.0), 8, "0:#80FFC8 360ms ease;1:#80FFC8 360ms ease;2:#80FFC8 360ms ease;3:#80FFC8 360ms ease;4:#80FFC8 360ms ease;5:#80FFC8 360ms ease;6:#336650 360ms ease;7:#000000 360ms ease\n6:#80FFC8 1400ms pulse"),
            (unplugged(100), 2, "0:#00FF66 360ms ease;1:#00FF66 360ms ease"),
            (unplugged(15), 2, "0:#4C0B00 360ms ease;1:#000000 360ms ease"),
            (unplugged(15), 8, "0:#FF2600 360ms ease;1:#330800 360ms ease;2:#000000 360ms ease;3:#000000 360ms ease;4:#000000 360ms ease;5:#000000 360ms ease;6:#000000 360ms ease;7:#000000 360ms ease"),
            (
                BatterySnapshot { percent: 33, is_plugged: true, is_charging: true, adapter_voltage: 20.0, adapter_current: 3.34, full_charge_watts: 96.0, ..Default::default() },
                8,
                "0:#80FFC8 360ms ease;1:#80FFC8 360ms ease;2:#52A380 360ms ease;3:#000000 360ms ease;4:#000000 360ms ease;5:#000000 360ms ease;6:#000000 360ms ease;7:#000000 360ms ease\n2:#80FFC8 1029ms pulse",
            ),
            (
                BatterySnapshot { percent: 33, is_plugged: true, is_charging: true, adapter_voltage: 20.0, adapter_current: 3.34, full_charge_watts: 96.0, ..Default::default() },
                2,
                "0:#54A884 360ms ease;1:#000000 360ms ease\n0:#80FFC8 1029ms pulse",
            ),
            (
                BatterySnapshot { percent: 99, is_plugged: true, is_charged: true, ..Default::default() },
                2,
                "0:#00FF66 360ms ease;1:#00FF66 360ms ease",
            ),
            (
                BatterySnapshot { percent: 0, is_plugged: true, is_charging: true, ..Default::default() },
                2,
                "0:#000000 360ms ease;1:#000000 360ms ease\n0:#80FFC8 180ms pulse",
            ),
        ];
        for (snapshot, count, expected) in cases {
            let program = battery_program(&snapshot, count, None, 255);
            assert_eq!(program, expected, "{snapshot:?} x{count}");
            crate::devices::validate_program(&program).unwrap();
            assert!(!program.contains("repeat"));
        }
        assert_eq!(
            battery_program(&unplugged(57), 8, None, 128),
            "brightness 128\n0:#00FF66 360ms ease;1:#00FF66 360ms ease;2:#00FF66 360ms ease;3:#00FF66 360ms ease;4:#008F39 360ms ease;5:#000000 360ms ease;6:#000000 360ms ease;7:#000000 360ms ease"
        );
        assert_eq!(
            battery_program_with(&unplugged(57), 8, None, 0, 255),
            "0:#00FF66;1:#00FF66;2:#00FF66;3:#00FF66;4:#008F39;5:#000000;6:#000000;7:#000000"
        );
    }

    #[test]
    fn pulse_length_and_cadence() {
        for (ratio, ms, interval) in [
            (0.0, 180, 3.0),
            (0.25, 485, 2.475),
            (0.5, 790, 1.95),
            (0.7, 1034, 1.53),
            (1.0, 1400, 0.9),
        ] {
            assert_eq!(charging_pulse_length_ms(ratio), ms);
            assert!((charging_update_interval(ratio) - interval).abs() < 1e-12);
        }
        assert_eq!(charging_pulse_length_ms(7.0), 1400);
        let s = charging(50, 70, 140.0);
        assert!(!should_rewrite(&s, 1.9));
        assert!(should_rewrite(&s, 1.95));
        assert!(!should_rewrite(&unplugged(50), 100.0));
        let charged = BatterySnapshot {
            is_charged: true,
            ..charging(100, 70, 140.0)
        };
        assert!(!should_rewrite(&charged, 100.0));
    }

    #[test]
    fn scale_hex_passthrough() {
        assert_eq!(scale_hex_color("red", 0.5), "red");
        assert_eq!(scale_hex_color("#00ff66", 0.5), "#008033");
        assert_eq!(scale_hex_color("#FFFFFF", 2.0), "#FFFFFF");
    }

    #[test]
    fn product_watts_rules() {
        assert_eq!(
            full_charge_watts_for_product("MacBook Pro (14-inch, Nov 2023)", "Apple M3 Max"),
            96.0
        );
        assert_eq!(
            full_charge_watts_for_product("MacBook Pro (14-inch, Nov 2023)", "Apple M3"),
            70.0
        );
        assert_eq!(
            full_charge_watts_for_product("MacBook Pro (16-inch, 2021)", "Apple M1 Pro"),
            140.0
        );
        assert_eq!(
            full_charge_watts_for_product("MacBook Air (15-inch, M2, 2023)", "Apple M2"),
            100.0
        );
        assert_eq!(full_charge_watts_for_product("", ""), 100.0);
    }

    /// `test_battery_led_controller_animates_charging_on_cadence` and
    /// `test_battery_led_controller_skips_unchanged_static_level`.
    #[test]
    fn controller_cadence_and_static_dedupe() {
        let ok = |_: &Path, _: &str| -> std::io::Result<()> { Ok(()) };
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let dev = Device::from_path(Path::new("/tmp/fake/SidePulsePro"));

        let mut c = BatteryLedController::new(dev.clone());
        let s = charging(50, 70, 140.0);
        assert!(c.sync_with(&s, 255, at(0.0), ok).unwrap().is_some());
        assert_eq!(c.sync_with(&s, 255, at(0.5), ok), Ok(None));
        assert!(c.sync_with(&s, 255, at(2.0), ok).unwrap().is_some());

        let mut c = BatteryLedController::new(dev.clone());
        let s = unplugged(50);
        assert!(c.sync_with(&s, 255, at(0.0), ok).unwrap().is_some());
        assert_eq!(c.sync_with(&s, 255, at(10.0), ok), Ok(None));
        assert!(c
            .sync_with(&unplugged(51), 255, at(10.0), ok)
            .unwrap()
            .is_some());

        let mut c = BatteryLedController::new(dev);
        let calls = Cell::new(0);
        let fail = |_: &Path, _: &str| -> std::io::Result<()> {
            calls.set(calls.get() + 1);
            Err(std::io::Error::other("busy"))
        };
        assert!(c.sync_with(&s, 255, at(0.0), fail).is_err());
        assert_eq!(c.sync_with(&s, 255, at(5.0), fail), Err("busy".into()));
        assert_eq!(calls.get(), 1);
        assert!(c.sync_with(&s, 255, at(10.5), ok).unwrap().is_some());
    }

    #[test]
    fn power_change_preview() {
        let t0 = Instant::now();
        let mut p = PowerChangePreview::default();
        assert!(!p.observe(true, true, DEFAULT_POWER_CHANGE_PREVIEW, t0));
        assert!(!p.active(t0));
        assert!(p.observe(false, true, DEFAULT_POWER_CHANGE_PREVIEW, t0));
        assert!(p.active(t0 + Duration::from_secs(6)));
        assert!(!p.active(t0 + Duration::from_secs(7)));
        assert!(!p.observe(true, false, DEFAULT_POWER_CHANGE_PREVIEW, t0));
    }

    /// Non-destructive live read; run with `cargo test -p sidepulse-core -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_battery_read() {
        let t = Instant::now();
        let snapshot = read();
        println!("battery ({:?}): {snapshot:#?}", t.elapsed());
        println!("full-charge watts: {}", default_full_charge_watts());
    }
}
