//! Port of `sidepulse/hook.py` + `hook_entry.py`.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::audit::append_status_audit_record;
use crate::collector::{status_from_event, title_from_event, StatusMetadata};
use crate::ipc::{default_event_socket_path, send_hook_event, HOOK_EVENT_SEND_TIMEOUT};
use crate::logrot::{append_json_record, HOOK_LOG_BACKUP_COUNT, HOOK_LOG_MAX_BYTES};
use crate::models::utc_stamp;
use crate::origin::{annotate_payload_with_origin, detect_agent_origin, AgentOrigin, Env};
use crate::providers::{
    detect_log_path, expanduser, infer_provider_from_payload, parse_log_object,
};

/// `format_hook_payload(provider, text, include_origin=False)`.
pub fn format_hook_payload(provider: &str, payload_text: &str, logged_at: &str) -> Value {
    let text = if payload_text.is_empty() {
        "{}"
    } else {
        payload_text
    };
    let payload: Value = serde_json::from_str(text).unwrap_or_else(|err| {
        let mut m = Map::new();
        m.insert("hook_event_name".into(), "ParseError".into());
        m.insert("raw".into(), payload_text.into());
        m.insert("parse_error".into(), python_style_parse_error(&err).into());
        Value::Object(m)
    });
    let wrap = |payload: Value| {
        let mut m = Map::new();
        m.insert("logged_at".into(), logged_at.into());
        m.insert("event".into(), payload);
        Value::Object(m)
    };
    if provider == "codex" {
        return wrap(payload);
    }
    match payload {
        Value::Object(mut line) => {
            let keep = crate::pyjson::truthy(line.get("logged_at"));
            if !keep {
                line.insert("logged_at".into(), logged_at.into());
            }
            Value::Object(line)
        }
        other => wrap(other),
    }
}

fn python_style_parse_error(err: &serde_json::Error) -> String {
    // Python: "Expecting value: line 1 column 1 (char 0)". Exact wording differs; keep position info.
    format!("{err}")
}

fn infer_provider_from_hook_line(provider: &str, line: &Value) -> String {
    let raw = if provider == "codex" {
        line.get("event")
    } else {
        Some(line)
    };
    match raw {
        Some(Value::Object(raw)) => infer_provider_from_payload(provider, raw),
        _ => provider.into(),
    }
}

fn annotate_hook_line(line: Value, detect: &mut dyn FnMut() -> AgentOrigin) -> Value {
    let Value::Object(mut obj) = line else {
        return line;
    };
    if let Some(Value::Object(event)) = obj.get("event") {
        let annotated = annotate_payload_with_origin(event, detect);
        obj.insert("event".into(), Value::Object(annotated));
        return Value::Object(obj);
    }
    Value::Object(annotate_payload_with_origin(&obj, detect))
}

pub struct Routed {
    pub provider: String,
    pub log_path: PathBuf,
    pub line: Value,
}

/// `routed_hook_payload(provider, log_path, text)`.
pub fn routed_hook_payload(
    provider: &str,
    log_path: &Path,
    payload_text: &str,
    env: &Env,
    parent_pid: i32,
) -> Routed {
    let line = format_hook_payload(provider, payload_text, &utc_stamp());
    let actual = infer_provider_from_hook_line(provider, &line);
    let origin: OnceCell<AgentOrigin> = OnceCell::new();
    let mut detect = || {
        origin
            .get_or_init(|| detect_agent_origin(&actual, env, parent_pid))
            .clone()
    };
    let line = annotate_hook_line(line, &mut detect);
    let log_path = if actual != provider {
        detect_log_path(&actual)
    } else {
        log_path.to_path_buf()
    };
    Routed {
        provider: actual,
        log_path,
        line,
    }
}

pub fn write_hook_status_audit(provider: &str, line: &Value) {
    let Value::Object(obj) = line else { return };
    let Some(record) = parse_log_object(provider, obj) else {
        return;
    };
    let metadata = StatusMetadata {
        cwd: record.cwd.clone(),
        title: title_from_event(&record),
        origin: None,
    };
    let status = status_from_event(&record, &metadata);
    append_status_audit_record(&record, status.as_ref());
}

/// `hook_log_main(provider, log_path)`; always succeeds so agents are never blocked.
pub fn hook_log_main(provider: &str, log_path: &Path, stdin_bytes: &[u8]) {
    let Ok(text) = std::str::from_utf8(stdin_bytes) else {
        return;
    };
    // Python reads stdin in text mode, which applies universal-newline translation.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let env: Env = std::env::vars().collect();
    // SAFETY: getppid has no preconditions.
    let parent = unsafe { libc::getppid() };
    let routed = routed_hook_payload(provider, log_path, &text, &env, parent);
    send_hook_event(
        &routed.provider,
        &routed.line,
        &default_event_socket_path(),
        HOOK_EVENT_SEND_TIMEOUT,
    );
    if append_json_record(
        &routed.log_path,
        &routed.line,
        HOOK_LOG_MAX_BYTES,
        HOOK_LOG_BACKUP_COUNT,
    )
    .is_err()
    {
        return;
    }
    write_hook_status_audit(&routed.provider, &routed.line);
}

/// Parse `--provider X --log Y` like `hook_entry.main`; `None` means "exit 0 quietly".
pub fn parse_args(args: &[String]) -> Option<(String, PathBuf)> {
    let value_after = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    Some((
        value_after("--provider")?,
        expanduser(&value_after("--log")?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_claude_and_codex_payloads() {
        let line = format_hook_payload(
            "claude",
            r#"{"hook_event_name":"Stop","session_id":"s"}"#,
            "T",
        );
        assert_eq!(
            crate::pyjson::compact(&line),
            r#"{"hook_event_name":"Stop","session_id":"s","logged_at":"T"}"#
        );
        let line = format_hook_payload("codex", r#"{"hook_event_name":"Stop"}"#, "T");
        assert_eq!(
            crate::pyjson::compact(&line),
            r#"{"logged_at":"T","event":{"hook_event_name":"Stop"}}"#
        );
        let line = format_hook_payload("claude", "", "T");
        assert_eq!(crate::pyjson::compact(&line), r#"{"logged_at":"T"}"#);
        let line = format_hook_payload("claude", "[1]", "T");
        assert_eq!(
            crate::pyjson::compact(&line),
            r#"{"logged_at":"T","event":[1]}"#
        );
        let line = format_hook_payload("claude", "{bad", "T");
        assert_eq!(line["hook_event_name"], json!("ParseError"));
    }

    #[test]
    fn explicit_origin_is_not_overwritten() {
        let env: Env = vec![("SIDEPULSE_AGENT_ORIGIN".into(), "Bot".into())];
        let r = routed_hook_payload(
            "claude",
            Path::new("/tmp/x"),
            r#"{"hook_event_name":"Stop","agent_origin":"Keep"}"#,
            &env,
            0,
        );
        assert_eq!(r.line["agent_origin"], json!("Keep"));
        assert!(r.line.get("agent_origin_kind").is_none());
        let r = routed_hook_payload(
            "claude",
            Path::new("/tmp/x"),
            r#"{"hook_event_name":"Stop"}"#,
            &env,
            0,
        );
        assert_eq!(r.line["agent_origin"], json!("Bot"));
        assert_eq!(r.line["agent_origin_confidence"], json!("explicit"));
    }

    #[test]
    fn parses_args() {
        let args: Vec<String> = ["--provider", "claude", "--log", "/tmp/c.jsonl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_args(&args),
            Some(("claude".into(), PathBuf::from("/tmp/c.jsonl")))
        );
        assert_eq!(parse_args(&["--provider".into()]), None);
    }
}
