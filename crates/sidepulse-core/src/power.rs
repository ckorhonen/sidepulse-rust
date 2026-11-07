//! Keep-awake power assertions and the SD-card keepalive touch (`keep_awake.py`).
//!
//! API:
//! - [`Assertion`]: RAII set of IOPM assertions. [`Assertion::caffeinate`] matches
//!   `/usr/bin/caffeinate -dimsu` (display, idle, disk and system sleep, plus one user-activity
//!   declaration). Dropping it releases every assertion.
//! - [`Hold`]: something that can be acquired and released; [`AssertionHold`] is the real one.
//! - [`KeepAwake`]: `KeepAwakeController` policy. `update(mode, now)` holds while agents work and
//!   keeps a 300 s grace hold after completion, a question or an error.
//! - [`KeepaliveToucher`]: touches `<volume>/keepalive` at most once per 60 s per path, off-thread.
//! - [`keepalive_file_for_target`], [`keepalive_targets`], [`format_duration`].
//!
//! Differences from Python: assertions are taken in-process with IOPMLib instead of spawning
//! `caffeinate -w <pid>` (they die with the process just the same), and the keepalive touch is a
//! direct `open(O_CREAT)` + `futimens` instead of `/usr/bin/touch`.

#[cfg(target_os = "macos")]
pub(crate) mod iokit;

use std::collections::{HashMap, HashSet};
use std::fs::{File, FileTimes};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::devices::{Device, KNOWN_LED_FILE_NAMES, MOUNT_ROOT};
use crate::models::AgentMode;

/// `AWAKE_GRACE_SECONDS`.
pub const AWAKE_GRACE: Duration = Duration::from_secs(300);
/// `SD_STATUS_READ_SECONDS`.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);
/// `KEEPALIVE_FILE_NAME`.
pub const KEEPALIVE_FILE_NAME: &str = "keepalive";
/// `STATUS_BAR_KEEPALIVE_VOLUME_NAMES` (status_bar.py), used when no device is mounted.
pub const FALLBACK_KEEPALIVE_VOLUMES: [&str; 2] = ["SidePulsePro", "SidePulseDot"];

/// IOPM assertion types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssertionKind {
    /// `-d`: `PreventUserIdleDisplaySleep`.
    PreventUserIdleDisplaySleep,
    /// `-i`: `PreventUserIdleSystemSleep`.
    PreventUserIdleSystemSleep,
    /// `-m`: `PreventDiskIdle`.
    PreventDiskIdle,
    /// `-s`: `PreventSystemSleep` (honored on AC power only, like caffeinate).
    PreventSystemSleep,
}

impl AssertionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreventUserIdleDisplaySleep => "PreventUserIdleDisplaySleep",
            Self::PreventUserIdleSystemSleep => "PreventUserIdleSystemSleep",
            Self::PreventDiskIdle => "PreventDiskIdle",
            Self::PreventSystemSleep => "PreventSystemSleep",
        }
    }
}

/// The `-dimsu` set used by `CAFFEINATE_COMMAND`.
pub const CAFFEINATE_DIMS: [AssertionKind; 4] = [
    AssertionKind::PreventUserIdleDisplaySleep,
    AssertionKind::PreventUserIdleSystemSleep,
    AssertionKind::PreventDiskIdle,
    AssertionKind::PreventSystemSleep,
];

/// RAII guard over one or more IOPM assertions; all are released on drop.
#[derive(Debug)]
pub struct Assertion {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    ids: Vec<u32>,
}

impl Assertion {
    /// Equivalent of `caffeinate -dimsu`.
    pub fn caffeinate(reason: &str) -> io::Result<Self> {
        Self::acquire(&CAFFEINATE_DIMS, reason, true)
    }

