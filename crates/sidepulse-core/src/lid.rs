//! Lid (clamshell) state, lid animations and the closed-lid awake policy (`lid_sleep.py`, plus the
//! default lid animations from `settings.py`).
//!
//! API:
//! - [`clamshell_closed`] / [`sleep_disabled`]: `AppleClamshellState` / `SleepDisabled` read from
//!   `IOPMrootDomain` natively (`None` when unavailable, e.g. desktops or non-macOS).
//! - [`DEFAULT_CLOSED_PROGRAM`], [`DEFAULT_OPEN_PROGRAM`] (+ durations), [`default_closed_program`],
//!   [`default_open_program`], [`lid_animation_program`] (`program_for_lid_animation`).
//! - [`ClosedLidPolicy`], [`closed_lid_awake_should_hold`].
//! - [`ClosedLidAwake`]: `ClosedLidAwakeController`, generic over a [`Hold`] and a [`SleepControl`];
//!   [`PmsetSleepControl`] runs `sudo -n /usr/bin/pmset -a disablesleep 0|1`.
//! - [`sleep_helper_installed`], [`sleep_helper_sudoers_rule`], [`SLEEP_HELPER_SUDOERS_PATH`].
//!
//! Differences from Python: the lid and `SleepDisabled` state are IOKit property reads instead of
//! `ioreg` subprocesses every second, and the user-mode hold is an in-process IOPM assertion
//! instead of `caffeinate -dimsu -w <pid>`. The install hint names `sidepulse` without resolving
//! it on `PATH`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::devices::{apply_brightness, decode_simple_escapes, validate_program, ProgramError};
use crate::power::{AssertionHold, Hold};

/// `SLEEP_HELPER_SUDOERS_PATH`.
pub const SLEEP_HELPER_SUDOERS_PATH: &str = "/etc/sudoers.d/sidepulse-disablesleep";
/// `LID_POLL_SECONDS`.
pub const LID_POLL: Duration = Duration::from_secs(1);

/// `DEFAULT_LID_CLOSED_ANIMATION_PROGRAM`.
pub const DEFAULT_CLOSED_PROGRAM: &str = "off 90ms cosine\n\
0:#FF7A00 180ms ease; 7:#FF7A00 180ms ease; 1:#FF7A00 180ms ease 80ms; 6:#FF7A00 180ms ease 80ms\n\
2:#FF4A00 180ms ease; 5:#FF4A00 180ms ease; 3:#FF3000 180ms ease 80ms; 4:#FF3000 180ms ease 80ms\n\
off 360ms ease-out";
/// `DEFAULT_LID_OPEN_ANIMATION_PROGRAM`.
pub const DEFAULT_OPEN_PROGRAM: &str = "off 90ms cosine\n\
3:#00E5FF 180ms ease; 4:#00E5FF 180ms ease; 2:#00E5FF 180ms ease 80ms; 5:#00E5FF 180ms ease 80ms\n\
1:#00FFB0 180ms ease; 6:#00FFB0 180ms ease; 0:#00FF66 180ms ease 80ms; 7:#00FF66 180ms ease 80ms\n\
#00FF66 220ms ease\n\
off 320ms ease-out";
/// `DEFAULT_LID_CLOSED_ANIMATION_SECONDS`.
pub const DEFAULT_CLOSED_DURATION: Duration = Duration::from_millis(900);
/// `DEFAULT_LID_OPEN_ANIMATION_SECONDS`.
pub const DEFAULT_OPEN_DURATION: Duration = Duration::from_millis(1000);

/// `True` when the lid is closed, `None` if the state cannot be read.
#[cfg(target_os = "macos")]
pub fn clamshell_closed() -> Option<bool> {
    root_domain_bool("AppleClamshellState")
}

/// `read_sleep_disabled()`: whether `pmset disablesleep` is in effect.
#[cfg(target_os = "macos")]
pub fn sleep_disabled() -> Option<bool> {
    root_domain_bool("SleepDisabled")
}

