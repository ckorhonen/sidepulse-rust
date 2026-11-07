//! Event socket server: port of `HookEventServer` in `sidepulse/ipc.py`.
//! One JSON message `{"provider", "line"}` per connection, read to EOF, at most 1 MiB.
//! Unlike the Python version it blocks in `accept` instead of waking every 0.5 s.

use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

use sidepulse_core::ipc::MAX_EVENT_BYTES;

pub fn parse_message(bytes: &[u8]) -> Option<(String, Value)> {
    let Ok(Value::Object(mut msg)) = serde_json::from_slice::<Value>(bytes) else {
        return None;
    };
    let provider = match msg.remove("provider") {
        Some(Value::String(p)) => p,
        _ => return None,
    };
    match msg.remove("line") {
        Some(line @ Value::Object(_)) => Some((provider, line)),
        _ => None,
    }
}

fn read_message(mut conn: UnixStream) -> Option<(String, Value)> {
    // macOS returns EINVAL here when the peer already closed (the hook writes and closes at
    // once); the data is still readable, so the error must not drop the message.
    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = Vec::with_capacity(4096);
    let read = (&mut conn)
        .take(MAX_EVENT_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if read > MAX_EVENT_BYTES {
        return None;
    }
    parse_message(&buf)
}

/// Bind `socket_path` (replacing a stale socket) and deliver each message to `on_event`
/// from a background thread.
pub fn start<F>(socket_path: &Path, on_event: F) -> std::io::Result<(PathBuf, JoinHandle<()>)>
where
    F: Fn(String, Value) + Send + 'static,
{
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(socket_path)?;
    let path = socket_path.to_path_buf();
    let handle = std::thread::Builder::new()
        .name("sidepulse-events".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(conn) => {
                        if let Some((provider, line)) = read_message(conn) {
                            on_event(provider, line);
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })?;
    Ok((path, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::mpsc;

    #[test]
    fn receives_messages_from_the_hook_client() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("events.sock");
        std::fs::write(&sock, b"stale").unwrap();
        let (tx, rx) = mpsc::channel();
        start(&sock, move |p, l| tx.send((p, l)).unwrap()).unwrap();
        let line = json!({"hook_event_name": "Stop", "session_id": "s"});
        assert!(sidepulse_core::ipc::send_hook_event(
            "claude",
            &line,
            &sock,
            Duration::from_millis(500)
        ));
        let (provider, got) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(provider, "claude");
        assert_eq!(got, line);
    }

    #[test]
    fn rejects_malformed_messages() {
        assert!(parse_message(b"{bad").is_none());
        assert!(parse_message(br#"{"provider": 1, "line": {}}"#).is_none());
        assert!(parse_message(br#"{"provider": "claude", "line": []}"#).is_none());
    }
}