    /// Creates each assertion in `kinds`; with `declare_user_activity` also declares local user
    /// activity once (caffeinate `-u`), which wakes the display. Fails as a whole: anything
    /// created before an error is released.
    #[cfg(target_os = "macos")]
    pub fn acquire(
        kinds: &[AssertionKind],
        reason: &str,
        declare_user_activity: bool,
    ) -> io::Result<Self> {
        let mut guard = Self {
            ids: Vec::with_capacity(kinds.len() + 1),
        };
        for kind in kinds {
            let id = iokit::assertion_create(kind.as_str(), reason).map_err(|rc| {
                io::Error::other(format!(
                    "IOPMAssertionCreateWithName({}) failed: 0x{rc:08x}",
                    kind.as_str()
                ))
            })?;
            guard.ids.push(id);
        }
        if declare_user_activity {
            let id = iokit::declare_user_activity(reason).map_err(|rc| {
                io::Error::other(format!(
                    "IOPMAssertionDeclareUserActivity failed: 0x{rc:08x}"
                ))
            })?;
            guard.ids.push(id);
        }
        Ok(guard)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn acquire(
        _kinds: &[AssertionKind],
        _reason: &str,
        _declare_user_activity: bool,
    ) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "power assertions need macOS",
        ))
    }

    /// Number of live assertion IDs held by this guard.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

impl Drop for Assertion {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        for id in self.ids.drain(..) {
            iokit::assertion_release(id);
        }
    }
}

/// Something that keeps the machine awake while held (the `caffeinate` process in Python).
pub trait Hold {
    /// Acquires the hold if it is not already held.
    fn acquire(&mut self) -> Result<(), String>;
    /// Releases the hold if held.
    fn release(&mut self);
    /// `process_running()`.
    fn is_held(&self) -> bool;
}

/// A [`Hold`] backed by an [`Assertion::caffeinate`] guard.
#[derive(Debug)]
pub struct AssertionHold {
    reason: String,
    assertion: Option<Assertion>,
}

impl AssertionHold {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            assertion: None,
        }
    }
}

impl Hold for AssertionHold {
    fn acquire(&mut self) -> Result<(), String> {
        if self.assertion.is_none() {
            self.assertion = Some(Assertion::caffeinate(&self.reason).map_err(|e| e.to_string())?);
        }
        Ok(())
    }

    fn release(&mut self) {
        self.assertion = None;
    }

    fn is_held(&self) -> bool {
        self.assertion.is_some()
    }
}

/// Port of `KeepAwakeController` (the hold half; see [`KeepaliveToucher`] for the SD touch).
pub struct KeepAwake<H: Hold = AssertionHold> {
    pub enabled: bool,
    pub grace: Duration,
    hold: H,
    last_mode: Option<AgentMode>,
    /// `holding_requested`: the policy wants a hold (feeds the closed-lid "agents" policy).
    pub holding_requested: bool,
    grace_until: Option<Instant>,
    pub last_error: Option<String>,
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self::new(AssertionHold::new("SidePulse agents are working"))
    }
}

impl<H: Hold> KeepAwake<H> {
    pub fn new(hold: H) -> Self {
        Self {
            enabled: true,
            grace: AWAKE_GRACE,
            hold,
            last_mode: None,
            holding_requested: false,
            grace_until: None,
            last_error: None,
        }
    }

    pub fn hold(&self) -> &H {
        &self.hold
    }