#[cfg(target_os = "macos")]
fn root_domain_bool(key: &str) -> Option<bool> {
    use crate::power::iokit::{to_json, IoObject};
    let root = IoObject::matching_service("IOPMrootDomain")?;
    let value = root.property(key)?;
    match to_json(value.as_ptr()) {
        serde_json::Value::Bool(b) => Some(b),
        serde_json::Value::Number(n) => n.as_i64().map(|v| v != 0),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn clamshell_closed() -> Option<bool> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn sleep_disabled() -> Option<bool> {
    None
}

/// `default_lid_animation(LID_ANIMATION_CLOSED).program` at the given brightness.
pub fn default_closed_program(brightness: u8) -> String {
    apply_brightness(DEFAULT_CLOSED_PROGRAM, brightness)
}

/// `default_lid_animation(LID_ANIMATION_OPEN).program` at the given brightness.
pub fn default_open_program(brightness: u8) -> String {
    apply_brightness(DEFAULT_OPEN_PROGRAM, brightness)
}

/// `program_for_lid_animation` (status_bar.py): decode escapes, validate both the program and its
/// dimmed form (`validate_lid_animation`), then apply brightness.
pub fn lid_animation_program(program: &str, brightness: u8) -> Result<String, ProgramError> {
    let decoded = decode_simple_escapes(program);
    validate_program(&decoded)?;
    validate_program(&apply_brightness(&decoded, 1))?;
    Ok(apply_brightness(&decoded, brightness))
}

/// `CLOSED_LID_AWAKE_CHOICES`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClosedLidPolicy {
    #[default]
    Never,
    Agents,
    Always,
}

impl ClosedLidPolicy {
    pub fn value(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Agents => "agents",
            Self::Always => "always",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "never" => Some(Self::Never),
            "agents" => Some(Self::Agents),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

/// `closed_lid_awake_should_hold`.
pub fn closed_lid_awake_should_hold(policy: ClosedLidPolicy, agents_active: bool) -> bool {
    match policy {
        ClosedLidPolicy::Always => true,
        ClosedLidPolicy::Agents => agents_active,
        ClosedLidPolicy::Never => false,
    }
}

/// The `pmset disablesleep` side of the closed-lid controller.
pub trait SleepControl {
    /// `sleep_disabled_reader`.
    fn read(&mut self) -> Result<Option<bool>, String>;
    /// `sleep_disabled_setter`.
    fn set(&mut self, disabled: bool) -> Result<(), String>;
}

/// Real [`SleepControl`]: IOKit read, non-interactive `sudo -n pmset` write.
#[derive(Debug, Default)]
pub struct PmsetSleepControl;

impl SleepControl for PmsetSleepControl {
    fn read(&mut self) -> Result<Option<bool>, String> {
        Ok(sleep_disabled())
    }

    fn set(&mut self, disabled: bool) -> Result<(), String> {
        run_sudo_pmset_disablesleep(disabled)
    }
}

/// `sleep_helper_installed()`.
pub fn sleep_helper_installed() -> bool {
    Path::new(SLEEP_HELPER_SUDOERS_PATH).exists()
}

/// `sleep_helper_install_command()`.
pub fn sleep_helper_install_command() -> String {
    "sudo sidepulse status-bar install-sleep-helper".into()
}

/// `sleep_helper_sudoers_rule(user)`: `None` for a user name sudoers could misparse.
pub fn sleep_helper_sudoers_rule(user: &str) -> Option<String> {
    let valid = !user.is_empty()
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    valid.then(|| {
        format!(
            "{user} ALL=(root) NOPASSWD: /usr/bin/pmset -a disablesleep 0, /usr/bin/pmset -a disablesleep 1\n"
        )
    })
}

/// `run_sudo_pmset_disablesleep`: never prompts (`sudo -n`), gives up after 5 s.
pub fn run_sudo_pmset_disablesleep(disabled: bool) -> Result<(), String> {
    let value = if disabled { "1" } else { "0" };
    let mut child = Command::new("/usr/bin/sudo")
        .args(["-n", "/usr/bin/pmset", "-a", "disablesleep", value])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "Lid-closed awake needs one-time setup: {} ({e})",
                sleep_helper_install_command()
            )
        })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    if status.is_some_and(|s| s.success()) {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stderr);
    }
    let detail = stderr
        .trim()
        .lines()
        .next()
        .map(|l| format!(" ({l})"))
        .unwrap_or_default();
    Err(format!(
        "Lid-closed awake needs one-time setup: {}{detail}",
        sleep_helper_install_command()
    ))
}

/// `ClosedLidAwakeController`.
pub struct ClosedLidAwake<H: Hold = AssertionHold, S: SleepControl = PmsetSleepControl> {
    hold: H,
    sleep: S,
    /// `use_system_disable`; callers pass [`sleep_helper_installed`].
    use_system_disable: bool,
    /// `changed_system_disable`: we turned `disablesleep` on and must turn it off again.
    pub changed_system_disable: bool,
    system_disable_attempted: bool,
    pub last_error: Option<String>,
    pub last_policy: ClosedLidPolicy,
    pub last_requested: bool,
}

impl Default for ClosedLidAwake {
    fn default() -> Self {
        Self::new(
            AssertionHold::new("SidePulse closed-lid awake"),
            PmsetSleepControl,
            sleep_helper_installed(),
        )
    }
}

