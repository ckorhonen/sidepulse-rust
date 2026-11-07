//! Port of `LiveAgentMonitor` and the snapshot logic in `sidepulse/collector.py`.
//!
//! Differences from Python, all deliberate:
//!
//! - State is bounded. Statuses, per-session/per-status metadata and pending permission
//!   signatures older than [`MonitorConfig::retain_for`] are dropped, and at most
//!   [`MonitorConfig::max_statuses`] of each are kept (newest first). Python keeps everything,
//!   which grew `latest.json` to ~10k statuses.
//! - [`LiveMonitor::ingest`] never touches the disk. It marks the monitor dirty and the caller
//!   debounces [`LiveMonitor::flush`], which writes compact JSON (Python reads it with
//!   `json.loads`, so this stays compatible). Python rewrote the file with `indent=2` per event.
//! - Metadata maps start empty after a reload, as in Python (they are not persisted).
//! - Python quirk kept for parity: a `PostToolUse` `WORKING` status older than
//!   [`POST_TOOL_WORKING_VISIBLE_SECONDS`] is shown as `COMPLETED`, and because that is already
//!   past [`COMPLETED_VISIBLE_SECONDS`] it is immediately stale (see `status_for_snapshot`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use crate::collector::{
    display_name_from_parts, project_name, read_recent_lines, status_from_event, title_from_event,
    StatusMetadata,
};
use crate::models::{isoformat, parse_datetime, AgentMode, AgentStatus, HookEvent};
use crate::origin::origin_label_from_payload;
use crate::providers::{detect_log_path, parse_log_object};
use crate::pyjson::{py_str, truthy, Object};

/// `COMPLETED_VISIBLE_SECONDS`.
pub const COMPLETED_VISIBLE_SECONDS: i64 = 90;
/// `IDLE_VISIBLE_SECONDS`: idle statuses are stale as soon as they have any age.
pub const IDLE_VISIBLE_SECONDS: i64 = 0;
/// `POST_TOOL_WORKING_VISIBLE_SECONDS`.
pub const POST_TOOL_WORKING_VISIBLE_SECONDS: i64 = 120;
/// Providers replayed at start-up (`replay_recent_debug_logs`).
pub const REPLAY_PROVIDERS: [&str; 3] = ["codex", "claude", "grok"];
/// Cap on pending permission signatures per status key (unbounded in Python).
const MAX_PENDING_PER_KEY: usize = 32;

/// `AggregateStatus`.
#[derive(Clone, Debug)]
pub struct Aggregate {
    pub mode: AgentMode,
    pub active_count: usize,
    pub stale_count: usize,
    pub representative: Option<AgentStatus>,
}

/// `MonitorSnapshot` without `sources`/`collected_at`.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Fresh statuses, sorted by (priority, newest first).
    pub statuses: Vec<AgentStatus>,
    /// Stale statuses, same order. Always populated (`include_stale` is a no-op in Python).
    pub stale: Vec<AgentStatus>,
    pub aggregate: Aggregate,
}

#[derive(Clone, Debug)]
pub struct MonitorConfig {
    /// `stale_after_seconds` for modes other than completed/idle.
    pub stale_after: Duration,
    /// Drop stored statuses and metadata older than this.
    pub retain_for: Duration,
    /// Keep at most this many statuses (and metadata entries per map).
    pub max_statuses: usize,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            stale_after: Duration::seconds(3600),
            retain_for: Duration::hours(6),
            max_statuses: 256,
        }
    }
}

#[derive(Clone, Debug)]
struct MetadataEntry {
    metadata: StatusMetadata,
    last_seen: DateTime<Utc>,
}

/// `LiveAgentMonitor`, with bounded state and debounced persistence.
#[derive(Debug)]
pub struct LiveMonitor {
    state_path: PathBuf,
    config: MonitorConfig,
    statuses: HashMap<String, AgentStatus>,
    metadata_by_session: HashMap<String, MetadataEntry>,
    metadata_by_status: HashMap<String, MetadataEntry>,
    pending_permissions: HashMap<String, Vec<String>>,
    dirty: bool,
}

impl LiveMonitor {
    /// `LiveAgentMonitor.__init__` + `load_latest_state`. Load errors are ignored.
    pub fn new(state_path: PathBuf, config: MonitorConfig) -> Self {
        Self::new_at(state_path, config, Utc::now())
    }

