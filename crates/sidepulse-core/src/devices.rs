//! SidePulse device discovery, LED program writing and agent LED programs
//! (`device_writer.py` and `led_status.py`).
//!
//! API:
//! - [`Device`] (`root`, `name`, `led_count`, `reason`), [`Device::from_path`], [`Device::target`].
//! - [`discover`] / [`discover_in`]: `discover_devices`.
//! - [`decode_simple_escapes`], [`validate_program`] ([`ProgramError`]), [`write_program`],
//!   [`write_program_to`].
//! - [`LedState`], [`led_state_for_mode`], [`agent_program`], [`rolling_program`],
//!   [`apply_brightness`], [`normalize_brightness`], [`brightness_percent`], [`py_round`].
//! - [`LedController`]: `AgentLedController` dedupe and error backoff, per device.
//!
//! Differences from Python: [`write_program_to`] does not create missing parent directories
//! (Python's `mkdir(parents=True)` could create a stray folder under `/Volumes` if a volume
//! unmounted between discovery and the write).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::models::AgentMode;
use crate::pyjson::splitlines;

/// `DEFAULT_FILE_NAME`.
pub const LED_FILE_NAME: &str = "LEDS.LED";
/// `KNOWN_LED_FILE_NAMES` (upper-cased).
pub const KNOWN_LED_FILE_NAMES: [&str; 1] = ["LEDS.LED"];
/// `MAX_LED_BYTES`.
pub const MAX_LED_BYTES: usize = 512;
/// `MAX_LED_LINES`.
pub const MAX_LED_LINES: usize = 20;
/// `MOUNT_ROOT`.
pub const MOUNT_ROOT: &str = "/Volumes";
/// `DEVICE_NAME_HINTS`.
pub const DEVICE_NAME_HINTS: [&str; 2] = ["sidepulsepro", "sidepulsedot"];
/// `AgentLedController(error_retry_seconds=10.0)`.
pub const ERROR_RETRY: Duration = Duration::from_secs(10);

pub const ASK_AMBER: &str = "#FF7A1A";
pub const ERROR_RED: &str = "#FF2600";
pub const WORKING_CYAN: &str = "#00E5FF";
pub const DONE_GREEN: &str = "#00FF66";
pub const IDLE_DIM: &str = "#020204";

/// Why a volume was treated as a device (`DeviceCandidate.reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryReason {
    /// `"contains LEDS.LED"`.
    ContainsLedFile,
    /// `"name matches device"`.
    NameMatches,
    /// Built from an explicit path.
    Explicit,
}

/// A mounted SidePulse Pro (8 LEDs) or SidePulse Dot (2 LEDs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// Volume root, e.g. `/Volumes/SidePulse`.
    pub root: PathBuf,
    /// Volume name.
    pub name: String,
    /// `led_count_for_target`: 2 for a Dot, otherwise 8.
    pub led_count: u8,
    pub reason: DiscoveryReason,
}

impl Device {
    /// `target_from_device_path`: accepts a volume root or a path to its `LEDS.LED`.
    pub fn from_path(path: &Path) -> Self {
        let root = if is_led_file(path) {
            path.parent().unwrap_or(Path::new("")).to_path_buf()
        } else {
            path.to_path_buf()
        };
        Self::with_reason(root, DiscoveryReason::Explicit)
    }

    fn with_reason(root: PathBuf, reason: DiscoveryReason) -> Self {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let led_count = led_count_for_name(&name);
        Self {
            root,
            name,
            led_count,
            reason,
        }
    }

    /// The `LEDS.LED` path.
    pub fn target(&self) -> PathBuf {
        self.root.join(LED_FILE_NAME)
    }
}

fn is_led_file(path: &Path) -> bool {
    path.file_name()
        .map(|n| KNOWN_LED_FILE_NAMES.contains(&n.to_string_lossy().to_uppercase().as_str()))
        .unwrap_or(false)
}

