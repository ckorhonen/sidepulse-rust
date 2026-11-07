//! Session rows and open actions: ports of `session_actions.py` and the menu helpers in
//! `status_bar.py` (`recent_statuses`, `menu_title_for_status`, `session_title_parts`, `format_age`).

use sidepulse_core::collector::project_name;
use sidepulse_core::models::{AgentMode, AgentStatus};

use crate::settings::Settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAction {
    App,
    Terminal,
    VsCode,
}

impl OpenAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "app" => Some(Self::App),
            "terminal" => Some(Self::Terminal),
            "vscode" => Some(Self::VsCode),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenTarget {
    Url(String),
    Terminal(String),
}

/// Menu-bar presentation of a mode (`state_for_mode`).
pub struct StateInfo {
    pub label: &'static str,
    pub symbol: &'static str,
}

pub fn state_for_mode(mode: AgentMode) -> StateInfo {
    match mode {
        AgentMode::Working | AgentMode::ToolRunning | AgentMode::LongTaskProgress => StateInfo {
            label: "Working",
            symbol: "arrow.triangle.2.circlepath",
        },
        AgentMode::Completed => StateInfo {
            label: "Done",
            symbol: "checkmark.circle",
        },
        AgentMode::WaitingForInput | AgentMode::BlockedError => StateInfo {
            label: "Ask",
            symbol: "questionmark.circle",
        },
        AgentMode::IdleReady | AgentMode::Unknown => StateInfo {
            label: "Idle",
            symbol: "circle",
        },
    }
}

/// RFC 3986 percent-encoding of every byte outside the unreserved set (`quote(s, safe='')`).
fn quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn session_id(status: &AgentStatus) -> Option<&str> {
    status.session_id.as_deref().filter(|s| !s.is_empty())
}

pub fn deep_link(status: &AgentStatus) -> Option<String> {
    match status.provider.to_lowercase().as_str() {
        "codex" => session_id(status).map(|id| format!("codex://threads/{}", quote(id))),
        "claude" => Some("claude://".into()),
        _ => None,
    }
}

pub fn vscode_link(status: &AgentStatus) -> Option<String> {
    if status.provider.to_lowercase() != "claude" {
        return None;
    }
    session_id(status)
        .map(|id| format!("vscode://anthropic.claude-code/open?session={}", quote(id)))
}

pub fn resume_command(status: &AgentStatus) -> Option<String> {
    let id = shell_quote(session_id(status)?);
    let home = sidepulse_core::providers::home_dir();
    let cwd = shell_quote(
        status
            .cwd
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&home.to_string_lossy()),
    );
    match status.provider.to_lowercase().as_str() {
        "codex" => Some(format!("cd {cwd} && codex resume {id}")),
        "claude" => Some(format!("cd {cwd} && claude --resume {id}")),
        "grok" => Some(format!("cd {cwd} && grok --resume {id}")),
        _ => None,
    }
}

pub fn open_target(status: &AgentStatus, action: OpenAction) -> Option<OpenTarget> {
    match action {
        OpenAction::App => deep_link(status).map(OpenTarget::Url),
        OpenAction::VsCode => vscode_link(status).map(OpenTarget::Url),
        OpenAction::Terminal => resume_command(status).map(OpenTarget::Terminal),
    }
}