    /// [`LiveMonitor::new`] with an explicit clock for retention pruning.
    pub fn new_at(state_path: PathBuf, config: MonitorConfig, now: DateTime<Utc>) -> Self {
        let mut monitor = Self {
            state_path,
            config,
            statuses: HashMap::new(),
            metadata_by_session: HashMap::new(),
            metadata_by_status: HashMap::new(),
            pending_permissions: HashMap::new(),
            dirty: false,
        };
        monitor.load_latest_state(now);
        monitor
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn config(&self) -> &MonitorConfig {
        &self.config
    }

    /// Number of stored (bounded) statuses.
    pub fn len(&self) -> usize {
        self.statuses.len()
    }

    pub fn is_empty(&self) -> bool {
        self.statuses.is_empty()
    }

    /// `LiveAgentMonitor.ingest_record`, minus the file write. Returns true when a status
    /// was stored (and the monitor marked dirty).
    pub fn ingest(&mut self, record: HookEvent) -> bool {
        self.ingest_at(record, Utc::now())
    }

    /// [`LiveMonitor::ingest`] with an explicit clock for retention pruning.
    pub fn ingest_at(&mut self, record: HookEvent, now: DateTime<Utc>) -> bool {
        let metadata = self.metadata_for_record(&record);
        let stored = self.store_status(&record, &metadata, now);
        self.prune(now);
        stored
    }

    fn store_status(
        &mut self,
        record: &HookEvent,
        metadata: &StatusMetadata,
        now: DateTime<Utc>,
    ) -> bool {
        let Some(status) = status_from_event(record, metadata) else {
            return false;
        };
        track_pending_permissions(record, &mut self.pending_permissions);
        if status.updated_at < now - self.config.retain_for {
            return false;
        }
        let previous = self.statuses.get(&status.agent_id);
        let pending = self
            .pending_permissions
            .get(&status.agent_id)
            .is_some_and(|p| !p.is_empty());
        if should_ignore_status_transition(previous, &status, pending) {
            return false;
        }
        self.statuses.insert(status.agent_id.clone(), status);
        self.dirty = true;
        true
    }

    /// `parse_log_line` on an already-decoded JSON line, then [`LiveMonitor::ingest`].
    pub fn ingest_line(&mut self, provider: &str, line: &Value) -> bool {
        match line
            .as_object()
            .and_then(|obj| parse_log_object(provider, obj))
        {
            Some(record) => self.ingest(record),
            None => false,
        }
    }

    /// `replay_recent_debug_logs`: last `max_lines_per_provider` lines of each provider's hook
    /// log (from `detect_log_path`). Returns the number of records parsed and ingested.
    pub fn replay_logs(&mut self, max_lines_per_provider: usize) -> usize {
        let sources: Vec<(&str, PathBuf)> = REPLAY_PROVIDERS
            .iter()
            .map(|p| (*p, detect_log_path(p)))
            .collect();
        self.replay_paths(&sources, max_lines_per_provider)
    }

    /// Replay explicit `(provider, log path)` pairs. Missing or unreadable logs are skipped.
    pub fn replay_paths(&mut self, sources: &[(&str, PathBuf)], max_lines: usize) -> usize {
        let mut replayed = 0;
        for (provider, path) in sources {
            let Ok(lines) = read_recent_lines(path, max_lines) else {
                continue;
            };
            for line in lines {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if let Some(record) = parse_log_object(provider, &obj) {
                    self.ingest(record);
                    replayed += 1;
                }
            }
        }
        replayed
    }

    /// `LiveAgentMonitor.snapshot` / `snapshot_from_statuses`. No I/O.
    pub fn snapshot(&self, now: DateTime<Utc>) -> Snapshot {
        snapshot_from_statuses(self.statuses.values(), now, self.config.stale_after)
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// `write_latest_state`, only when dirty. Atomic via `latest.json.tmp` + rename, compact
    /// JSON. Returns whether the file was written; on error the monitor stays dirty.
    pub fn flush(&mut self) -> std::io::Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        self.write_latest_state(Utc::now())?;
        self.dirty = false;
        Ok(true)
    }

    fn write_latest_state(&self, now: DateTime<Utc>) -> std::io::Result<()> {
        let mut statuses: Vec<&AgentStatus> = self.statuses.values().collect();
        statuses.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.agent_id.cmp(&b.agent_id))
        });
        let payload = json!({
            "updated_at": isoformat(&now),
            "statuses": statuses.iter().map(|s| status_to_value(s, now)).collect::<Vec<_>>(),
        });
        let mut text = serde_json::to_string(&payload).map_err(std::io::Error::other)?;
        text.push('\n');
        if let Some(parent) = self.state_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let temp = self.state_path.with_extension("json.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, &self.state_path)
    }

    /// `load_latest_state`, followed by bounding.
    fn load_latest_state(&mut self, now: DateTime<Utc>) {
        let Ok(text) = std::fs::read_to_string(&self.state_path) else {
            return;
        };
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let Some(Value::Array(items)) = data.get("statuses") else {
            return;
        };
        for item in items {
            if let Some(status) = status_from_value(item) {
                self.statuses.insert(status.agent_id.clone(), status);
            }
        }
        let before = self.statuses.len();
        self.prune(now);
        // A pruned file should be rewritten in its bounded form.
        if self.statuses.len() < before {
            self.dirty = true;
        }
        self.refresh_codex_titles();
    }

    /// The Codex title refresh in `agent_status_from_dict`, done after bounding so the session
    /// index is read at most once per kept Codex session.
    fn refresh_codex_titles(&mut self) {
        let mut titles: HashMap<String, Option<String>> = HashMap::new();
        for status in self.statuses.values_mut() {
            if status.provider != "codex" {
                continue;
            }
            let Some(session_id) = status.session_id.clone().filter(|s| !s.is_empty()) else {
                continue;
            };
            let title = titles
                .entry(session_id.clone())
                .or_insert_with(|| codex_title(&session_id))
                .clone();
            if let Some(title) = title {
                let short: String = session_id.chars().take(8).collect();
                status.display_name = display_name_from_parts(
                    project_name(status.cwd.as_deref()).as_deref(),
                    Some(&title),
                    &short,
                    &status.display_name,
                );
            }
        }
    }

    /// `metadata_for_record` + `update_metadata`. The title is computed once per record.
    fn metadata_for_record(&mut self, record: &HookEvent) -> StatusMetadata {
        let title = title_from_event(record);
        let origin = record
            .origin
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| origin_label_from_payload(&record.provider, &record.raw));
        let update = |map: &mut HashMap<String, MetadataEntry>, key: String| -> StatusMetadata {
            let entry = map.entry(key).or_insert_with(|| MetadataEntry {
                metadata: StatusMetadata::default(),
                last_seen: record.logged_at,
            });
            if entry.last_seen < record.logged_at {
                entry.last_seen = record.logged_at;
            }
            if let Some(cwd) = record.cwd.as_ref().filter(|s| !s.is_empty()) {
                entry.metadata.cwd = Some(cwd.clone());
            }
            if let Some(title) = &title {
                entry.metadata.title = Some(title.clone());
            }
            if let Some(origin) = &origin {
                entry.metadata.origin = Some(origin.clone());
            }
            entry.metadata.clone()
        };
        let session = record
            .session_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| {
                update(
                    &mut self.metadata_by_session,
                    format!("{}:session:{}", record.provider, s),
                )
            });
        let status = update(&mut self.metadata_by_status, record.status_key());
        match session {
            None => status,
            Some(session) => StatusMetadata {
                cwd: status.cwd.filter(|s| !s.is_empty()).or(session.cwd),
                title: status.title.filter(|s| !s.is_empty()).or(session.title),
                origin: status.origin.filter(|s| !s.is_empty()).or(session.origin),
            },
        }
    }

    /// Bound every map by `retain_for` and `max_statuses`.
    fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - self.config.retain_for;
        let max = self.config.max_statuses;
        let before = self.statuses.len();
        self.statuses.retain(|_, s| s.updated_at >= cutoff);
        evict_oldest(&mut self.statuses, max, |s| s.updated_at);
        if self.statuses.len() < before {
            self.dirty = true;
        }
        for map in [&mut self.metadata_by_session, &mut self.metadata_by_status] {
            map.retain(|_, e| e.last_seen >= cutoff);
            evict_oldest(map, max, |e| e.last_seen);
        }
        let statuses = &self.statuses;
        self.pending_permissions
            .retain(|key, pending| !pending.is_empty() && statuses.contains_key(key));
    }
}