/// `normalized_device_name`: lower-cased alphanumerics only.
pub fn normalized_device_name(name: &str) -> String {
    name.chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// `is_device_name`.
pub fn is_device_name(name: &str) -> bool {
    let normalized = normalized_device_name(name);
    DEVICE_NAME_HINTS
        .iter()
        .any(|hint| normalized.contains(hint))
}

/// `led_count_for_target` applied to a volume name.
pub fn led_count_for_name(name: &str) -> u8 {
    if normalized_device_name(name).contains("sidepulsedot") {
        2
    } else {
        8
    }
}

/// `discover_devices()` under `/Volumes`.
pub fn discover() -> Vec<Device> {
    discover_in(Path::new(MOUNT_ROOT))
}

/// `discover_devices(mount_root=...)`: volumes sorted by lower-cased name, skipping
/// `.timemachine` and `Macintosh HD`; a volume counts if it contains `LEDS.LED` or its name
/// matches a device hint. Unreadable entries are skipped.
pub fn discover_in(mount_root: &Path) -> Vec<Device> {
    let Ok(entries) = fs::read_dir(mount_root) else {
        return Vec::new();
    };
    let mut volumes: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".timemachine" || name == "Macintosh HD" {
                return None;
            }
            let path = entry.path();
            // `Path.is_dir()` follows symlinks; errors count as "not a directory".
            fs::metadata(&path).ok().filter(|m| m.is_dir())?;
            Some((name, path))
        })
        .collect();
    volumes.sort_by_key(|(name, _)| name.to_lowercase());
    volumes
        .into_iter()
        .filter_map(|(name, path)| {
            if path.join(LED_FILE_NAME).exists() {
                Some(Device::with_reason(path, DiscoveryReason::ContainsLedFile))
            } else if is_device_name(&name) {
                Some(Device::with_reason(path, DiscoveryReason::NameMatches))
            } else {
                None
            }
        })
        .collect()
}

/// `decode_simple_escapes`: `\n`, `\r`, `\t` and `\\`; any other backslash is kept.
pub fn decode_simple_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            _ => {
                out.push('\\');
                continue;
            }
        }
        chars.next();
    }
    out
}

/// `DeviceWriteError` raised by `validate_led_text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProgramError {
    Empty,
    TooManyBytes(usize),
    TooManyLines(usize),
}

impl fmt::Display for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "LED program is empty."),
            Self::TooManyBytes(n) => write!(f, "LED program is {n} bytes; max is {MAX_LED_BYTES}."),
            Self::TooManyLines(n) => {
                write!(f, "LED program has {n} lines; max is {MAX_LED_LINES}.")
            }
        }
    }
}

impl std::error::Error for ProgramError {}

impl From<ProgramError> for io::Error {
    fn from(e: ProgramError) -> Self {
        io::Error::new(io::ErrorKind::InvalidInput, e)
    }
}

/// `validate_led_text`: non-empty, at most 512 UTF-8 bytes and 20 lines (Python `splitlines`).
pub fn validate_program(text: &str) -> Result<(), ProgramError> {
    if text.is_empty() {
        return Err(ProgramError::Empty);
    }
    if text.len() > MAX_LED_BYTES {
        return Err(ProgramError::TooManyBytes(text.len()));
    }
    let lines = splitlines(text).len().max(1);
    if lines > MAX_LED_LINES {
        return Err(ProgramError::TooManyLines(lines));
    }
    Ok(())
}

/// `write_led_program(text, device_path=device)`.
pub fn write_program(device: &Device, text: &str) -> io::Result<PathBuf> {
    let target = device.target();
    write_program_to(&target, text)?;
    Ok(target)
}

/// Decodes escapes, validates and writes `text` to `target` (truncating, like `write_text`).
pub fn write_program_to(target: &Path, text: &str) -> io::Result<()> {
    let program = decode_simple_escapes(text);
    validate_program(&program)?;
    fs::write(target, program.as_bytes())
}