    /// `set_enabled`.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if !enabled {
            self.release();
            self.holding_requested = false;
            self.grace_until = None;
        }
    }

    /// `update(mode, now=...)`: returns whether a hold is active afterwards.
    pub fn update(&mut self, mode: AgentMode, now: Instant) -> bool {
        let should_hold = self.should_hold_for_mode(mode, now);
        self.holding_requested = should_hold;
        self.last_mode = Some(mode);
        if !self.enabled || !should_hold {
            self.release();
            return false;
        }
        self.ensure_awake();
        self.hold.is_held()
    }

    /// `should_hold_for_mode`.
    fn should_hold_for_mode(&mut self, mode: AgentMode, now: Instant) -> bool {
        match mode {
            AgentMode::Working | AgentMode::ToolRunning | AgentMode::LongTaskProgress => {
                self.grace_until = None;
                true
            }
            AgentMode::Completed | AgentMode::WaitingForInput | AgentMode::BlockedError => {
                if self.last_mode != Some(mode) || self.grace_until.is_none() {
                    self.grace_until = Some(now + self.grace);
                }
                self.grace_until.is_some_and(|until| now < until)
            }
            _ => self.grace_until.is_some_and(|until| now < until),
        }
    }

    fn ensure_awake(&mut self) {
        if self.hold.is_held() {
            return;
        }
        match self.hold.acquire() {
            Ok(()) => self.last_error = None,
            Err(e) => self.last_error = Some(e),
        }
    }

    /// `release()`.
    pub fn release(&mut self) {
        self.hold.release();
    }

    /// `process_running()`.
    pub fn is_held(&self) -> bool {
        self.hold.is_held()
    }

    /// `detail(now=...)`: the menu status line.
    pub fn detail(&self, now: Instant) -> String {
        if !self.enabled {
            return "Keep awake disabled".into();
        }
        if let Some(error) = &self.last_error {
            return format!("Keep awake error: {error}");
        }
        if let Some(until) = self.grace_until.filter(|until| now < *until) {
            let remaining = (until - now).as_secs();
            return format!("Keep awake grace: {}", format_duration(remaining));
        }
        if self.hold.is_held() {
            return "Keep awake active".into();
        }
        "Keep awake standby".into()
    }
}

/// `format_duration(seconds)`: `"4m05s"` or `"42s"`.
pub fn format_duration(seconds: u64) -> String {
    let (minutes, rest) = (seconds / 60, seconds % 60);
    if minutes > 0 {
        format!("{minutes}m{rest:02}s")
    } else {
        format!("{rest}s")
    }
}

/// `keepalive_file_for_target`: maps a device root or one of its known files to `<root>/keepalive`.
pub fn keepalive_file_for_target(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_uppercase())
        .unwrap_or_default();
    let known = KNOWN_LED_FILE_NAMES.contains(&name.as_str())
        || name == KEEPALIVE_FILE_NAME.to_uppercase()
        || name == "STATUS.TXT";
    if known {
        target
            .parent()
            .unwrap_or(Path::new(""))
            .join(KEEPALIVE_FILE_NAME)
    } else {
        target.join(KEEPALIVE_FILE_NAME)
    }
}

/// `status_keepalive_targets()` (status_bar.py): every device's LED file, or the fallback
/// `/Volumes/SidePulsePro/keepalive` and `/Volumes/SidePulseDot/keepalive` when none is found.
pub fn keepalive_targets(devices: &[Device]) -> Vec<PathBuf> {
    if !devices.is_empty() {
        return devices.iter().map(Device::target).collect();
    }
    FALLBACK_KEEPALIVE_VOLUMES
        .iter()
        .map(|name| Path::new(MOUNT_ROOT).join(name).join(KEEPALIVE_FILE_NAME))
        .collect()
}

/// `touch_keepalive_file` without `/usr/bin/touch`: create if missing (never truncate or write
/// data) and set access and modification times to now. Fails with `NotFound` when the volume is
/// not mounted, so nothing is ever created under `/Volumes` itself.
pub fn touch(path: &Path) -> io::Result<()> {
    let file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let now = SystemTime::now();
    file.set_times(FileTimes::new().set_accessed(now).set_modified(now))
}

type TouchFn = fn(&Path) -> io::Result<()>;