/// Keep the `max` newest entries.
fn evict_oldest<T>(map: &mut HashMap<String, T>, max: usize, time: impl Fn(&T) -> DateTime<Utc>) {
    if map.len() <= max {
        return;
    }
    let mut times: Vec<(DateTime<Utc>, String)> =
        map.iter().map(|(k, v)| (time(v), k.clone())).collect();
    times.sort_unstable();
    let excess = map.len() - max;
    for (_, key) in times.into_iter().take(excess) {
        map.remove(&key);
    }
}

fn codex_title(session_id: &str) -> Option<String> {
    // `title_from_event` checks the Codex session index first; any event name other than
    // `UserPromptSubmit` makes it return only that title.
    let probe = HookEvent {
        provider: "codex".into(),
        logged_at: Utc::now(),
        event_name: String::new(),
        raw: Object::new(),
        session_id: Some(session_id.to_string()),
        turn_id: None,
        agent_id: None,
        cwd: None,
        tool_name: None,
        message: None,
        origin: None,
    };
    title_from_event(&probe)
}

fn age_seconds(status: &AgentStatus, now: DateTime<Utc>) -> f64 {
    let micros = (now - status.updated_at)
        .num_microseconds()
        .unwrap_or(i64::MAX);
    (micros as f64 / 1e6).max(0.0)
}