/// `LedDisplayState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LedState {
    Idle,
    Working,
    Done,
    Ask,
    Error,
}

impl LedState {
    pub fn value(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Ask => "ask",
            Self::Error => "error",
        }
    }

    /// `LED_STATE_LABELS`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Working => "Working",
            Self::Done => "Done",
            Self::Ask => "Ask",
            Self::Error => "Error",
        }
    }
}

/// `display_state_for_mode`.
pub fn led_state_for_mode(mode: AgentMode) -> LedState {
    match mode {
        AgentMode::BlockedError => LedState::Error,
        AgentMode::WaitingForInput => LedState::Ask,
        AgentMode::Working | AgentMode::ToolRunning | AgentMode::LongTaskProgress => {
            LedState::Working
        }
        AgentMode::Completed => LedState::Done,
        AgentMode::IdleReady | AgentMode::Unknown => LedState::Idle,
    }
}

/// `program_for_display_state(state, led_count=..., brightness=...)`.
pub fn agent_program(state: LedState, led_count: u8, brightness: u8) -> String {
    let program = match state {
        LedState::Idle => format!("off\n{IDLE_DIM} 7.2s pulse\nrepeat"),
        LedState::Ask => format!("off\n{ASK_AMBER} 2.8s pulse\nrepeat"),
        LedState::Error => format!(
            "off 180ms cosine\n{ERROR_RED} 620ms pulse\noff 240ms ease-out\n\
             {ERROR_RED} 1.2s pulse\noff 1.1s ease-out\nrepeat"
        ),
        LedState::Done => format!("off 180ms cosine\n{DONE_GREEN} 3.6s pulse\nrepeat"),
        LedState::Working => rolling_program(WORKING_CYAN, led_count),
    };
    apply_brightness(&program, brightness)
}

/// `rolling_program(color, led_count=...)`: a pulse walking across 2..=8 LEDs.
pub fn rolling_program(color: &str, led_count: u8) -> String {
    let count = led_count.clamp(2, 8) as u32;
    let delay_ms = if count == 2 { 360 } else { 150 };
    let segments: Vec<String> = (0..count)
        .map(|i| format!("{i}:{color} 1600ms pulse {}ms", i * delay_ms))
        .collect();
    format!("off 320ms cosine\n{}\nrepeat", segments.join("; "))
}

/// Python `round()` for floats (round half to even).
pub fn py_round(value: f64) -> i64 {
    value.round_ties_even() as i64
}

/// `normalize_brightness`: round and clamp to 0..=255.
pub fn normalize_brightness(value: f64) -> u8 {
    if value.is_nan() {
        return 0;
    }
    py_round(value).clamp(0, 255) as u8
}

/// `brightness_percent`.
pub fn brightness_percent(value: u8) -> u8 {
    py_round(f64::from(value) / 255.0 * 100.0) as u8
}

/// `apply_brightness`: prefixes `brightness N` below full brightness.
pub fn apply_brightness(program: &str, brightness: u8) -> String {
    if brightness == 255 {
        program.to_string()
    } else {
        format!("brightness {brightness}\n{program}")
    }
}

/// Outcome of a controller sync: `Ok(Some(program))` written, `Ok(None)` unchanged, `Err` when the
/// write failed now or is still inside the retry back-off.
pub type SyncResult = Result<Option<String>, String>;

/// `AgentLedController` for one device: writes only when (state, brightness) changes, and after
/// a failed write retries the same state no sooner than [`ERROR_RETRY`].
#[derive(Debug)]
pub struct LedController {
    pub device: Device,
    pub error_retry: Duration,
    last: Option<(LedState, u8)>,
    last_error: Option<String>,
    last_attempt: Option<Instant>,
}

impl LedController {
    pub fn new(device: Device) -> Self {
        Self {
            device,
            error_retry: ERROR_RETRY,
            last: None,
            last_error: None,
            last_attempt: None,
        }
    }