impl<H: Hold, S: SleepControl> ClosedLidAwake<H, S> {
    pub fn new(hold: H, sleep: S, use_system_disable: bool) -> Self {
        Self {
            hold,
            sleep,
            use_system_disable,
            changed_system_disable: false,
            system_disable_attempted: false,
            last_error: None,
            last_policy: ClosedLidPolicy::Never,
            last_requested: false,
        }
    }

    pub fn hold(&self) -> &H {
        &self.hold
    }

    pub fn sleep_control(&self) -> &S {
        &self.sleep
    }

    /// `set_use_system_disable`.
    pub fn set_use_system_disable(&mut self, enabled: bool) {
        if self.use_system_disable == enabled {
            return;
        }
        self.use_system_disable = enabled;
        self.system_disable_attempted = false;
        if !enabled {
            let mut errors = Vec::new();
            self.release_system_disable(&mut errors);
            self.last_error = join_errors(errors);
        }
    }

    /// `update(policy, agents_active=...)`: returns [`Self::active`].
    pub fn update(&mut self, policy: ClosedLidPolicy, agents_active: bool) -> bool {
        let should_hold = closed_lid_awake_should_hold(policy, agents_active);
        if policy != self.last_policy {
            self.system_disable_attempted = false;
        }
        self.last_policy = policy;
        self.last_requested = should_hold;
        if should_hold {
            self.ensure_awake();
        } else {
            self.release();
        }
        self.active()
    }

    fn ensure_awake(&mut self) {
        let mut errors = Vec::new();
        if self.use_system_disable && !self.changed_system_disable && !self.system_disable_attempted
        {
            self.system_disable_attempted = true;
            // Only touch the setting when it is not already on, so we never revert someone
            // else's `disablesleep 1`.
            let result = self.sleep.read().and_then(|already| match already {
                Some(true) => Ok(()),
                _ => self
                    .sleep
                    .set(true)
                    .map(|()| self.changed_system_disable = true),
            });
            if let Err(e) = result {
                errors.push(format!("disablesleep: {e}"));
            }
        }
        if !self.hold.is_held() {
            if let Err(e) = self.hold.acquire() {
                errors.push(format!("caffeinate: {e}"));
            }
        }
        self.last_error = join_errors(errors);
    }

    /// `release()`.
    pub fn release(&mut self) {
        self.hold.release();
        let mut errors = Vec::new();
        self.release_system_disable(&mut errors);
        self.last_error = join_errors(errors);
    }

    fn release_system_disable(&mut self, errors: &mut Vec<String>) {
        if self.changed_system_disable {
            match self.sleep.set(false) {
                Ok(()) => {
                    self.changed_system_disable = false;
                    self.system_disable_attempted = false;
                }
                Err(e) => errors.push(format!("disablesleep: {e}")),
            }
        } else {
            self.system_disable_attempted = false;
        }
    }

    /// `active()`.
    pub fn active(&self) -> bool {
        self.hold.is_held() || self.changed_system_disable
    }
}