fn sort_key(status: &AgentStatus) -> (u8, std::cmp::Reverse<DateTime<Utc>>) {
    (status.mode.priority(), std::cmp::Reverse(status.updated_at))
}

/// `status_counts_active`.
pub fn status_counts_active(status: &AgentStatus) -> bool {
    !matches!(status.mode, AgentMode::Completed | AgentMode::IdleReady)
}

/// `status_is_stale` with tool-running expiry disabled (as the status bar configures it).
pub fn status_is_stale(status: &AgentStatus, now: DateTime<Utc>, stale_after: Duration) -> bool {
    let age = age_seconds(status, now);
    match status.mode {
        AgentMode::Completed => age > COMPLETED_VISIBLE_SECONDS as f64,
        AgentMode::IdleReady => age > IDLE_VISIBLE_SECONDS as f64,
        _ => age > stale_after.num_milliseconds() as f64 / 1000.0,
    }
}

/// `status_for_snapshot`: an old post-tool `WORKING` status is shown as `COMPLETED`.
pub fn status_for_snapshot(status: &AgentStatus, now: DateTime<Utc>) -> AgentStatus {
    let mut status = status.clone();
    if status.mode == AgentMode::Working
        && status.event_name == "PostToolUse"
        && age_seconds(&status, now) > POST_TOOL_WORKING_VISIBLE_SECONDS as f64
    {
        status.mode = AgentMode::Completed;
    }
    status
}

/// `snapshot_from_statuses`.
pub fn snapshot_from_statuses<'a>(
    statuses: impl IntoIterator<Item = &'a AgentStatus>,
    now: DateTime<Utc>,
    stale_after: Duration,
) -> Snapshot {
    let mut fresh = Vec::new();
    let mut stale = Vec::new();
    for status in statuses {
        let mut current = status_for_snapshot(status, now);
        current.stale = status_is_stale(&current, now, stale_after);
        if current.stale {
            stale.push(current);
        } else {
            fresh.push(current);
        }
    }
    if fresh.iter().any(status_counts_active) {
        let (active, inactive): (Vec<_>, Vec<_>) =
            fresh.into_iter().partition(status_counts_active);
        fresh = active;
        stale.extend(inactive.into_iter().map(|mut s| {
            s.stale = true;
            s
        }));
    }
    // Ties broken by key for a deterministic order (Python uses dict insertion order).
    let order = |a: &AgentStatus, b: &AgentStatus| {
        sort_key(a)
            .cmp(&sort_key(b))
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    };
    fresh.sort_by(order);
    stale.sort_by(order);
    let aggregate = aggregate_status(&fresh, stale.len());
    Snapshot {
        statuses: fresh,
        stale,
        aggregate,
    }
}

/// `aggregate_status`: representative is the minimum (priority, newest first).
pub fn aggregate_status(statuses: &[AgentStatus], stale_count: usize) -> Aggregate {
    let representative = statuses.iter().min_by_key(|s| sort_key(s)).cloned();
    Aggregate {
        mode: representative
            .as_ref()
            .map_or(AgentMode::IdleReady, |s| s.mode),
        active_count: statuses.iter().filter(|s| status_counts_active(s)).count(),
        stale_count,
        representative,
    }
}

