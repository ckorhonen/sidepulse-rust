//! Port of the per-event status logic in `sidepulse/collector.py`
//! (`status_from_event`, `mode_for_event`, `title_from_event` and helpers).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::models::{provider_label, AgentMode, AgentStatus, HookEvent};
use crate::origin::origin_label_from_payload;
use crate::providers::home_dir;
use crate::pyjson::{get_str_or, or, prefix_chars, splitlines, string_or_none, Object};

pub const CODEX_SESSION_INDEX_MAX_LINES: usize = 5000;

#[derive(Clone, Debug, Default)]
pub struct StatusMetadata {
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub origin: Option<String>,
}

macro_rules! re {
    ($pat:expr) => {{
        static R: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
        &*R
    }};
}

pub fn status_from_event(record: &HookEvent, metadata: &StatusMetadata) -> Option<AgentStatus> {
    let mode = mode_for_event(record)?;
    if should_ignore_record(record, metadata) {
        return None;
    }
    let label = provider_label(&record.provider);
    let display_name = if let Some(agent) = record.agent_id.as_deref().filter(|s| !s.is_empty()) {
        let short = prefix_chars(agent, 8);
        let fallback = format!("{label} agent {short}");
        display_name_for_record(record, metadata, &format!("agent {short}"), &fallback)
    } else if let Some(session) = record.session_id.as_deref().filter(|s| !s.is_empty()) {
        let short = prefix_chars(session, 8);
        let fallback = format!("{label} session {short}");
        display_name_for_record(record, metadata, short, &fallback)
    } else {
        label
    };
    let origin = record
        .origin
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| metadata.origin.clone().filter(|s| !s.is_empty()))
        .or_else(|| origin_label_from_payload(&record.provider, &record.raw));
    Some(AgentStatus {
        provider: record.provider.clone(),
        agent_id: record.status_key(),
        display_name,
        mode,
        updated_at: record.logged_at,
        event_name: record.event_name.clone(),
        session_id: record.session_id.clone(),
        cwd: record.cwd.clone(),
        tool_name: record.tool_name.clone(),
        message: record.message.clone(),
        origin,
        stale: false,
    })
}

pub fn mode_for_event(record: &HookEvent) -> Option<AgentMode> {
    let raw = &record.raw;
    if let Some(mode) = explicit_mode_for_record(raw) {
        return Some(mode);
    }
    Some(match record.event_name.as_str() {
        "PostToolUseFailure" | "PermissionDenied" | "StopFailure" => AgentMode::BlockedError,
        "PermissionRequest" => AgentMode::WaitingForInput,
        "Notification" => {
            let notification_type = get_str_or(raw, "notification_type", "")
                .trim()
                .to_lowercase();
            let message = get_str_or(raw, "message", "").trim().to_lowercase();
            let text = format!(
                "{} {}",
                get_str_or(raw, "notification_type", ""),
                get_str_or(raw, "message", "")
            )
            .to_lowercase();
            if notification_text_indicates_completion(&notification_type, &message) {
                AgentMode::Completed
            } else if notification_text_indicates_input_needed(&text) {
                AgentMode::WaitingForInput
            } else {
                AgentMode::Working
            }
        }
        "PreToolUse" => AgentMode::ToolRunning,
        "PostToolUse" => {
            if tool_response_looks_failed(raw.get("tool_response")) {
                AgentMode::BlockedError
            } else {
                AgentMode::Working
            }
        }
        "UserPromptSubmit" | "PreCompact" | "PostCompact" | "SubagentStart" => AgentMode::Working,
        "Stop" | "SubagentStop" => {
            if assistant_message_asks_question(raw.get("last_assistant_message")) {
                AgentMode::WaitingForInput
            } else {
                AgentMode::Completed
            }
        }
        "SessionEnd" => AgentMode::Completed,
        "SessionStart" => AgentMode::IdleReady,
        _ => return None,
    })
}

fn explicit_mode_for_record(raw: &Object) -> Option<AgentMode> {
    for key in ["sidepulse_status", "sidepulse_mode"] {
        if let Some(mode) = explicit_mode_from_value(raw.get(key)) {
            return Some(mode);
        }
    }
    explicit_mode_from_message(or(raw.get("last_assistant_message"), raw.get("message")))
}

fn notification_text_indicates_completion(notification_type: &str, message: &str) -> bool {
    let text = format!("{notification_type} {message}");
    let text = text.trim();
    let phrases = [
        "turn complete",
        "turn completed",
        "task complete",
        "task completed",
        "completed successfully",
        "work complete",
        "work completed",
    ];
    if phrases.iter().any(|p| text.contains(p)) {
        return true;
    }
    notification_type == "idle_prompt" && ["done", "complete", "completed"].contains(&message)
}

