//! Port of the parts of `sidepulse/providers.py` the hook needs.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::models::{parse_datetime, HookEvent};
use crate::origin::origin_label_from_payload;
use crate::pyjson::{first_string, or, Object};

pub const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "PreCompact",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];
pub const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Notification",
    "PreCompact",
    "PostCompact",
    "SubagentStop",
    "Stop",
    "SessionEnd",
];
pub const GROK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionDenied",
    "Notification",
    "PreCompact",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

fn is_known_event(name: &str) -> bool {
    CODEX_EVENTS.contains(&name) || CLAUDE_EVENTS.contains(&name) || GROK_EVENTS.contains(&name)
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `Path(value).expanduser()` for the common `~` / `~/...` forms.
pub fn expanduser(value: &str) -> PathBuf {
    if value == "~" {
        home_dir()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(value)
    }
}

pub fn default_state_dir() -> PathBuf {
    if let Some(xdg) = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return expanduser(&xdg).join("sidepulse").join("agent-monitor");
    }
    home_dir().join(".local/state/sidepulse/agent-monitor")
}

pub fn default_log_path(provider: &str) -> PathBuf {
    default_state_dir().join(format!("{provider}.jsonl"))
}

/// `detect_log_path(provider)`: first `--log` path configured for the provider's hooks.
pub fn detect_log_path(provider: &str) -> PathBuf {
    let home = home_dir();
    let paths = match provider {
        "claude" => json_hook_log_paths(&home.join(".claude/settings.json"), CLAUDE_EVENTS),
        "grok" => json_hook_log_paths(&home.join(".grok/hooks/sidepulse.json"), GROK_EVENTS),
        "codex" => codex_hook_log_paths(&home.join(".codex/config.toml")),
        _ => Vec::new(),
    };
    paths
        .into_iter()
        .next()
        .unwrap_or_else(|| default_log_path(provider))
}

fn json_hook_log_paths(config: &Path, events: &[&str]) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(config) else {
        return Vec::new();
    };
    let Ok(Value::Object(data)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    if let Some(Value::Object(hooks)) = data.get("hooks") {
        for (event_name, entries) in hooks {
            let canonical = canonical_event_name(&Value::String(event_name.clone()));
            let Some(canonical) = canonical else { continue };
            if !events.contains(&canonical.as_str()) {
                continue;
            }
            if let Value::Array(entries) = entries {
                paths.extend(paths_from_hook_entries(entries));
            }
        }
    }
    dedupe(paths)
}

/// Regex fallback of `detect_codex_config_from_text` (only `--log` paths are needed).
fn codex_hook_log_paths(config: &Path) -> Vec<PathBuf> {
    static TRIPLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)command\s*=\s*'''(.*?)'''").unwrap());
    static QUOTED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"command\s*=\s*"(.*?)""#).unwrap());
    let Ok(text) = std::fs::read_to_string(config) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for caps in TRIPLE.captures_iter(&text) {
        paths.extend(extract_log_paths_from_command(&caps[1]));
    }
    for caps in QUOTED.captures_iter(&text) {
        paths.extend(extract_log_paths_from_command(&caps[1]));
    }
    dedupe(paths)
}

fn paths_from_hook_entries(entries: &[Value]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in entries {
        let Some(hooks) = entry.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        for hook in hooks {
            if let Some(Value::String(command)) = hook.get("command") {
                paths.extend(extract_log_paths_from_command(command));
            }
        }
    }
    paths
}

pub fn extract_log_paths_from_command(command: &str) -> Vec<PathBuf> {
    // Python: r">>\s+(['\"]?)([^'\"\s]+)\1" (the back-reference expanded into alternatives).
    static REDIRECT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#">>\s+(?:'([^'"\s]+)'|"([^'"\s]+)"|([^'"\s]+))"#).unwrap());
    let mut paths = Vec::new();
    for caps in REDIRECT.captures_iter(command) {
        if let Some(m) = caps.get(1).or_else(|| caps.get(2)).or_else(|| caps.get(3)) {
            paths.push(expanduser(m.as_str()));
        }
    }
    let parts = shlex::split(command).unwrap_or_default();
    for (i, part) in parts.iter().enumerate() {
        if part == "--log" && i + 1 < parts.len() {
            paths.push(expanduser(&parts[i + 1]));
        } else if let Some(value) = part.strip_prefix("--log=") {
            paths.push(expanduser(value));
        }
    }
    dedupe(paths)
}

fn dedupe(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

pub fn infer_provider_from_payload(provider: &str, raw: &Object) -> String {
    if provider == "claude" && grok_payload_looks_compatible(raw) {
        "grok".into()
    } else {
        provider.into()
    }
}

fn grok_payload_looks_compatible(raw: &Object) -> bool {
    let transcript = or(raw.get("transcriptPath"), raw.get("transcript_path"))
        .map(crate::pyjson::py_str)
        .unwrap_or_default();
    if transcript.contains("/.grok/") || transcript.contains("\\.grok\\") {
        return true;
    }
    raw.contains_key("hookEventName")
        && ["hookEventName", "sessionId", "workspaceRoot"]
            .iter()
            .any(|k| raw.contains_key(*k))
}

fn camel_to_snake(text: &str) -> String {
    static CAMEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z0-9])([A-Z])").unwrap());
    CAMEL.replace_all(text, "${1}_${2}").into_owned()
}

