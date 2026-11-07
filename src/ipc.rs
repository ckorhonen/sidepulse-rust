//! Client side of `sidepulse/ipc.py`: one JSON message per connection on `events.sock`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use socket2::{Domain, SockAddr, Socket, Type};

use crate::providers::default_state_dir;
use crate::pyjson::compact;

pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
pub const HOOK_EVENT_SEND_TIMEOUT: Duration = Duration::from_millis(200);

pub fn default_event_socket_path() -> PathBuf {
    default_state_dir().join("events.sock")
}

pub fn send_hook_event(
    provider: &str,
    line: &Value,
    socket_path: &Path,
    timeout: Duration,
) -> bool {
    let payload = compact(&json!({"provider": provider, "line": line}));
    if payload.len() > MAX_EVENT_BYTES {
        return false;
    }
    let attempt = || -> std::io::Result<()> {
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        socket.connect_timeout(&SockAddr::unix(socket_path)?, timeout)?;
        socket.set_write_timeout(Some(timeout))?;
        let mut stream: std::os::unix::net::UnixStream = socket.into();
        stream.write_all(payload.as_bytes())
    };
    attempt().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    #[test]
    fn sends_one_message_per_connection() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("events.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let line = json!({"hook_event_name": "Stop", "é": 1});
        assert!(send_hook_event(
            "claude",
            &line,
            &sock,
            HOOK_EVENT_SEND_TIMEOUT
        ));
        let (mut conn, _) = listener.accept().unwrap();
        let mut body = String::new();
        conn.read_to_string(&mut body).unwrap();
        assert_eq!(
            body,
            r#"{"provider":"claude","line":{"hook_event_name":"Stop","é":1}}"#
        );
    }

    #[test]
    fn missing_socket_fails_fast() {
        let start = std::time::Instant::now();
        assert!(!send_hook_event(
            "claude",
            &json!({}),
            Path::new("/nonexistent/events.sock"),
            HOOK_EVENT_SEND_TIMEOUT
        ));
        assert!(start.elapsed() < Duration::from_millis(100));
    }
}