fn notification_text_indicates_input_needed(text: &str) -> bool {
    [
        "waiting for your input",
        "waiting for input",
        "needs your input",
        "needs input",
        "permission",
        "approval",
        "confirm",
    ]
    .iter()
    .any(|p| text.contains(p))
}

pub fn explicit_mode_from_value(value: Option<&Value>) -> Option<AgentMode> {
    let Some(Value::String(value)) = value else {
        return None;
    };
    let lowered = value.trim().to_lowercase();
    let normalized = re!(r"[^a-z0-9]+").replace_all(&lowered, "_");
    Some(match normalized.trim_matches('_') {
        "ask" | "question" | "waiting" | "waiting_for_input" | "input" => {
            AgentMode::WaitingForInput
        }
        "blocked" | "error" | "blocked_error" => AgentMode::BlockedError,
        "working" => AgentMode::Working,
        "tool_running" => AgentMode::ToolRunning,
        "progress" | "long_task_progress" => AgentMode::LongTaskProgress,
        "done" | "complete" | "completed" => AgentMode::Completed,
        "idle" | "ready" | "idle_ready" => AgentMode::IdleReady,
        _ => return None,
    })
}

fn explicit_mode_from_message(message: Option<&Value>) -> Option<AgentMode> {
    let Some(Value::String(message)) = message else {
        return None;
    };
    let text = strip_markdown_code_blocks(message);
    let patterns: [&Regex; 3] = [
        re!(r"(?im)^\s*<!--\s*(?:sidepulse|agent[-_ ]monitor)\s*:\s*([a-z0-9_ -]+)\s*-->\s*$"),
        re!(
            r"(?im)^\s*<!--\s*(?:sidepulse|agent[-_ ]monitor)\s+(?:status|mode)\s*:\s*([a-z0-9_ -]+)\s*-->\s*$"
        ),
        re!(
            r"(?im)^\s*\[(?:sidepulse|agent[-_ ]monitor)\s+(?:status|mode)\s*:\s*([a-z0-9_ -]+)\]\s*$"
        ),
    ];
    for pattern in patterns {
        for caps in pattern.captures_iter(&text) {
            if let Some(mode) = explicit_mode_from_value(Some(&Value::String(caps[1].to_string())))
            {
                return Some(mode);
            }
        }
    }
    None
}

fn strip_markdown_code_blocks(text: &str) -> String {
    re!(r"(?s)```.*?```").replace_all(text, "").into_owned()
}

fn strip_markdown_inline_code(text: &str) -> String {
    re!(r"`[^`\n]*`").replace_all(text, "").into_owned()
}

fn should_ignore_record(record: &HookEvent, metadata: &StatusMetadata) -> bool {
    if record.provider != "codex" {
        return false;
    }
    let raw = &record.raw;
    let parts: Vec<String> = [
        metadata.title.clone(),
        string_or_none(raw.get("prompt")),
        string_or_none(raw.get("message")),
        string_or_none(raw.get("last_assistant_message")),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    .collect();
    let text = parts.join(" ").to_lowercase();
    !text.is_empty()
        && [
            "generate 0 to 3 hyperpersonalized suggestions",
            "you are an expert at upholding safety and compliance standards",
        ]
        .iter()
        .any(|p| text.contains(p))
}

pub fn title_from_event(record: &HookEvent) -> Option<String> {
    if record.provider == "codex" {
        if let Some(title) = codex_session_title(record.session_id.as_deref()) {
            return Some(title);
        }
    }
    if record.event_name != "UserPromptSubmit" {
        return None;
    }
    summarize_prompt(record.raw.get("prompt"), 72)
}

fn codex_session_title(session_id: Option<&str>) -> Option<String> {
    let session_id = session_id.filter(|s| !s.is_empty())?;
    let path = home_dir().join(".codex/session_index.jsonl");
    let mut title = None;
    for line in read_recent_lines(&path, CODEX_SESSION_INDEX_MAX_LINES).ok()? {
        let Ok(Value::Object(row)) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = string_or_none(row.get("id"));
        let name = string_or_none(row.get("thread_name"));
        if let (Some(id), Some(name)) = (id, name) {
            if !id.is_empty() && !name.is_empty() && id == session_id {
                title = Some(truncate_text(name.trim(), 72));
            }
        }
    }
    title
}

pub fn read_recent_lines(path: &Path, max_lines: usize) -> std::io::Result<Vec<String>> {
    if max_lines == 0 {
        return Ok(Vec::new());
    }
    let mut file = std::fs::File::open(path)?;
    let mut position = file.seek(SeekFrom::End(0))?;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut newlines = 0usize;
    while position > 0 && newlines <= max_lines {
        let size = position.min(8192);
        position -= size;
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = vec![0u8; size as usize];
        file.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|b| **b == b'\n').count();
        chunks.push(chunk);
    }
    let data: Vec<u8> = chunks.into_iter().rev().flatten().collect();
    let text = String::from_utf8_lossy(&data);
    let lines = splitlines(&text);
    let start = lines.len().saturating_sub(max_lines);
    Ok(lines[start..].iter().map(|s| s.to_string()).collect())
}

