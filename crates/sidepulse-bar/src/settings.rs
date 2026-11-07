//! `settings.json`, shared with the Python SidePulse app (`settings.py`).
//!
//! The file is kept as a JSON value so keys this port doesn't use (Setup window state,
//! transcript monitoring, hook preferences) survive a save. Saved like Python:
//! `json.dumps(data, indent=2, sort_keys=True)`.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use sidepulse_core::providers::{expanduser, home_dir};

pub const SESSION_OPEN_CHOICES: [&str; 3] = ["app", "terminal", "vscode"];

pub fn default_config_dir() -> PathBuf {
    if let Some(xdg) = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return expanduser(&xdg).join("sidepulse").join("agent-monitor");
    }
    home_dir().join(".config/sidepulse/agent-monitor")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedDisplay {
    Agent,
    Battery,
}

impl LedDisplay {
    fn parse(v: Option<&Value>) -> Option<Self> {
        match v.and_then(Value::as_str) {
            Some("agent") => Some(Self::Agent),
            Some("battery") => Some(Self::Battery),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Battery => "battery",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosedLidPolicy {
    Never,
    Agents,
    Always,
}

impl ClosedLidPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Agents => "agents",
            Self::Always => "always",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "never" => Some(Self::Never),
            "agents" => Some(Self::Agents),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Animation {
    pub program: String,
    pub duration_seconds: f64,
}

#[derive(Clone, Debug)]
pub struct Settings {
    path: PathBuf,
    data: Map<String, Value>,
}

impl Settings {
    pub fn load_default() -> Self {
        Self::load(&default_config_dir().join("settings.json"))
    }

    pub fn load(path: &Path) -> Self {
        let data = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            data,
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&sorted(&Value::Object(self.data.clone())))
            .map_err(std::io::Error::other)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text + "\n")?;
        std::fs::rename(tmp, &self.path)
    }

    pub fn led_display(&self) -> LedDisplay {
        LedDisplay::parse(self.data.get("led_display")).unwrap_or(LedDisplay::Agent)
    }

    fn device_entry(&self, path: &str) -> Option<&Map<String, Value>> {
        self.data
            .get("devices")?
            .as_array()?
            .iter()
            .filter_map(Value::as_object)
            .find(|d| {
                d.get("path").and_then(Value::as_str) == Some(path)
                    || d.get("id").and_then(Value::as_str) == Some(path)
            })
    }

    pub fn device_display(&self, path: &str) -> LedDisplay {
        self.device_entry(path)
            .and_then(|d| LedDisplay::parse(d.get("led_display")))
            .unwrap_or_else(|| self.led_display())
    }

    pub fn device_brightness(&self, path: &str) -> u8 {
        self.device_entry(path)
            .and_then(|d| d.get("brightness"))
            .and_then(Value::as_i64)
            .map(|b| b.clamp(0, 255) as u8)
            .unwrap_or(255)
    }

    /// Remembered devices as (path, name).
    pub fn remembered_devices(&self) -> Vec<(String, String)> {
        let Some(list) = self.data.get("devices").and_then(Value::as_array) else {
            return Vec::new();
        };
        list.iter()
            .filter_map(Value::as_object)
            .filter_map(|d| {
                let path = d.get("path").and_then(Value::as_str)?;
                if path.starts_with("virtual:") {
                    return None;
                }
                let name = d.get("name").and_then(Value::as_str).unwrap_or(path);
                Some((path.to_string(), name.to_string()))
            })
            .collect()
    }

    fn devices_mut(&mut self) -> &mut Vec<Value> {
        let entry = self
            .data
            .entry("devices")
            .or_insert_with(|| Value::Array(Vec::new()));
        if !entry.is_array() {
            *entry = Value::Array(Vec::new());
        }
        entry.as_array_mut().expect("array")
    }

    fn device_entry_mut(&mut self, path: &str, name: &str) -> &mut Map<String, Value> {
        let default_display = self.led_display().as_str();
        let devices = self.devices_mut();
        let index = devices
            .iter()
            .position(|d| d.get("path").and_then(Value::as_str) == Some(path));
        let index = index.unwrap_or_else(|| {
            devices.push(json!({"id": path, "name": name, "path": path, "led_display": default_display, "brightness": 255}));
            devices.len() - 1
        });
        devices[index].as_object_mut().expect("object")
    }

    /// Auto-remember a connected device (Python saves connected devices into settings).
    /// Returns true when the settings changed.
    pub fn remember_device(&mut self, path: &str, name: &str) -> bool {
        if self.device_entry(path).is_some() {
            return false;
        }
        self.device_entry_mut(path, name);
        true
    }

    pub fn set_device_display(&mut self, path: &str, name: &str, display: LedDisplay) {
        self.device_entry_mut(path, name)
            .insert("led_display".into(), display.as_str().into());
    }

    pub fn set_device_brightness(&mut self, path: &str, name: &str, brightness: u8) {
        self.device_entry_mut(path, name)
            .insert("brightness".into(), brightness.into());
    }

    pub fn forget_device(&mut self, path: &str) {
        self.devices_mut()
            .retain(|d| d.get("path").and_then(Value::as_str) != Some(path));
    }

    pub fn closed_lid_policy(&self) -> ClosedLidPolicy {
        self.data
            .get("closed_lid_awake_policy")
            .and_then(Value::as_str)
            .and_then(ClosedLidPolicy::parse)
            .unwrap_or(ClosedLidPolicy::Never)
    }

    pub fn set_closed_lid_policy(&mut self, policy: ClosedLidPolicy) {
        self.data
            .insert("closed_lid_awake_policy".into(), policy.as_str().into());
    }

    fn animation(&self, key: &str, default_program: &str, default_duration: f64) -> Animation {
        let obj = self.data.get(key).and_then(Value::as_object);
        let program = obj
            .and_then(|o| o.get("program"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(default_program)
            .to_string();
        let duration_seconds = obj
            .and_then(|o| o.get("duration_seconds"))
            .and_then(Value::as_f64)
            .unwrap_or(default_duration)
            .clamp(0.1, 10.0);
        Animation {
            program,
            duration_seconds,
        }
    }

    pub fn lid_closed_animation(&self, default_program: &str) -> Animation {
        self.animation("lid_closed_animation", default_program, 0.9)
    }

    pub fn lid_open_animation(&self, default_program: &str) -> Animation {
        self.animation("lid_open_animation", default_program, 1.0)
    }

    fn battery_setting(&self, key: &str) -> Option<&Value> {
        self.data.get("battery_monitoring")?.as_object()?.get(key)
    }

    pub fn full_charge_watts(&self) -> Option<f64> {
        self.battery_setting("full_charge_watts")
            .and_then(Value::as_f64)
            .filter(|w| *w > 0.0)
    }

    pub fn show_on_power_change(&self) -> bool {
        self.battery_setting("show_on_power_change")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    pub fn power_change_preview_seconds(&self) -> f64 {
        self.battery_setting("power_change_preview_seconds")
            .and_then(Value::as_f64)
            .filter(|s| *s > 0.0)
            .unwrap_or(7.0)
    }

    /// `Settings.session_open_action(provider, origin)`.
    pub fn session_open_action(&self, provider: &str, origin: Option<&str>) -> Option<String> {
        let prefs = self.data.get("session_open_preferences")?.as_object()?;
        let valid = |k: &str| {
            prefs
                .get(k)
                .and_then(Value::as_str)
                .filter(|a| SESSION_OPEN_CHOICES.contains(a))
                .map(str::to_string)
        };
        let provider = provider.to_lowercase();
        if let Some(origin) = origin.filter(|o| !o.is_empty()) {
            let key = normalize_origin_key(origin);
            if let Some(a) = valid(&format!("origin:{provider}:{key}"))
                .or_else(|| valid(&format!("origin:{key}")))
            {
                return Some(a);
            }
        }
        valid(&provider)
    }
}

pub fn normalize_origin_key(origin: &str) -> String {
    static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
    let lowered = origin.trim().to_lowercase();
    let normalized = NON_ALNUM.replace_all(&lowered, "_");
    let normalized = normalized.trim_matches('_');
    if normalized.is_empty() {
        "unknown".into()
    } else {
        normalized.into()
    }
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), sorted(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
  "closed_lid_awake_policy": "agents",
  "devices": [{"brightness": 144, "id": "/Volumes/SidePulse", "led_display": "agent", "name": "SidePulse", "path": "/Volumes/SidePulse"}],
  "led_display": "agent",
  "session_open_preferences": {"claude": "terminal", "origin:codex:codex_ui": "app"},
  "setup_screen_completed": true,
  "zzz_unknown": {"keep": [1, 2]}
}"#;

    #[test]
    fn reads_python_settings_and_preserves_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, SAMPLE).unwrap();
        let mut s = Settings::load(&path);
        assert_eq!(s.device_brightness("/Volumes/SidePulse"), 144);
        assert_eq!(s.closed_lid_policy(), ClosedLidPolicy::Agents);
        assert_eq!(
            s.session_open_action("Claude", None).as_deref(),
            Some("terminal")
        );
        assert_eq!(
            s.session_open_action("codex", Some("Codex UI")).as_deref(),
            Some("app")
        );
        assert_eq!(s.session_open_action("codex", None), None);
        s.set_device_brightness("/Volumes/SidePulse", "SidePulse", 200);
        s.set_device_display("/Volumes/Other", "Other", LedDisplay::Battery);
        s.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"zzz_unknown\": {\n    \"keep\": [\n      1,\n      2\n    ]\n  }"));
        let again = Settings::load(&path);
        assert_eq!(again.device_brightness("/Volumes/SidePulse"), 200);
        assert_eq!(again.device_display("/Volumes/Other"), LedDisplay::Battery);
        assert_eq!(again.remembered_devices().len(), 2);
    }

    #[test]
    fn defaults_when_missing() {
        let s = Settings::load(Path::new("/nonexistent/settings.json"));
        assert_eq!(s.led_display(), LedDisplay::Agent);
        assert_eq!(s.device_brightness("/Volumes/X"), 255);
        assert!(s.show_on_power_change());
        assert_eq!(s.power_change_preview_seconds(), 7.0);
        assert_eq!(s.lid_open_animation("p").duration_seconds, 1.0);
    }
}