/// `track_pending_permissions`.
fn track_pending_permissions(record: &HookEvent, pending: &mut HashMap<String, Vec<String>>) {
    let signature = permission_signature(record);
    let key = record.status_key();
    match (record.event_name.as_str(), signature) {
        ("PermissionRequest", Some(sig)) => {
            let set = pending.entry(key).or_default();
            if !set.contains(&sig) {
                set.push(sig);
                if set.len() > MAX_PENDING_PER_KEY {
                    set.remove(0);
                }
            }
        }
        ("PostToolUse", Some(sig)) => {
            if let Some(set) = pending.get_mut(&key) {
                set.retain(|s| *s != sig);
                if set.is_empty() {
                    pending.remove(&key);
                }
            }
        }
        ("Stop" | "SessionEnd" | "UserPromptSubmit", _) => {
            pending.remove(&key);
        }
        _ => {}
    }
}

/// `permission_signature`: tool name + NUL + command, when the tool input has a command.
pub fn permission_signature(record: &HookEvent) -> Option<String> {
    let raw = &record.raw;
    let tool_name = match raw.get("tool_name") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
        _ => record.tool_name.as_deref(),
    };
    let Some(Value::Object(input)) = raw.get("tool_input") else {
        return None;
    };
    match input.get("command") {
        Some(Value::String(command)) if !command.is_empty() => {
            Some(format!("{}\0{}", tool_name.unwrap_or(""), command))
        }
        _ => None,
    }
}

/// `should_ignore_status_transition`.
pub fn should_ignore_status_transition(
    previous: Option<&AgentStatus>,
    current: &AgentStatus,
    has_pending_permissions: bool,
) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    if previous.mode == AgentMode::Completed && current.event_name == "Notification" {
        return true;
    }
    previous.mode == AgentMode::WaitingForInput
        && previous.event_name == "PermissionRequest"
        && current.event_name != "PermissionRequest"
        && has_pending_permissions
}

/// `AgentStatus.to_dict(now)`, same key order.
pub fn status_to_value(s: &AgentStatus, now: DateTime<Utc>) -> Value {
    let age = (age_seconds(s, now) * 1000.0).round() / 1000.0;
    json!({
        "provider": s.provider,
        "agent_id": s.agent_id,
        "display_name": s.display_name,
        "mode": s.mode.value(),
        "mode_label": s.mode.label(),
        "priority": s.mode.priority(),
        "updated_at": isoformat(&s.updated_at),
        "age_seconds": age,
        "event_name": s.event_name,
        "session_id": s.session_id,
        "cwd": s.cwd,
        "tool_name": s.tool_name,
        "message": s.message,
        "origin": s.origin,
        "stale": s.stale,
    })
}

/// `AggregateStatus.to_dict(now)`.
pub fn aggregate_to_value(a: &Aggregate, now: DateTime<Utc>) -> Value {
    json!({
        "mode": a.mode.value(),
        "mode_label": a.mode.label(),
        "active_count": a.active_count,
        "stale_count": a.stale_count,
        "representative": a.representative.as_ref().map(|s| status_to_value(s, now)),
    })
}