pub fn canonical_event_name(value: &Value) -> Option<String> {
    static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-zA-Z0-9]+").unwrap());
    let Value::String(text) = value else {
        return None;
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if is_known_event(text) {
        return Some(text.to_string());
    }
    let normalized = NON_ALNUM.replace_all(text, "_");
    let normalized = camel_to_snake(&normalized);
    let normalized = normalized.trim_matches('_').to_lowercase();
    let extra: &[(&str, &str)] = &[
        ("pre_tool_use", "PreToolUse"),
        ("post_tool_use", "PostToolUse"),
        ("post_tool_use_failure", "PostToolUseFailure"),
        ("permission_request", "PermissionRequest"),
        ("permission_denied", "PermissionDenied"),
        ("user_prompt_submit", "UserPromptSubmit"),
        ("session_start", "SessionStart"),
        ("session_end", "SessionEnd"),
        ("subagent_start", "SubagentStart"),
        ("subagent_stop", "SubagentStop"),
        ("subagent_end", "SubagentStop"),
        ("pre_compact", "PreCompact"),
        ("post_compact", "PostCompact"),
        ("stop_failure", "StopFailure"),
    ];
    if let Some((_, event)) = extra.iter().find(|(k, _)| *k == normalized) {
        return Some((*event).to_string());
    }
    CODEX_EVENTS
        .iter()
        .chain(CLAUDE_EVENTS)
        .chain(GROK_EVENTS)
        .find(|event| camel_to_snake(event).to_lowercase() == normalized)
        .map(|e| (*e).to_string())
}

fn normalize_event_payload(raw: &Object, event_name: &str, logged_at: Option<&Value>) -> Object {
    let mut normalized = raw.clone();
    normalized
        .entry("hook_event_name")
        .or_insert_with(|| Value::String(event_name.into()));
    if let Some(v) = logged_at.filter(|v| !v.is_null()) {
        normalized.entry("logged_at").or_insert_with(|| v.clone());
    }
    for (source, target) in [
        ("sessionId", "session_id"),
        ("turnId", "turn_id"),
        ("agentId", "agent_id"),
        ("workspaceRoot", "cwd"),
        ("toolName", "tool_name"),
        ("toolInput", "tool_input"),
        ("toolResponse", "tool_response"),
        ("lastAssistantMessage", "last_assistant_message"),
        ("notificationType", "notification_type"),
        ("agentOrigin", "agent_origin"),
        ("agentOriginKind", "agent_origin_kind"),
        ("sidepulseOrigin", "sidepulse_origin"),
    ] {
        if !normalized.contains_key(target) {
            if let Some(v) = normalized.get(source).cloned() {
                normalized.insert(target.into(), v);
            }
        }
    }
    normalized
}

/// `parse_log_line(provider, json.dumps(obj))` without the serialize/parse round trip.
pub fn parse_log_object(provider: &str, obj: &Object) -> Option<HookEvent> {
    let (raw, logged_at) = match obj.get("event") {
        Some(Value::Object(event)) if provider == "codex" => {
            (event, or(obj.get("logged_at"), event.get("logged_at")))
        }
        _ => (obj, or(obj.get("logged_at"), obj.get("timestamp"))),
    };
    let provider = infer_provider_from_payload(provider, raw);
    let name_value = or(
        or(raw.get("hook_event_name"), raw.get("hookEventName")),
        or(raw.get("event_name"), raw.get("eventName")),
    )?;
    let event_name = canonical_event_name(name_value)?;
    let normalized = normalize_event_payload(raw, &event_name, logged_at);
    let origin = origin_label_from_payload(&provider, &normalized);
    Some(HookEvent {
        logged_at: parse_datetime(logged_at),
        session_id: first_string(&normalized, &["session_id", "sessionId"]),
        turn_id: first_string(&normalized, &["turn_id", "turnId"]),
        agent_id: first_string(&normalized, &["agent_id", "agentId"]),
        cwd: first_string(&normalized, &["cwd", "workspaceRoot"]),
        tool_name: first_string(&normalized, &["tool_name", "toolName"]),
        message: first_string(
            &normalized,
            &["message", "last_assistant_message", "lastAssistantMessage"],
        ),
        origin,
        provider,
        event_name,
        raw: normalized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonicalizes_event_names() {
        assert_eq!(
            canonical_event_name(&json!("PreToolUse")).as_deref(),
            Some("PreToolUse")
        );
        assert_eq!(
            canonical_event_name(&json!("pre-tool-use")).as_deref(),
            Some("PreToolUse")
        );
        assert_eq!(
            canonical_event_name(&json!("subagent_end")).as_deref(),
            Some("SubagentStop")
        );
        assert_eq!(
            canonical_event_name(&json!("stopFailure")).as_deref(),
            Some("StopFailure")
        );
        assert_eq!(canonical_event_name(&json!("bogus")), None);
        assert_eq!(canonical_event_name(&json!(3)), None);
    }

    #[test]
    fn extracts_log_paths() {
        let paths = extract_log_paths_from_command(
            "python3 hook.py --provider claude --log '/tmp/a b.jsonl' >> \"/tmp/x.log\"",
        );
        assert_eq!(
            paths,
            vec![PathBuf::from("/tmp/x.log"), PathBuf::from("/tmp/a b.jsonl")]
        );
    }

    #[test]
    fn detects_grok_payloads() {
        let raw = json!({"hookEventName": "Stop", "sessionId": "x"});
        assert_eq!(
            infer_provider_from_payload("claude", raw.as_object().unwrap()),
            "grok"
        );
        let raw = json!({"hook_event_name": "Stop", "transcript_path": "/u/.claude/x"});
        assert_eq!(
            infer_provider_from_payload("claude", raw.as_object().unwrap()),
            "claude"
        );
    }
}