/// `poke_status_file` from `KeepAwakeController`: rate-limited keepalive touches.
pub struct KeepaliveToucher {
    pub interval: Duration,
    /// Run touches on a background thread (`status_read_async`) so a slow SD card never blocks.
    pub asynchronous: bool,
    toucher: TouchFn,
    last_by_path: HashMap<PathBuf, Instant>,
    in_flight: Arc<Mutex<HashSet<PathBuf>>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl Default for KeepaliveToucher {
    fn default() -> Self {
        Self::new(touch, true)
    }
}

impl KeepaliveToucher {
    pub fn new(toucher: TouchFn, asynchronous: bool) -> Self {
        Self {
            interval: KEEPALIVE_INTERVAL,
            asynchronous,
            toucher,
            last_by_path: HashMap::new(),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            last_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Touches the keepalive file for `target` if the interval has elapsed. Returns the path when
    /// a touch was started (async) or succeeded (sync), like `poke_status_file`.
    pub fn poke(&mut self, target: &Path, now: Instant) -> Option<PathBuf> {
        let path = keepalive_file_for_target(target);
        if let Some(last) = self.last_by_path.get(&path) {
            if now.saturating_duration_since(*last) < self.interval {
                return None;
            }
        }
        self.last_by_path.insert(path.clone(), now);
        if self.asynchronous {
            if !lock(&self.in_flight).insert(path.clone()) {
                return None;
            }
            let (toucher, in_flight, last_error) = (
                self.toucher,
                Arc::clone(&self.in_flight),
                Arc::clone(&self.last_error),
            );
            let worker_path = path.clone();
            let spawned = std::thread::Builder::new()
                .name("sidepulse-keepalive".into())
                .spawn(move || {
                    let _ = run_touch(toucher, &worker_path, &last_error);
                    lock(&in_flight).remove(&worker_path);
                });
            if spawned.is_err() {
                lock(&self.in_flight).remove(&path);
                return None;
            }
            return Some(path);
        }
        run_touch(self.toucher, &path, &self.last_error).then_some(path)
    }

    /// `last_status_error`.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.last_error).clone()
    }

    /// Forget timing state (`set_enabled(False)`).
    pub fn reset(&mut self) {
        self.last_by_path.clear();
        *lock(&self.last_error) = None;
    }
}

fn run_touch(toucher: TouchFn, path: &Path, last_error: &Mutex<Option<String>>) -> bool {
    match toucher(path) {
        Ok(()) => {
            *lock(last_error) = None;
            true
        }
        Err(e) => {
            *lock(last_error) = Some(format!("{}: {e}", path.display()));
            false
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeHold {
        held: bool,
        acquires: usize,
        releases: usize,
    }

    impl Hold for FakeHold {
        fn acquire(&mut self) -> Result<(), String> {
            if !self.held {
                self.held = true;
                self.acquires += 1;
            }
            Ok(())
        }
        fn release(&mut self) {
            if self.held {
                self.held = false;
                self.releases += 1;
            }
        }
        fn is_held(&self) -> bool {
            self.held
        }
    }

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    /// `test_keep_awake_holds_working_then_graces_done`.
    #[test]
    fn holds_working_then_graces_done() {
        let t0 = Instant::now();
        let mut ka = KeepAwake::new(FakeHold::default());
        assert!(ka.update(AgentMode::Working, at(t0, 100)));
        assert_eq!(ka.hold().acquires, 1);
        assert!(ka.update(AgentMode::Completed, at(t0, 110)));
        assert!(ka.detail(at(t0, 110)).contains("grace"));
        assert_eq!(ka.detail(at(t0, 110)), "Keep awake grace: 5m00s");
        assert!(ka.update(AgentMode::IdleReady, at(t0, 200)));
        assert!(ka.is_held());
        assert!(!ka.update(AgentMode::IdleReady, at(t0, 411)));
        assert!(!ka.is_held());
        assert_eq!(ka.hold().releases, 1);
        assert_eq!(ka.hold().acquires, 1);
        assert_eq!(ka.detail(at(t0, 411)), "Keep awake standby");
    }

    /// `test_keep_awake_ask_grace_expires_without_refresh_extension`.
    #[test]
    fn ask_grace_expires_without_extension() {
        let t0 = Instant::now();
        let mut ka = KeepAwake::new(FakeHold::default());
        assert!(ka.update(AgentMode::WaitingForInput, at(t0, 100)));
        assert!(ka.update(AgentMode::WaitingForInput, at(t0, 350)));
        assert!(!ka.update(AgentMode::WaitingForInput, at(t0, 401)));
        assert_eq!(ka.hold().acquires, 1);
        assert_eq!(ka.hold().releases, 1);
    }

    #[test]
    fn mode_change_restarts_grace_and_disable_clears() {
        let t0 = Instant::now();
        let mut ka = KeepAwake::new(FakeHold::default());
        assert!(ka.update(AgentMode::Completed, at(t0, 0)));
        assert!(ka.update(AgentMode::BlockedError, at(t0, 290)));
        assert!(ka.update(AgentMode::BlockedError, at(t0, 589)));
        assert!(!ka.update(AgentMode::BlockedError, at(t0, 590)));
        assert!(!ka.update(AgentMode::Unknown, at(t0, 600)));
        assert!(ka.update(AgentMode::ToolRunning, at(t0, 601)));
        ka.set_enabled(false);
        assert!(!ka.is_held());
        assert!(!ka.update(AgentMode::Working, at(t0, 602)));
        assert!(ka.holding_requested);
        assert_eq!(ka.detail(at(t0, 602)), "Keep awake disabled");
    }

    #[test]
    fn format_duration_matches_python() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(65), "1m05s");
        assert_eq!(format_duration(300), "5m00s");
    }

    #[test]
    fn keepalive_file_mapping() {
        let dev = Path::new("/Volumes/SidePulsePro");
        let expected = dev.join("keepalive");
        assert_eq!(keepalive_file_for_target(&dev.join("LEDS.LED")), expected);
        assert_eq!(keepalive_file_for_target(&dev.join("leds.led")), expected);
        assert_eq!(keepalive_file_for_target(&dev.join("STATUS.TXT")), expected);
        assert_eq!(keepalive_file_for_target(&dev.join("keepalive")), expected);
        assert_eq!(keepalive_file_for_target(dev), expected);
    }

    #[test]
    fn keepalive_targets_fall_back_to_known_volumes() {
        assert_eq!(
            keepalive_targets(&[]),
            vec![
                PathBuf::from("/Volumes/SidePulsePro/keepalive"),
                PathBuf::from("/Volumes/SidePulseDot/keepalive"),
            ]
        );
        let dev = Device::from_path(Path::new("/tmp/x/SidePulseDot"));
        assert_eq!(
            keepalive_targets(&[dev]),
            vec![PathBuf::from("/tmp/x/SidePulseDot/LEDS.LED")]
        );
    }

    static TOUCHES: AtomicUsize = AtomicUsize::new(0);
    fn counting_touch(_p: &Path) -> io::Result<()> {
        TOUCHES.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// `test_keep_awake_touches_keepalive_file_once_per_interval`.
    #[test]
    fn touches_once_per_interval() {
        let t0 = Instant::now();
        let dev = PathBuf::from("/tmp/fake/SidePulsePro");
        let status = dev.join("keepalive");
        let mut toucher = KeepaliveToucher::new(counting_touch, false);
        assert_eq!(
            toucher.poke(&dev.join("LEDS.LED"), at(t0, 0)),
            Some(status.clone())
        );
        assert_eq!(toucher.poke(&dev.join("LEDS.LED"), at(t0, 30)), None);
        assert_eq!(
            toucher.poke(&dev.join("LEDS.LED"), at(t0, 61)),
            Some(status)
        );
        assert_eq!(TOUCHES.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn touch_creates_and_updates_without_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keepalive");
        touch(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        std::fs::write(&path, b"x").unwrap();
        touch(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        // A missing volume is an error, never a created directory.
        let missing = dir.path().join("NotMounted").join("keepalive");
        assert!(touch(&missing).is_err());
        assert!(!dir.path().join("NotMounted").exists());
    }

    #[test]
    fn async_touch_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut toucher = KeepaliveToucher::default();
        let missing = dir.path().join("Gone");
        assert!(toucher.poke(&missing, Instant::now()).is_some());
        for _ in 0..200 {
            if toucher.last_error().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(toucher.last_error().is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn assertion_guard_acquires_and_releases() {
        // Display/idle assertions only (no user-activity declaration, which would wake the screen).
        let guard = Assertion::acquire(
            &[AssertionKind::PreventUserIdleSystemSleep],
            "sidepulse-core test",
            false,
        )
        .unwrap();
        assert_eq!(guard.len(), 1);
        drop(guard);
    }
}