fn normalized_origin(origin: Option<&str>) -> String {
    origin
        .unwrap_or("")
        .trim()
        .to_lowercase()
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn preferred_actions(status: &AgentStatus) -> [OpenAction; 3] {
    use OpenAction::*;
    let origin = normalized_origin(status.origin.as_deref());
    if !origin.is_empty() {
        let has = |list: &[&str]| list.iter().any(|s| origin.contains(s));
        if has(&["vscode", "vs code", "visual studio code"]) {
            return [VsCode, App, Terminal];
        }
        if has(&["cli", "terminal", "command line"]) {
            return [Terminal, App, VsCode];
        }
        if has(&["app", "ui", "transcript"]) {
            return [App, VsCode, Terminal];
        }
        if has(&["cursor", "windsurf"]) {
            return [App, Terminal, VsCode];
        }
    }
    if status.provider.to_lowercase() == "claude" {
        [VsCode, App, Terminal]
    } else {
        [App, Terminal, VsCode]
    }
}

pub fn default_action(status: &AgentStatus) -> OpenAction {
    preferred_actions(status)
        .into_iter()
        .find(|a| open_target(status, *a).is_some())
        .unwrap_or(OpenAction::Terminal)
}

/// `StatusBarController.open_session(status, None, remember=False)` target resolution.
pub fn resolve_open(status: &AgentStatus, settings: &Settings) -> Option<OpenTarget> {
    let requested = settings
        .session_open_action(&status.provider, status.origin.as_deref())
        .and_then(|a| OpenAction::parse(&a))
        .unwrap_or_else(|| default_action(status));
    open_target(status, requested).or_else(|| open_target(status, default_action(status)))
}

pub fn applescript_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn recent_statuses(statuses: &[AgentStatus]) -> Vec<AgentStatus> {
    let mut list = statuses.to_vec();
    list.sort_by(|a, b| {
        a.mode
            .priority()
            .cmp(&b.mode.priority())
            .then(b.updated_at.cmp(&a.updated_at))
    });
    list.truncate(12);
    list
}

fn normalized_part(text: &str) -> String {
    text.replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn strip_session_short_id(display_name: &str, session_id: Option<&str>) -> String {
    let text = display_name.trim();
    if let Some(id) = session_id.filter(|s| !s.is_empty()) {
        let short: String = id.chars().take(8).collect();
        let suffix = format!(" ({short})");
        if let Some(stripped) = text.strip_suffix(&suffix) {
            return stripped.trim().to_string();
        }
    }
    if text.ends_with(')') {
        if let Some((prefix, suffix)) = text.rsplit_once(" (") {
            let token = &suffix[..suffix.len() - 1];
            let n = token.chars().count();
            if (6..=12).contains(&n) && token.chars().all(|c| c.is_alphanumeric() || c == '-') {
                return prefix.trim().to_string();
            }
        }
    }
    text.to_string()
}

pub fn session_title_parts(status: &AgentStatus) -> (String, Option<String>) {
    let mut project = project_name(status.cwd.as_deref());
    let mut title = strip_session_short_id(&status.display_name, status.session_id.as_deref());
    let prefixed = project.as_ref().map(|p| format!("{p}: "));
    if let Some(rest) = prefixed.as_deref().and_then(|p| title.strip_prefix(p)) {
        title = rest.to_string();
    } else if let Some((maybe_project, maybe_title)) = title.clone().split_once(": ") {
        if project.is_none() {
            project = Some(maybe_project.to_string());
        }
        title = maybe_title.to_string();
    }
    if let Some(p) = &project {
        if normalized_part(p) == normalized_part(&title) {
            project = None;
        }
    }
    if title.is_empty() {
        title = status.display_name.clone();
    }
    (title, project)
}

pub fn menu_title(status: &AgentStatus) -> String {
    let state = state_for_mode(status.mode);
    let (title, project) = session_title_parts(status);
    let first = match status.origin.as_deref().filter(|s| !s.is_empty()) {
        Some(origin) => format!("{}  {}  {}", state.label, origin, title),
        None => format!("{}  {}", state.label, title),
    };
    match project {
        Some(p) => format!("{first}\n{p}"),
        None => first,
    }
}

pub fn format_age(seconds: f64) -> String {
    let s = seconds.max(0.0) as i64;
    if s < 60 {
        return format!("{s}s");
    }
    let (m, rest) = (s / 60, s % 60);
    if m < 60 {
        return format!("{m}m{rest:02}s");
    }
    format!("{}h{:02}m", m / 60, m % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn status(
        provider: &str,
        session: &str,
        origin: Option<&str>,
        display: &str,
        cwd: Option<&str>,
    ) -> AgentStatus {
        AgentStatus {
            provider: provider.into(),
            agent_id: format!("{provider}:session:{session}"),
            display_name: display.into(),
            mode: AgentMode::Working,
            updated_at: Utc::now(),
            event_name: "PreToolUse".into(),
            session_id: Some(session.into()),
            cwd: cwd.map(Into::into),
            tool_name: None,
            message: None,
            origin: origin.map(Into::into),
            stale: false,
        }
    }

    #[test]
    fn open_actions_follow_origin() {
        let s = status(
            "claude",
            "abc 1",
            Some("Claude Code CLI"),
            "x",
            Some("/tmp/my dir"),
        );
        assert_eq!(default_action(&s), OpenAction::Terminal);
        assert_eq!(
            resume_command(&s).unwrap(),
            "cd '/tmp/my dir' && claude --resume 'abc 1'"
        );
        let s = status("claude", "abc", None, "x", None);
        assert_eq!(default_action(&s), OpenAction::VsCode);
        assert_eq!(
            vscode_link(&s).unwrap(),
            "vscode://anthropic.claude-code/open?session=abc"
        );
        let s = status("codex", "a/b", Some("Codex UI"), "x", None);
        assert_eq!(
            open_target(&s, default_action(&s)),
            Some(OpenTarget::Url("codex://threads/a%2Fb".into()))
        );
    }

    #[test]
    fn title_parts_strip_ids_and_projects() {
        let s = status(
            "claude",
            "abcdef123",
            None,
            "sidepulse: Fix the hook (abcdef12)",
            Some("/nonexistent/sidepulse"),
        );
        assert_eq!(
            session_title_parts(&s),
            ("Fix the hook".into(), Some("sidepulse".into()))
        );
        assert_eq!(menu_title(&s), "Working  Fix the hook\nsidepulse");
        let s = status("codex", "zz", Some("Codex CLI"), "proj: proj (zz)", None);
        assert_eq!(session_title_parts(&s).1, None);
    }

    #[test]
    fn ages() {
        assert_eq!(format_age(5.0), "5s");
        assert_eq!(format_age(65.0), "1m05s");
        assert_eq!(format_age(3720.0), "1h02m");
    }
}