fn join_errors(errors: Vec<String>) -> Option<String> {
    (!errors.is_empty()).then(|| errors.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeHold {
        held: bool,
        acquires: usize,
        releases: usize,
    }

    impl Hold for FakeHold {
        fn acquire(&mut self) -> Result<(), String> {
            self.held = true;
            self.acquires += 1;
            Ok(())
        }
        fn release(&mut self) {
            if self.held {
                self.releases += 1;
            }
            self.held = false;
        }
        fn is_held(&self) -> bool {
            self.held
        }
    }

    struct FakeSleep {
        current: Option<bool>,
        calls: Vec<bool>,
    }

    impl SleepControl for FakeSleep {
        fn read(&mut self) -> Result<Option<bool>, String> {
            Ok(self.current)
        }
        fn set(&mut self, disabled: bool) -> Result<(), String> {
            self.calls.push(disabled);
            Ok(())
        }
    }

    fn sleep(current: Option<bool>) -> FakeSleep {
        FakeSleep {
            current,
            calls: Vec::new(),
        }
    }

    #[test]
    fn default_programs_match_python() {
        assert_eq!(
            default_closed_program(128),
            "brightness 128\noff 90ms cosine\n0:#FF7A00 180ms ease; 7:#FF7A00 180ms ease; 1:#FF7A00 180ms ease 80ms; 6:#FF7A00 180ms ease 80ms\n2:#FF4A00 180ms ease; 5:#FF4A00 180ms ease; 3:#FF3000 180ms ease 80ms; 4:#FF3000 180ms ease 80ms\noff 360ms ease-out"
        );
        assert_eq!(
            default_open_program(255),
            "off 90ms cosine\n3:#00E5FF 180ms ease; 4:#00E5FF 180ms ease; 2:#00E5FF 180ms ease 80ms; 5:#00E5FF 180ms ease 80ms\n1:#00FFB0 180ms ease; 6:#00FFB0 180ms ease; 0:#00FF66 180ms ease 80ms; 7:#00FF66 180ms ease 80ms\n#00FF66 220ms ease\noff 320ms ease-out"
        );
        /* `test_lid_animation_program_uses_device_brightness` */
        let p = lid_animation_program(DEFAULT_CLOSED_PROGRAM, 128).unwrap();
        assert!(p.starts_with("brightness 128\n"));
        validate_program(&p).unwrap();
        assert_eq!(
            lid_animation_program(r"off\nrepeat", 255).unwrap(),
            "off\nrepeat"
        );
        assert!(
            lid_animation_program(&vec!["off"; 20].join("\n"), 255).is_err(),
            "dimmed form is 21 lines"
        );
    }

    #[test]
    fn policy_decisions() {
        assert!(!closed_lid_awake_should_hold(ClosedLidPolicy::Never, true));
        assert!(!closed_lid_awake_should_hold(
            ClosedLidPolicy::Agents,
            false
        ));
        assert!(closed_lid_awake_should_hold(ClosedLidPolicy::Agents, true));
        assert!(closed_lid_awake_should_hold(ClosedLidPolicy::Always, false));
        for p in [
            ClosedLidPolicy::Never,
            ClosedLidPolicy::Agents,
            ClosedLidPolicy::Always,
        ] {
            assert_eq!(ClosedLidPolicy::parse(p.value()), Some(p));
        }
        assert_eq!(ClosedLidPolicy::parse("sometimes"), None);
    }

    /// `test_closed_lid_awake_controller_sets_and_restores_system_disable`.
    #[test]
    fn sets_and_restores_system_disable() {
        let mut c = ClosedLidAwake::new(FakeHold::default(), sleep(Some(false)), true);
        assert!(c.update(ClosedLidPolicy::Always, false));
        assert_eq!(c.sleep_control().calls, [true]);
        assert!(c.changed_system_disable);
        assert_eq!(c.hold().acquires, 1);
        c.update(ClosedLidPolicy::Always, false);
        assert_eq!(c.sleep_control().calls, [true]);
        assert_eq!(c.hold().acquires, 1);
        assert!(!c.update(ClosedLidPolicy::Never, false));
        assert_eq!(c.sleep_control().calls, [true, false]);
        assert_eq!(c.hold().releases, 1);
    }

    /// `test_closed_lid_awake_controller_defaults_to_user_mode_only`.
    #[test]
    fn user_mode_only_without_helper() {
        let mut c = ClosedLidAwake::new(FakeHold::default(), sleep(Some(false)), false);
        assert!(c.update(ClosedLidPolicy::Always, false));
        assert!(c.sleep_control().calls.is_empty());
        assert!(c.hold().is_held());
        assert!(!c.changed_system_disable);
    }

    /// `test_closed_lid_awake_controller_preserves_existing_system_disable`.
    #[test]
    fn preserves_existing_system_disable() {
        let mut c = ClosedLidAwake::new(FakeHold::default(), sleep(Some(true)), true);
        c.update(ClosedLidPolicy::Always, false);
        c.update(ClosedLidPolicy::Never, false);
        assert!(c.sleep_control().calls.is_empty());
    }

    #[test]
    fn unknown_state_is_set_and_helper_removal_reverts() {
        let mut c = ClosedLidAwake::new(FakeHold::default(), sleep(None), true);
        c.update(ClosedLidPolicy::Agents, true);
        assert_eq!(c.sleep_control().calls, [true]);
        c.set_use_system_disable(false);
        assert_eq!(c.sleep_control().calls, [true, false]);
        assert!(c.active(), "user-mode hold remains");
    }

    #[test]
    fn sudoers_rule_is_narrow() {
        assert_eq!(
            sleep_helper_sudoers_rule("pero").unwrap(),
            "pero ALL=(root) NOPASSWD: /usr/bin/pmset -a disablesleep 0, /usr/bin/pmset -a disablesleep 1\n"
        );
        assert_eq!(sleep_helper_sudoers_rule("bad user"), None);
        assert_eq!(sleep_helper_sudoers_rule(""), None);
    }

    /// Non-destructive live read; run with `cargo test -p sidepulse-core -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_lid_read() {
        let t = Instant::now();
        let closed = clamshell_closed();
        println!("clamshell closed: {closed:?} ({:?})", t.elapsed());
        println!("sleep disabled: {:?}", sleep_disabled());
        println!("sleep helper installed: {}", sleep_helper_installed());
    }
}
