//! Port of `sidepulse/origin.py`.
//!
//! Process ancestry is read with `proc_pidinfo` + `sysctl(KERN_PROCARGS2)` instead of
//! spawning `/bin/ps` once per ancestor, which dominated the Python hook's cost.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::models::provider_label;
use crate::pyjson::{clean_label, or, Object};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentOrigin {
    pub kind: String,
    pub label: String,
    pub source: String,
    pub confidence: String,
}

impl AgentOrigin {
    fn new(kind: String, label: String, source: &str, confidence: &str) -> Self {
        Self {
            kind,
            label,
            source: source.into(),
            confidence: confidence.into(),
        }
    }

    pub fn payload_fields(&self) -> [(&'static str, String); 4] {
        [
            ("agent_origin", self.label.clone()),
            ("agent_origin_kind", self.kind.clone()),
            ("agent_origin_source", self.source.clone()),
            ("agent_origin_confidence", self.confidence.clone()),
        ]
    }
}

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: i32,
    pub ppid: Option<i32>,
    pub comm: String,
    pub command: String,
}

pub type Env = Vec<(String, String)>;

fn env_get<'a>(env: &'a Env, key: &str) -> Option<&'a str> {
    env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

pub fn detect_agent_origin(provider: &str, env: &Env, parent_pid: i32) -> AgentOrigin {
    let provider = provider.to_lowercase();
    if let Some(origin) = explicit_origin_from_env(env) {
        return origin;
    }
    if let Some(origin) = origin_from_environment(&provider, env) {
        return origin;
    }
    let processes = process_ancestry(parent_pid, 10);
    if let Some(origin) = origin_from_processes(&provider, &processes) {
        return origin;
    }
    if terminal_environment(env) {
        return surface_origin(&provider, "cli", "env:TERM_PROGRAM");
    }
    AgentOrigin::new(
        format!("{provider}_unknown"),
        provider_label(&provider),
        "fallback:provider",
        "unknown",
    )
}

fn explicit_origin_from_env(env: &Env) -> Option<AgentOrigin> {
    let label = clean_label(
        env_get(env, "SIDEPULSE_AGENT_ORIGIN")
            .map(|s| Value::String(s.into()))
            .as_ref(),
    )?;
    let kind = clean_label(
        env_get(env, "SIDEPULSE_AGENT_ORIGIN_KIND")
            .map(|s| Value::String(s.into()))
            .as_ref(),
    )
    .unwrap_or_else(|| normalize_kind(&label));
    Some(AgentOrigin::new(
        kind,
        label,
        "env:SIDEPULSE_AGENT_ORIGIN",
        "explicit",
    ))
}

fn origin_from_environment(provider: &str, env: &Env) -> Option<AgentOrigin> {
    let term_program = env_get(env, "TERM_PROGRAM")
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if term_program == "vscode" || env.iter().any(|(k, _)| k.starts_with("VSCODE_")) {
        return Some(surface_origin(provider, "vscode", "env:VSCODE"));
    }
    let bundle_id = env_get(env, "__CFBundleIdentifier")
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !bundle_id.is_empty() {
        let app = match provider {
            "codex" => ["openai", "chatgpt", "codex"]
                .iter()
                .any(|p| bundle_id.contains(p)),
            "claude" => bundle_id.contains("anthropic"),
            "grok" => bundle_id.contains("grok"),
            _ => false,
        };
        if app {
            return Some(surface_origin(provider, "app", "env:__CFBundleIdentifier"));
        }
    }
    None
}

pub fn origin_from_processes(provider: &str, processes: &[ProcessInfo]) -> Option<AgentOrigin> {
    let haystack = processes
        .iter()
        .map(|p| format!("{}\n{}", p.comm, p.command))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    let has = |t: &str| haystack.contains(t);

    if has("visual studio code.app") || has("code helper") || has("vscode") {
        return Some(surface_origin(
            provider,
            "vscode",
            "process:Visual Studio Code",
        ));
    }
    if has("cursor.app") || has("cursor helper") {
        return Some(surface_origin(provider, "cursor", "process:Cursor"));
    }
    if has("windsurf.app") || has("windsurf helper") {
        return Some(surface_origin(provider, "windsurf", "process:Windsurf"));
    }
    if provider == "codex" && (has("codex.app") || has("chatgpt.app")) {
        return Some(surface_origin(provider, "app", "process:Codex.app"));
    }
    if provider == "claude" && has("claude.app") {
        return Some(surface_origin(provider, "app", "process:Claude.app"));
    }
    if provider == "grok" && has("grok.app") {
        return Some(surface_origin(provider, "app", "process:Grok.app"));
    }
    for info in processes {
        let base = process_basename(info);
        match (provider, base.as_str()) {
            ("codex", "codex") => return Some(surface_origin(provider, "cli", "process:codex")),
            ("claude", "claude" | "claude-code") => {
                return Some(surface_origin(provider, "cli", "process:claude"))
            }
            ("grok", "grok") => return Some(surface_origin(provider, "cli", "process:grok")),
            _ => {}
        }
    }
    None
}

