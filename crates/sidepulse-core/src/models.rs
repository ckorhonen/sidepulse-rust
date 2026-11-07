//! Port of `sidepulse/models.py`.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Timelike, Utc};
use serde_json::Value;

use crate::pyjson::{title_case, Object};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentMode {
    IdleReady,
    Working,
    ToolRunning,
    WaitingForInput,
    LongTaskProgress,
    BlockedError,
    Completed,
    Unknown,
}

impl AgentMode {
    pub fn value(self) -> &'static str {
        match self {
            Self::IdleReady => "idle_ready",
            Self::Working => "working",
            Self::ToolRunning => "tool_running",
            Self::WaitingForInput => "waiting_for_input",
            Self::LongTaskProgress => "long_task_progress",
            Self::BlockedError => "blocked_error",
            Self::Completed => "completed",
            Self::Unknown => "unknown",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::IdleReady => "Idle / Ready",
            Self::Working => "Working",
            Self::ToolRunning => "Tool Running",
            Self::WaitingForInput => "Waiting for Input",
            Self::LongTaskProgress => "Long Task Progress",
            Self::BlockedError => "Blocked / Error",
            Self::Completed => "Completed",
            Self::Unknown => "Unknown",
        }
    }

    pub fn priority(self) -> u8 {
        match self {
            Self::BlockedError => 1,
            Self::WaitingForInput => 2,
            Self::ToolRunning => 3,
            Self::LongTaskProgress => 4,
            Self::Working => 5,
            Self::Completed => 6,
            Self::IdleReady => 7,
            Self::Unknown => 99,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HookEvent {
    pub provider: String,
    pub logged_at: DateTime<Utc>,
    pub event_name: String,
    pub raw: Object,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub agent_id: Option<String>,
    pub cwd: Option<String>,
    pub tool_name: Option<String>,
    pub message: Option<String>,
    pub origin: Option<String>,
}

impl HookEvent {
    pub fn status_key(&self) -> String {
        if let Some(agent) = self.agent_id.as_deref().filter(|s| !s.is_empty()) {
            return format!("{}:agent:{}", self.provider, agent);
        }
        if let Some(session) = self.session_id.as_deref().filter(|s| !s.is_empty()) {
            return format!("{}:session:{}", self.provider, session);
        }
        format!("{}:unknown", self.provider)
    }
}

#[derive(Clone, Debug)]
pub struct AgentStatus {
    pub provider: String,
    pub agent_id: String,
    pub display_name: String,
    pub mode: AgentMode,
    pub updated_at: DateTime<Utc>,
    pub event_name: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub tool_name: Option<String>,
    pub message: Option<String>,
    pub origin: Option<String>,
    pub stale: bool,
}

/// `parse_datetime(value)`: ISO-8601 string to UTC, falling back to "now".
pub fn parse_datetime(value: Option<&Value>) -> DateTime<Utc> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => parse_iso(s.trim()).unwrap_or_else(Utc::now),
        _ => Utc::now(),
    }
}

fn parse_iso(text: &str) -> Option<DateTime<Utc>> {
    let text = match text.strip_suffix('Z') {
        Some(rest) => format!("{rest}+00:00"),
        None => text.to_string(),
    };
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M%:z",
    ] {
        if let Ok(dt) = DateTime::<FixedOffset>::parse_from_str(&text, fmt) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&text, fmt) {
            return Some(dt.and_utc());
        }
    }
    NaiveDate::parse_from_str(&text, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc())
}

/// Python `datetime.isoformat()` for a UTC datetime.
pub fn isoformat(dt: &DateTime<Utc>) -> String {
    let micros = dt.nanosecond() / 1_000;
    let base = dt.format("%Y-%m-%dT%H:%M:%S");
    if micros == 0 {
        format!("{base}+00:00")
    } else {
        format!("{base}.{micros:06}+00:00")
    }
}

/// `datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")`.
pub fn utc_stamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn provider_label(provider: &str) -> String {
    match provider {
        "codex" => "Codex".into(),
        "claude" => "Claude".into(),
        "grok" => "Grok".into(),
        other => title_case(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_python_style_timestamps() {
        let dt = parse_datetime(Some(&json!("2026-11-07T14:03:00Z")));
        assert_eq!(isoformat(&dt), "2026-11-07T14:03:00+00:00");
        let dt = parse_datetime(Some(&json!("2026-11-07T14:03:00.123456-05:00")));
        assert_eq!(isoformat(&dt), "2026-11-07T19:03:00.123456+00:00");
        let dt = parse_datetime(Some(&json!("2026-11-07T14:03:00.5")));
        assert_eq!(isoformat(&dt), "2026-11-07T14:03:00.500000+00:00");
    }

    #[test]
    fn status_key_prefers_agent_then_session() {
        let mut ev = HookEvent {
            provider: "claude".into(),
            logged_at: Utc::now(),
            event_name: "Stop".into(),
            raw: Object::new(),
            session_id: Some("s1".into()),
            turn_id: None,
            agent_id: None,
            cwd: None,
            tool_name: None,
            message: None,
            origin: None,
        };
        assert_eq!(ev.status_key(), "claude:session:s1");
        ev.agent_id = Some("a1".into());
        assert_eq!(ev.status_key(), "claude:agent:a1");
    }
}