pub fn summarize_prompt(value: Option<&Value>, max_len: usize) -> Option<String> {
    let Some(Value::String(value)) = value else {
        return None;
    };
    let mut text = value.trim().to_string();
    if text.is_empty() || text.starts_with("<task-notification>") {
        return None;
    }
    if let Some(caps) = re!(r"(?is)##\s+My request for [^:\n]+:\s*(.*)").captures(&text) {
        text = caps[1].to_string();
    }
    text = re!(r"(?s)```.*?```").replace_all(&text, " ").into_owned();
    text = re!(r"`([^`]+)`").replace_all(&text, "${1}").into_owned();
    // Python: (['\"])(?:~|/Users|/var|/private|/tmp)[^'\"]+\1 -> \1...\1
    text =
        re!(r#"'(?:~|/Users|/var|/private|/tmp)[^'"]+'|"(?:~|/Users|/var|/private|/tmp)[^'"]+""#)
            .replace_all(&text, |caps: &regex::Captures| {
                let q = &caps[0][..1];
                format!("{q}...{q}")
            })
            .into_owned();
    text = re!(r#"(?:~|/Users|/var|/private|/tmp)/[^\s,;)'"`]+"#)
        .replace_all(&text, "...")
        .into_owned();
    text = re!(r"<[^>]+>").replace_all(&text, " ").into_owned();
    text = re!(r"#+\s*").replace_all(&text, " ").into_owned();
    text = re!(r"\s+").replace_all(&text, " ").into_owned();
    let text = text.trim_matches(|c| matches!(c, ' ' | '-' | ':' | '\n' | '\t'));
    if text.is_empty() {
        return None;
    }
    Some(truncate_text(text, max_len))
}

fn display_name_for_record(
    record: &HookEvent,
    metadata: &StatusMetadata,
    short_id: &str,
    fallback: &str,
) -> String {
    let cwd = metadata
        .cwd
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(record.cwd.as_deref());
    let project = project_name(cwd);
    display_name_from_parts(
        project.as_deref(),
        metadata.title.as_deref(),
        short_id,
        fallback,
    )
}

pub fn display_name_from_parts(
    project: Option<&str>,
    title: Option<&str>,
    short_id: &str,
    fallback: &str,
) -> String {
    let project = project.filter(|s| !s.is_empty());
    let title = title.filter(|s| !s.is_empty());
    match (project, title) {
        (Some(p), Some(t)) if normalized_name_part(p) == normalized_name_part(t) => {
            truncate_text(&format!("{t} ({short_id})"), 96)
        }
        (Some(p), Some(t)) => truncate_text(&format!("{p}: {t} ({short_id})"), 96),
        (None, Some(t)) => truncate_text(&format!("{t} ({short_id})"), 96),
        (Some(p), None) => truncate_text(&format!("{p} ({short_id})"), 96),
        (None, None) => fallback.to_string(),
    }
}

fn normalized_name_part(text: &str) -> String {
    text.replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn project_name(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd.filter(|s| !s.is_empty())?;
    let path = PathBuf::from(cwd);
    for candidate in path.ancestors() {
        let probe = if candidate.as_os_str().is_empty() {
            Path::new(".")
        } else {
            candidate
        };
        if probe.join(".git").exists() {
            return Some(match probe.file_name() {
                Some(name) => name.to_string_lossy().into_owned(),
                None => probe.to_string_lossy().into_owned(),
            });
        }
    }
    Some(match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => cwd.to_string(),
    })
}

pub fn truncate_text(text: &str, max_len: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_len {
        return text.to_string();
    }
    let mut trimmed: String = chars[..max_len.saturating_sub(1)]
        .iter()
        .collect::<String>()
        .trim_end()
        .to_string();
    let tchars: Vec<char> = trimmed.chars().collect();
    let boundary = tchars.iter().rposition(|c| matches!(c, ' ' | ',' | ';'));
    if let Some(b) = boundary {
        if b >= max_len / 2 {
            trimmed = tchars[..b]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string();
        }
    }
    format!("{trimmed}...")
}

fn tool_response_looks_failed(response: Option<&Value>) -> bool {
    match response {
        Some(Value::Object(r)) => {
            if r.get("interrupted") == Some(&Value::Bool(true))
                || r.get("success") == Some(&Value::Bool(false))
            {
                return true;
            }
            match r.get("exit_code") {
                None | Some(Value::Null) | Some(Value::Bool(false)) => false,
                Some(Value::Number(n)) => n.as_f64() != Some(0.0),
                Some(_) => true,
            }
        }
        Some(Value::String(s)) => {
            let text = s.to_lowercase();
            text.contains("exit code: 1") || text.contains("traceback")
        }
        _ => false,
    }
}

fn assistant_message_asks_question(message: Option<&Value>) -> bool {
    let Some(Value::String(message)) = message else {
        return false;
    };
    let text = strip_markdown_inline_code(&strip_markdown_code_blocks(message));
    let lines: Vec<&str> = splitlines(&text)
        .into_iter()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(8)..];
    for line in tail.iter().rev() {
        if assistant_status_line(line) {
            continue;
        }
        if assistant_line_asks_question(line) {
            return true;
        }
    }
    false
}

fn assistant_status_line(line: &str) -> bool {
    let text = line.trim().to_lowercase();
    ["* cogitated ", "* recap:", "※ recap:", "recap:"]
        .iter()
        .any(|p| text.starts_with(p))
}

fn assistant_line_asks_question(line: &str) -> bool {
    let text = line.trim();
    if text.is_empty() {
        return false;
    }
    let lowered = text.to_lowercase();
    if text.ends_with(':') || assistant_line_is_casual_closing_question(&lowered) {
        return false;
    }
    if re!(r"(?:^|[.!?]\s+)(?:want me to|need me to|should i|should we|do you want me to)\b")
        .is_match(&lowered)
    {
        return true;
    }
    const DIRECT: [&str; 8] = [
        "please confirm",
        "please choose",
        "choose ",
        "need me to ",
        "want me to ",
        "should i ",
        "should we ",
        "do you want me to ",
    ];
    const QUESTION: [&str; 9] = [
        "which ",
        "what ",
        "where ",
        "when ",
        "who ",
        "why ",
        "how ",
        "can you ",
        "could you ",
    ];
    if text.ends_with('?') {
        return QUESTION
            .iter()
            .chain(DIRECT.iter())
            .any(|p| lowered.starts_with(p));
    }
    DIRECT.iter().any(|p| lowered.starts_with(p))
}

fn assistant_line_is_casual_closing_question(lowered: &str) -> bool {
    [
        "anything else",
        "any other",
        "all good",
        "need anything else",
        "want anything else",
        "anything you want",
        "anything you'd like",
        "anything else you want",
        "anything else you'd like",
    ]
    .iter()
    .any(|p| lowered.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truncate_matches_python() {
        assert_eq!(truncate_text("short", 10), "short");
        assert_eq!(
            truncate_text("hello world this is long", 12),
            "hello world..."
        );
        assert_eq!(truncate_text("abcdefghijklmnop", 8), "abcdefg...");
    }

    #[test]
    fn summarize_prompt_scrubs_paths_and_code() {
        let p = json!("Fix `foo` in '/Users/me/x.py' and /tmp/y/z.txt <b>now</b> ## Heading");
        assert_eq!(
            summarize_prompt(Some(&p), 72).as_deref(),
            Some("Fix foo in '...' and ... now Heading")
        );
        assert_eq!(
            summarize_prompt(Some(&json!("<task-notification>x")), 72),
            None
        );
    }

    #[test]
    fn question_detection() {
        assert!(assistant_message_asks_question(Some(&json!(
            "Done.\n\nWant me to deploy it?"
        ))));
        assert!(!assistant_message_asks_question(Some(&json!(
            "Done.\nAnything else?"
        ))));
        assert!(assistant_message_asks_question(Some(&json!(
            "Which option do you prefer?"
        ))));
        assert!(!assistant_message_asks_question(Some(&json!(
            "Is this fine?"
        ))));
    }

    #[test]
    fn explicit_modes() {
        assert_eq!(
            explicit_mode_from_message(Some(&json!("x\n<!-- sidepulse: waiting -->\n"))),
            Some(AgentMode::WaitingForInput)
        );
        assert_eq!(
            explicit_mode_from_message(Some(&json!("```\n<!-- sidepulse: done -->\n```"))),
            None
        );
        assert_eq!(
            explicit_mode_from_value(Some(&json!(" Blocked-Error "))),
            Some(AgentMode::BlockedError)
        );
    }
}