/// `agent_status_from_dict`, without the Codex title refresh (done in bulk on load).
pub fn status_from_value(v: &Value) -> Option<AgentStatus> {
    let data = v.as_object()?;
    let string_or_none = |key: &str| match data.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    let required = |key: &str| data.get(key).map(py_str);
    Some(AgentStatus {
        provider: required("provider")?,
        agent_id: required("agent_id")?,
        display_name: required("display_name")?,
        mode: AgentMode::from_value(&required("mode")?)?,
        updated_at: parse_datetime(Some(data.get("updated_at")?)),
        event_name: required("event_name")?,
        session_id: string_or_none("session_id"),
        cwd: string_or_none("cwd"),
        tool_name: string_or_none("tool_name"),
        message: string_or_none("message"),
        origin: string_or_none("origin"),
        stale: truthy(data.get("stale")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn base() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 7, 12, 0, 0).unwrap()
    }

    fn event(name: &str, session: &str, at: DateTime<Utc>, extra: Value) -> HookEvent {
        let mut obj = json!({
            "hook_event_name": name,
            "session_id": session,
            "logged_at": isoformat(&at),
        });
        if let (Value::Object(o), Value::Object(e)) = (&mut obj, extra) {
            o.extend(e);
        }
        parse_log_object("claude", obj.as_object().unwrap()).expect("event parses")
    }

    fn status(key: &str, mode: AgentMode, at: DateTime<Utc>, event: &str) -> AgentStatus {
        AgentStatus {
            provider: "claude".into(),
            agent_id: key.into(),
            display_name: key.into(),
            mode,
            updated_at: at,
            event_name: event.into(),
            session_id: None,
            cwd: None,
            tool_name: None,
            message: None,
            origin: None,
            stale: false,
        }
    }

    fn monitor(dir: &tempfile::TempDir) -> LiveMonitor {
        LiveMonitor::new_at(
            dir.path().join("latest.json"),
            MonitorConfig::default(),
            base(),
        )
    }

    fn bash(cmd: &str) -> Value {
        json!({"tool_name": "Bash", "tool_input": {"command": cmd}})
    }

    #[test]
    fn permission_request_holds_waiting_until_post_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = monitor(&dir);
        let t = base();
        assert!(m.ingest_at(event("PermissionRequest", "s", t, bash("rm x")), t));
        let snap = m.snapshot(t);
        assert_eq!(snap.aggregate.mode, AgentMode::WaitingForInput);

        let t1 = t + Duration::seconds(1);
        assert!(!m.ingest_at(event("PreToolUse", "s", t1, bash("rm x")), t1));
        assert!(!m.ingest_at(event("Notification", "s", t1, json!({})), t1));
        assert_eq!(m.snapshot(t1).aggregate.mode, AgentMode::WaitingForInput);

        let t2 = t + Duration::seconds(2);
        assert!(m.ingest_at(event("PostToolUse", "s", t2, bash("rm x")), t2));
        let snap = m.snapshot(t2);
        assert_eq!(snap.aggregate.mode, AgentMode::Working);
        assert!(m.pending_permissions.is_empty());
    }

    #[test]
    fn stop_clears_pending_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = monitor(&dir);
        let t = base();
        m.ingest_at(event("PermissionRequest", "s", t, bash("ls")), t);
        assert!(m.ingest_at(event("Stop", "s", t, json!({})), t));
        assert!(m.pending_permissions.is_empty());
        assert_eq!(m.snapshot(t).aggregate.mode, AgentMode::Completed);
    }

    #[test]
    fn notification_after_completed_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = monitor(&dir);
        let t = base();
        assert!(m.ingest_at(event("Stop", "s", t, json!({})), t));
        let ev = event(
            "Notification",
            "s",
            t + Duration::seconds(1),
            json!({"message": "Claude is waiting for your input"}),
        );
        assert!(!m.ingest_at(ev, t));
        assert_eq!(m.snapshot(t).aggregate.mode, AgentMode::Completed);
    }

    #[test]
    fn completed_hides_after_90_seconds() {
        let t = base();
        let s = [status("a", AgentMode::Completed, t, "Stop")];
        let at90 = snapshot_from_statuses(&s, t + Duration::seconds(90), Duration::hours(1));
        assert_eq!(at90.statuses.len(), 1);
        let at91 = snapshot_from_statuses(&s, t + Duration::seconds(91), Duration::hours(1));
        assert!(at91.statuses.is_empty());
        assert_eq!(at91.stale.len(), 1);
        assert!(at91.stale[0].stale);
        assert_eq!(at91.aggregate.mode, AgentMode::IdleReady);
        assert_eq!(at91.aggregate.stale_count, 1);
    }

    #[test]
    fn idle_is_stale_once_it_has_age() {
        let t = base();
        let s = [status("a", AgentMode::IdleReady, t, "SessionStart")];
        let snap = snapshot_from_statuses(&s, t + Duration::milliseconds(1), Duration::hours(1));
        assert!(snap.statuses.is_empty());
        assert_eq!(snap.stale.len(), 1);
    }

    #[test]
    fn working_goes_stale_after_stale_after() {
        let t = base();
        let s = [status("a", AgentMode::ToolRunning, t, "PreToolUse")];
        let hour = Duration::hours(1);
        assert_eq!(snapshot_from_statuses(&s, t + hour, hour).statuses.len(), 1);
        let later = t + hour + Duration::seconds(1);
        assert!(snapshot_from_statuses(&s, later, hour).statuses.is_empty());
    }

    #[test]
    fn active_statuses_push_inactive_ones_to_stale() {
        let t = base();
        let s = [
            status("done", AgentMode::Completed, t, "Stop"),
            status("busy", AgentMode::Working, t, "UserPromptSubmit"),
        ];
        let snap = snapshot_from_statuses(&s, t + Duration::seconds(5), Duration::hours(1));
        assert_eq!(snap.statuses.len(), 1);
        assert_eq!(snap.statuses[0].agent_id, "busy");
        assert_eq!(snap.stale.len(), 1);
        assert!(snap.stale[0].stale);
        assert_eq!(snap.aggregate.active_count, 1);
        assert_eq!(snap.aggregate.stale_count, 1);
    }

    #[test]
    fn post_tool_working_becomes_completed_then_stale() {
        // Python parity quirk: past 120 s the status reads COMPLETED, which is already past 90 s.
        let t = base();
        let s = [status("a", AgentMode::Working, t, "PostToolUse")];
        let at = t + Duration::seconds(121);
        let snap = snapshot_from_statuses(&s, at, Duration::hours(1));
        assert!(snap.statuses.is_empty());
        assert_eq!(snap.stale[0].mode, AgentMode::Completed);
        let fresh = snapshot_from_statuses(&s, t + Duration::seconds(60), Duration::hours(1));
        assert_eq!(fresh.aggregate.mode, AgentMode::Working);
    }

    #[test]
    fn aggregate_picks_min_priority_then_newest() {
        let t = base();
        let s = vec![
            status("work", AgentMode::Working, t + Duration::seconds(9), "x"),
            status("old-wait", AgentMode::WaitingForInput, t, "x"),
            status(
                "new-wait",
                AgentMode::WaitingForInput,
                t + Duration::seconds(5),
                "x",
            ),
        ];
        let agg = aggregate_status(&s, 2);
        assert_eq!(agg.mode, AgentMode::WaitingForInput);
        assert_eq!(agg.representative.unwrap().agent_id, "new-wait");
        assert_eq!(agg.active_count, 3);
        assert_eq!(agg.stale_count, 2);
        let empty = aggregate_status(&[], 0);
        assert_eq!(empty.mode, AgentMode::IdleReady);
        assert!(empty.representative.is_none());
    }

    #[test]
    fn snapshot_sorts_by_priority_then_newest() {
        let t = base();
        let s = [
            status("w1", AgentMode::Working, t, "x"),
            status("err", AgentMode::BlockedError, t, "x"),
            status("w2", AgentMode::Working, t + Duration::seconds(1), "x"),
        ];
        let snap = snapshot_from_statuses(&s, t + Duration::seconds(2), Duration::hours(1));
        let ids: Vec<_> = snap.statuses.iter().map(|s| s.agent_id.as_str()).collect();
        assert_eq!(ids, ["err", "w2", "w1"]);
    }

    #[test]
    fn latest_json_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/latest.json");
        let now = Utc::now();
        let mut m = LiveMonitor::new(path.clone(), MonitorConfig::default());
        assert!(!m.flush().unwrap());
        let ev = event(
            "PreToolUse",
            "sess-1",
            now,
            json!({"tool_name": "Bash", "agent_origin": "Codex UI"}),
        );
        assert!(m.ingest(ev));
        assert!(m.is_dirty());
        assert!(!path.exists(), "ingest must not write");
        assert!(m.flush().unwrap());
        assert!(!m.is_dirty());
        assert!(!dir.path().join("state/latest.json.tmp").exists());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\n "), "compact JSON");
        let data: Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<_> = data["statuses"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys[..4], ["provider", "agent_id", "display_name", "mode"]);
        assert_eq!(keys.len(), 15);

        let reloaded = LiveMonitor::new(path, MonitorConfig::default());
        let snap = reloaded.snapshot(now);
        assert_eq!(snap.aggregate.mode, AgentMode::ToolRunning);
        assert_eq!(snap.statuses[0].tool_name.as_deref(), Some("Bash"));
        assert_eq!(snap.statuses[0].agent_id, "claude:session:sess-1");
        assert!(!reloaded.is_dirty());
    }

    #[test]
    fn status_value_round_trip_and_rejects_bad_rows() {
        let t = base();
        let mut s = status("k", AgentMode::BlockedError, t, "StopFailure");
        s.cwd = Some("/tmp/p".into());
        let v = status_to_value(&s, t + Duration::milliseconds(1500));
        assert_eq!(v["age_seconds"], json!(1.5));
        assert_eq!(v["priority"], json!(1));
        assert_eq!(v["mode_label"], json!("Blocked / Error"));
        let back = status_from_value(&v).unwrap();
        assert_eq!(back.mode, AgentMode::BlockedError);
        assert_eq!(back.updated_at, t);
        assert_eq!(back.cwd.as_deref(), Some("/tmp/p"));
        assert!(status_from_value(&json!({"provider": "x"})).is_none());
        let mut bad = v.clone();
        bad["mode"] = json!("nope");
        assert!(status_from_value(&bad).is_none());
        assert!(status_from_value(&json!([1])).is_none());
    }

    #[test]
    fn bounding_drops_old_and_excess_statuses() {
        let dir = tempfile::tempdir().unwrap();
        let config = MonitorConfig {
            max_statuses: 3,
            ..MonitorConfig::default()
        };
        let mut m = LiveMonitor::new_at(dir.path().join("latest.json"), config, base());
        let t = base();
        // Older than retain_for: never stored.
        let old = t - Duration::hours(7);
        assert!(!m.ingest_at(event("UserPromptSubmit", "old", old, json!({})), t));
        assert!(m.is_empty());
        for i in 0..5 {
            let at = t + Duration::seconds(i);
            m.ingest_at(
                event("UserPromptSubmit", &format!("s{i}"), at, json!({})),
                at,
            );
        }
        assert_eq!(m.len(), 3);
        let mut keys: Vec<_> = m.statuses.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "claude:session:s2",
                "claude:session:s3",
                "claude:session:s4"
            ]
        );
        assert!(m.metadata_by_session.len() <= 3);
        assert!(m.metadata_by_status.len() <= 3);

        // Time passes: everything ages out of retain_for.
        let later = t + Duration::hours(7);
        m.ingest_at(event("UserPromptSubmit", "new", later, json!({})), later);
        assert_eq!(m.len(), 1);
        assert_eq!(m.metadata_by_status.len(), 1);
    }

    #[test]
    fn load_bounds_oversized_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latest.json");
        let t = base();
        let rows: Vec<Value> = (0..10)
            .map(|i| {
                let s = status(
                    &format!("k{i}"),
                    AgentMode::Working,
                    t - Duration::hours(i),
                    "x",
                );
                status_to_value(&s, t)
            })
            .collect();
        std::fs::write(
            &path,
            json!({"updated_at": "x", "statuses": rows}).to_string(),
        )
        .unwrap();
        let config = MonitorConfig {
            max_statuses: 4,
            ..MonitorConfig::default()
        };
        let m = LiveMonitor::new_at(path, config, t);
        // retain_for 6h keeps k0..k6, max_statuses keeps the 4 newest.
        let mut keys: Vec<_> = m.statuses.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["k0", "k1", "k2", "k3"]);
        assert!(m.is_dirty(), "pruned state should be rewritten");
    }

    #[test]
    fn pending_permissions_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = monitor(&dir);
        let t = base();
        for i in 0..50 {
            m.ingest_at(
                event("PermissionRequest", "s", t, bash(&format!("cmd{i}"))),
                t,
            );
        }
        assert_eq!(
            m.pending_permissions["claude:session:s"].len(),
            MAX_PENDING_PER_KEY
        );
    }

    #[test]
    fn replay_ignores_missing_logs_and_counts_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = monitor(&dir);
        let log = dir.path().join("codex.jsonl");
        let now = Utc::now();
        let line = json!({
            "logged_at": isoformat(&now),
            "event": {
                "hook_event_name": "UserPromptSubmit",
                "session_id": "eeeeeeee-ffff",
                "cwd": "/tmp/project",
                "prompt": "startup replay should restore this",
            },
        });
        std::fs::write(&log, format!("{line}\nnot json\n\n")).unwrap();
        let sources = [("claude", dir.path().join("missing.jsonl")), ("codex", log)];
        assert_eq!(m.replay_paths(&sources, 20), 1);
        let snap = m.snapshot(now);
        assert_eq!(snap.aggregate.mode, AgentMode::Working);
        assert!(snap.statuses[0].display_name.contains("startup replay"));
    }

    #[test]
    fn ingest_line_rejects_non_objects() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = monitor(&dir);
        assert!(!m.ingest_line("claude", &json!([1, 2])));
        assert!(!m.ingest_line("claude", &json!({"no": "event"})));
        let line = json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "x",
            "logged_at": isoformat(&Utc::now()),
        });
        assert!(m.ingest_line("claude", &line));
    }
}