pub fn surface_origin(provider: &str, surface: &str, source: &str) -> AgentOrigin {
    let label = match (provider, surface) {
        ("codex", "app") => "Codex UI",
        ("codex", "cli") => "Codex CLI",
        ("codex", "vscode") => "Codex in VS Code",
        ("codex", "cursor") => "Codex in Cursor",
        ("codex", "windsurf") => "Codex in Windsurf",
        ("codex", "transcript") => "Codex Transcript",
        ("claude", "app") => "Claude App",
        ("claude", "cli") => "Claude Code CLI",
        ("claude", "vscode") => "Claude in VS Code",
        ("claude", "cursor") => "Claude in Cursor",
        ("claude", "windsurf") => "Claude in Windsurf",
        ("claude", "transcript") => "Claude Transcript",
        ("grok", "app") => "Grok App",
        ("grok", "cli") => "Grok CLI",
        ("grok", "vscode") => "Grok in VS Code",
        ("grok", "cursor") => "Grok in Cursor",
        ("grok", "windsurf") => "Grok in Windsurf",
        ("grok", "transcript") => "Grok Transcript",
        _ => "",
    };
    let label = if label.is_empty() {
        provider_label(provider)
    } else {
        label.to_string()
    };
    AgentOrigin::new(format!("{provider}_{surface}"), label, source, "inferred")
}

pub fn process_ancestry(pid: i32, limit: usize) -> Vec<ProcessInfo> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut current = pid;
    while current > 1 && seen.insert(current) && result.len() < limit {
        let Some(info) = process_info(current) else {
            break;
        };
        let next = info.ppid;
        result.push(info);
        match next {
            Some(p) => current = p,
            None => break,
        }
    }
    result
}

