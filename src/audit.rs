//! Port of `status_audit_record` / `append_status_audit_record` from `sidepulse/audit.py`.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::logrot::{append_json_record, AUDIT_LOG_BACKUP_COUNT, AUDIT_LOG_MAX_BYTES};
use crate::models::{isoformat, utc_stamp, AgentStatus, HookEvent};
use crate::providers::default_state_dir;
use crate::pyjson::{sorted_default, Object};

pub const RAW_PREVIEW_LIMIT: usize = 2000;
pub const MESSAGE_PREVIEW_LIMIT: usize = 240;

pub fn default_status_audit_log_path() -> PathBuf {
    default_state_dir().join("event-status.jsonl")
}

pub fn status_audit_record(event: &HookEvent, status: Option<&AgentStatus>) -> Value {
    let origin = status
        .and_then(|s| s.origin.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| event.origin.clone())
        .unwrap_or_default();
    let message = event
        .message
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| raw_message(&event.raw));
    let fields: [(&str, String); 14] = [
        ("audited_at", utc_stamp()),
        ("logged_at", isoformat(&event.logged_at)),
        ("provider", event.provider.clone()),
        ("hook_event", event.event_name.clone()),
        (
            "status",
            status
                .map(|s| s.mode.value().to_string())
                .unwrap_or_default(),
        ),
        (
            "status_label",
            status
                .map(|s| s.mode.label().to_string())
                .unwrap_or_default(),
        ),
        ("origin", origin),
        (
            "display_name",
            status.map(|s| s.display_name.clone()).unwrap_or_default(),
        ),
        ("session_id", event.session_id.clone().unwrap_or_default()),
        ("agent_id", event.status_key()),
        ("cwd", event.cwd.clone().unwrap_or_default()),
        ("tool_name", event.tool_name.clone().unwrap_or_default()),
        ("message", truncate_preview(&message, MESSAGE_PREVIEW_LIMIT)),
        (
            "raw_preview",
            truncate_preview(
                &sorted_default(&Value::Object(event.raw.clone())),
                RAW_PREVIEW_LIMIT,
            ),
        ),
    ];
    let mut map = Map::new();
    for (k, v) in fields {
        map.insert(k.into(), Value::String(v));
    }
    Value::Object(map)
}

fn raw_message(raw: &Object) -> String {
    for key in ["message", "last_assistant_message", "prompt"] {
        if let Some(Value::String(s)) = raw.get(key) {
            if !s.trim().is_empty() {
                return s.trim().to_string();
            }
        }
    }
    String::new()
}

pub fn truncate_preview(text: &str, limit: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text;
    }
    let head: String = chars[..limit.saturating_sub(3)].iter().collect();
    format!("{}...", head.trim_end())
}

pub fn append_status_audit_record(event: &HookEvent, status: Option<&AgentStatus>) {
    let _ = append_json_record(
        &default_status_audit_log_path(),
        &status_audit_record(event, status),
        AUDIT_LOG_MAX_BYTES,
        AUDIT_LOG_BACKUP_COUNT,
    );
}