    /// `reset()`: forces the next sync to write.
    pub fn reset(&mut self) {
        self.last = None;
        self.last_error = None;
        self.last_attempt = None;
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// `sync_mode(mode)` with the controller's brightness passed in.
    pub fn sync(&mut self, mode: AgentMode, brightness: u8, now: Instant) -> SyncResult {
        self.sync_with(mode, brightness, now, write_program_to)
    }

    fn sync_with(
        &mut self,
        mode: AgentMode,
        brightness: u8,
        now: Instant,
        write: impl FnOnce(&Path, &str) -> io::Result<()>,
    ) -> SyncResult {
        let state = led_state_for_mode(mode);
        let key = Some((state, brightness));
        if key == self.last {
            match &self.last_error {
                None => return Ok(None),
                Some(error) => {
                    let recent = self
                        .last_attempt
                        .is_some_and(|t| now.saturating_duration_since(t) < self.error_retry);
                    if recent {
                        return Err(error.clone());
                    }
                }
            }
        }
        self.last_attempt = Some(now);
        self.last = key;
        let program = agent_program(state, self.device.led_count, brightness);
        match write(&self.device.target(), &program) {
            Ok(()) => {
                self.last_error = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn working8() -> String {
        "off 320ms cosine\n0:#00E5FF 1600ms pulse 0ms; 1:#00E5FF 1600ms pulse 150ms; \
         2:#00E5FF 1600ms pulse 300ms; 3:#00E5FF 1600ms pulse 450ms; 4:#00E5FF 1600ms pulse 600ms; \
         5:#00E5FF 1600ms pulse 750ms; 6:#00E5FF 1600ms pulse 900ms; 7:#00E5FF 1600ms pulse 1050ms\n\
         repeat"
            .to_string()
    }

    /// Byte-exact against `program_for_display_state` output captured from the Python source.
    #[test]
    fn agent_programs_match_python() {
        let cases: [(LedState, u8, &str); 6] = [
            (LedState::Idle, 8, "off\n#020204 7.2s pulse\nrepeat"),
            (LedState::Ask, 2, "off\n#FF7A1A 2.8s pulse\nrepeat"),
            (
                LedState::Error,
                8,
                "off 180ms cosine\n#FF2600 620ms pulse\noff 240ms ease-out\n#FF2600 1.2s pulse\noff 1.1s ease-out\nrepeat",
            ),
            (LedState::Done, 8, "off 180ms cosine\n#00FF66 3.6s pulse\nrepeat"),
            (
                LedState::Working,
                2,
                "off 320ms cosine\n0:#00E5FF 1600ms pulse 0ms; 1:#00E5FF 1600ms pulse 360ms\nrepeat",
            ),
            (LedState::Working, 8, ""),
        ];
        for (state, count, expected) in cases {
            let expected = if expected.is_empty() {
                working8()
            } else {
                expected.to_string()
            };
            assert_eq!(
                agent_program(state, count, 255),
                expected,
                "{state:?}/{count}"
            );
            assert_eq!(
                agent_program(state, count, 128),
                format!("brightness 128\n{expected}"),
                "{state:?}/{count} dimmed"
            );
            validate_program(&agent_program(state, count, 1)).unwrap();
        }
        assert_eq!(
            rolling_program(WORKING_CYAN, 0),
            rolling_program(WORKING_CYAN, 2)
        );
        assert_eq!(rolling_program(WORKING_CYAN, 20), working8());
    }

    #[test]
    fn modes_map_to_states() {
        assert_eq!(
            led_state_for_mode(AgentMode::WaitingForInput),
            LedState::Ask
        );
        assert_eq!(led_state_for_mode(AgentMode::BlockedError), LedState::Error);
        assert_eq!(
            led_state_for_mode(AgentMode::ToolRunning),
            LedState::Working
        );
        assert_eq!(
            led_state_for_mode(AgentMode::LongTaskProgress),
            LedState::Working
        );
        assert_eq!(led_state_for_mode(AgentMode::Completed), LedState::Done);
        assert_eq!(led_state_for_mode(AgentMode::IdleReady), LedState::Idle);
        assert_eq!(led_state_for_mode(AgentMode::Unknown), LedState::Idle);
    }

    #[test]
    fn brightness_rounds_like_python() {
        assert_eq!(normalize_brightness(127.5), 128);
        assert_eq!(normalize_brightness(128.5), 128);
        assert_eq!(normalize_brightness(-3.0), 0);
        assert_eq!(normalize_brightness(999.0), 255);
        assert_eq!(brightness_percent(128), 50);
        assert_eq!(apply_brightness("off", 255), "off");
        assert_eq!(apply_brightness("off", 0), "brightness 0\noff");
    }

    #[test]
    fn escapes_and_limits() {
        assert_eq!(
            decode_simple_escapes(r"off\n#FF00FF pulse"),
            "off\n#FF00FF pulse"
        );
        assert_eq!(decode_simple_escapes(r"a\tb\rc\\d\xe\"), "a\tb\rc\\d\\xe\\");
        assert_eq!(validate_program(""), Err(ProgramError::Empty));
        assert_eq!(
            validate_program(&"x".repeat(513)),
            Err(ProgramError::TooManyBytes(513))
        );
        validate_program(&"x".repeat(512)).unwrap();
        validate_program(&vec!["off"; 20].join("\n")).unwrap();
        assert_eq!(
            validate_program(&vec!["off"; 21].join("\n")),
            Err(ProgramError::TooManyLines(21))
        );
        assert_eq!(validate_program("\n").map(|_| ()), Ok(()));
        assert_eq!(
            ProgramError::TooManyBytes(513).to_string(),
            "LED program is 513 bytes; max is 512."
        );
    }

    #[test]
    fn device_names_and_counts() {
        assert_eq!(
            Device::from_path(Path::new("/Volumes/SidePulseDot/LEDS.LED")).led_count,
            2
        );
        assert_eq!(
            Device::from_path(Path::new("/Volumes/SidePulsePro/LEDS.LED")).led_count,
            8
        );
        assert_eq!(
            Device::from_path(Path::new("/Volumes/SidePulse")).led_count,
            8
        );
        assert!(is_device_name("SidePulse Pro"));
        assert!(is_device_name("side-pulse_dot 2"));
        assert!(!is_device_name("SidePulse"));
        let d = Device::from_path(Path::new("/Volumes/SidePulseDot/leds.led"));
        assert_eq!(d.root, PathBuf::from("/Volumes/SidePulseDot"));
        assert_eq!(d.target(), PathBuf::from("/Volumes/SidePulseDot/LEDS.LED"));
    }

    #[test]
    fn discovery_in_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("SidePulseDot")).unwrap();
        fs::create_dir(root.join("SidePulse")).unwrap();
        fs::write(root.join("SidePulse").join("LEDS.LED"), "off").unwrap();
        fs::create_dir(root.join("USB Drive")).unwrap();
        fs::write(root.join("USB Drive").join("LEDS.TXT"), "off").unwrap();
        fs::create_dir(root.join(".timemachine")).unwrap();
        fs::write(root.join(".timemachine").join("LEDS.LED"), "off").unwrap();
        fs::write(root.join("sidepulsepro-file"), "not a dir").unwrap();
        fs::create_dir(root.join("Macintosh HD")).unwrap();
        fs::write(root.join("Macintosh HD").join("LEDS.LED"), "off").unwrap();

        let found = discover_in(root);
        let names: Vec<_> = found.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["SidePulse", "SidePulseDot"]);
        assert_eq!(found[0].reason, DiscoveryReason::ContainsLedFile);
        assert_eq!(found[0].led_count, 8);
        assert_eq!(found[1].reason, DiscoveryReason::NameMatches);
        assert_eq!(found[1].led_count, 2);
        assert_eq!(
            found[1].target(),
            root.join("SidePulseDot").join("LEDS.LED")
        );
        assert!(discover_in(&root.join("missing")).is_empty());
    }

    #[test]
    fn write_decodes_escapes_and_keeps_old_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dev_root = tmp.path().join("SidePulseDot");
        fs::create_dir(&dev_root).unwrap();
        fs::write(dev_root.join("LEDS.TXT"), "off").unwrap();
        let device = Device::from_path(&dev_root);
        let target = write_program(&device, r"off\n#FF00FF pulse").unwrap();
        assert_eq!(target, dev_root.join("LEDS.LED"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "off\n#FF00FF pulse");
        assert_eq!(
            fs::read_to_string(dev_root.join("LEDS.TXT")).unwrap(),
            "off"
        );
        assert!(write_program(&device, &"x".repeat(513)).is_err());
        // Missing volume: no directory is created.
        let gone = Device::from_path(&tmp.path().join("SidePulsePro"));
        assert!(write_program(&gone, "off").is_err());
        assert!(!tmp.path().join("SidePulsePro").exists());
    }

    /// `test_agent_led_controller_skips_unchanged_state` and `test_write_mode_to_leds_*`.
    #[test]
    fn controller_dedupes_by_state_and_brightness() {
        let tmp = tempfile::tempdir().unwrap();
        let dev_root = tmp.path().join("SidePulseDot");
        fs::create_dir(&dev_root).unwrap();
        let mut c = LedController::new(Device::from_path(&dev_root));
        let now = Instant::now();
        assert!(c.sync(AgentMode::Completed, 255, now).unwrap().is_some());
        assert_eq!(c.sync(AgentMode::Completed, 255, now), Ok(None));
        let third = c
            .sync(AgentMode::WaitingForInput, 255, now)
            .unwrap()
            .unwrap();
        assert!(third.contains("#FF7A1A 2.8s pulse"));
        assert_eq!(
            fs::read_to_string(dev_root.join("LEDS.LED")).unwrap(),
            third
        );
        assert_eq!(
            c.sync(AgentMode::Working, 255, now).unwrap().unwrap(),
            "off 320ms cosine\n0:#00E5FF 1600ms pulse 0ms; 1:#00E5FF 1600ms pulse 360ms\nrepeat"
        );
        // Same LED state from a different mode is not rewritten.
        assert_eq!(c.sync(AgentMode::ToolRunning, 255, now), Ok(None));
        assert_eq!(
            c.sync(AgentMode::Completed, 64, now).unwrap().unwrap(),
            "brightness 64\noff 180ms cosine\n#00FF66 3.6s pulse\nrepeat"
        );
        c.reset();
        assert!(c.sync(AgentMode::Completed, 64, now).unwrap().is_some());
    }

    #[test]
    fn controller_backs_off_after_error() {
        let mut c = LedController::new(Device::from_path(Path::new("/nonexistent/SidePulsePro")));
        let t0 = Instant::now();
        let calls = Cell::new(0);
        let failing = |_: &Path, _: &str| -> io::Result<()> {
            calls.set(calls.get() + 1);
            Err(io::Error::other("offline"))
        };
        assert_eq!(
            c.sync_with(AgentMode::Working, 255, t0, failing),
            Err("offline".into())
        );
        let t5 = t0 + Duration::from_secs(5);
        assert_eq!(
            c.sync_with(AgentMode::Working, 255, t5, failing),
            Err("offline".into())
        );
        assert_eq!(calls.get(), 1, "retry suppressed inside back-off");
        // A different state retries immediately.
        assert!(c.sync_with(AgentMode::Completed, 255, t5, failing).is_err());
        assert_eq!(calls.get(), 2);
        let t16 = t0 + Duration::from_secs(16);
        let ok = |_: &Path, _: &str| -> io::Result<()> { Ok(()) };
        assert!(c
            .sync_with(AgentMode::Completed, 255, t16, ok)
            .unwrap()
            .is_some());
        assert_eq!(c.sync_with(AgentMode::Completed, 255, t16, ok), Ok(None));
    }
}