/// Equivalent of `ps -p PID -o pid= -o ppid= -o comm= -o command=` on macOS.
#[cfg(target_os = "macos")]
pub fn process_info(pid: i32) -> Option<ProcessInfo> {
    use std::mem::{size_of, MaybeUninit};
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: fully initialised by the successful call above.
    let info = unsafe { info.assume_init() };
    let p_comm = c_chars_to_string(&info.pbi_comm);
    let (comm, command) = match process_args(pid) {
        Some(args) if !args.is_empty() => (args[0].clone(), args.join(" ")),
        // ps prints "(name)" when it cannot read the arguments.
        _ => (format!("({p_comm})"), format!("({p_comm})")),
    };
    Some(ProcessInfo {
        pid,
        ppid: Some(info.pbi_ppid as i32),
        comm,
        command,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn process_info(_pid: i32) -> Option<ProcessInfo> {
    None
}

#[cfg(target_os = "macos")]
fn c_chars_to_string(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// argv of a process via `sysctl(KERN_PROCARGS2)`.
#[cfg(target_os = "macos")]
fn process_args(pid: i32) -> Option<Vec<String>> {
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: valid mib and output buffer of the advertised size.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut argmax as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let mut buf = vec![0u8; argmax.max(0) as usize];
    let mut len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: buffer is `len` bytes; the kernel updates `len` with the bytes written.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buf.truncate(len);
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?).max(0) as usize;
    let mut rest = &buf[4..];
    // Skip the exec path and the NUL padding that follows it.
    let exec_end = rest.iter().position(|b| *b == 0)?;
    rest = &rest[exec_end..];
    let start = rest.iter().position(|b| *b != 0)?;
    rest = &rest[start..];
    let args: Vec<String> = rest
        .split(|b| *b == 0)
        .take(argc)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    Some(args)
}

pub fn process_basename(info: &ProcessInfo) -> String {
    let mut candidates = vec![info.comm.clone()];
    if let Some(first) = shlex::split(&info.command).and_then(|v| v.into_iter().next()) {
        candidates.push(first);
    }
    for value in candidates {
        let name = std::path::Path::new(&value)
            .file_name()
            .map(|n| n.to_string_lossy().trim().to_lowercase())
            .unwrap_or_default();
        if !name.is_empty()
            && !["sh", "bash", "zsh", "python", "python3", "env"].contains(&name.as_str())
        {
            return name;
        }
    }
    String::new()
}

fn terminal_environment(env: &Env) -> bool {
    let term = env_get(env, "TERM_PROGRAM").unwrap_or("").trim();
    !term.is_empty() && term.to_lowercase() != "vscode"
}

/// `annotate_payload_with_origin(provider, payload)`; `detect` is called lazily.
pub fn annotate_payload_with_origin(
    payload: &Object,
    detect: &mut dyn FnMut() -> AgentOrigin,
) -> Object {
    if payload.contains_key("agent_origin") || payload.contains_key("agentOrigin") {
        return payload.clone();
    }
    let mut result = payload.clone();
    for (key, value) in detect().payload_fields() {
        result.insert(key.into(), Value::String(value));
    }
    result
}

pub fn origin_label_from_payload(provider: &str, raw: &Object) -> Option<String> {
    for key in [
        "agent_origin",
        "agentOrigin",
        "agent_origin_label",
        "origin_label",
    ] {
        if let Some(label) = clean_label(raw.get(key)) {
            return Some(label);
        }
    }
    match or(raw.get("sidepulse_origin"), raw.get("sidepulseOrigin")) {
        Some(Value::Object(structured)) => {
            for key in ["label", "name", "origin"] {
                if let Some(label) = clean_label(structured.get(key)) {
                    return Some(label);
                }
            }
        }
        Some(v @ Value::String(_)) => {
            if let Some(label) = clean_label(Some(v)) {
                return Some(label);
            }
        }
        _ => {}
    }
    let source = clean_label(raw.get("source"));
    if let Some(source) = source.filter(|s| s == "codex-transcripts" || s == "claude-transcripts") {
        return Some(surface_origin(provider, "transcript", &format!("source:{source}")).label);
    }
    None
}

pub fn normalize_kind(label: &str) -> String {
    static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
    let lowered = label.trim().to_lowercase();
    let normalized = NON_ALNUM.replace_all(&lowered, "_");
    let normalized = normalized.trim_matches('_');
    if normalized.is_empty() {
        "custom".into()
    } else {
        normalized.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn explicit_env_wins() {
        let o = detect_agent_origin(
            "claude",
            &env(&[("SIDEPULSE_AGENT_ORIGIN", "  My  Bot ")]),
            0,
        );
        assert_eq!(
            (o.label.as_str(), o.kind.as_str(), o.confidence.as_str()),
            ("My Bot", "my_bot", "explicit")
        );
    }

    #[test]
    fn vscode_and_bundle_detection() {
        assert_eq!(
            detect_agent_origin("codex", &env(&[("VSCODE_PID", "1")]), 0).label,
            "Codex in VS Code"
        );
        let o = detect_agent_origin(
            "claude",
            &env(&[("__CFBundleIdentifier", "com.anthropic.claudefordesktop")]),
            0,
        );
        assert_eq!(o.label, "Claude App");
    }

    #[test]
    fn process_heuristics() {
        let procs = vec![
            ProcessInfo {
                pid: 3,
                ppid: Some(2),
                comm: "/bin/zsh".into(),
                command: "-zsh".into(),
            },
            ProcessInfo {
                pid: 2,
                ppid: Some(1),
                comm: "claude".into(),
                command: "claude --resume".into(),
            },
        ];
        assert_eq!(
            origin_from_processes("claude", &procs).unwrap().label,
            "Claude Code CLI"
        );
    }

    #[test]
    fn reads_own_process() {
        let me = process_info(std::process::id() as i32).expect("own process readable");
        assert!(me.ppid.unwrap_or(0) > 0);
        assert!(!me.command.is_empty());
    }

    #[test]
    fn fallback_unknown() {
        let o = detect_agent_origin("grok", &env(&[]), 0);
        assert_eq!(
            (o.kind.as_str(), o.label.as_str(), o.source.as_str()),
            ("grok_unknown", "Grok", "fallback:provider")
        );
    }
}
