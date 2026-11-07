# Agent rules for sidepulse-rust

## What this is

A Rust port of SidePulse components. The Python project
[inteliwear/sidepulse](https://github.com/inteliwear/sidepulse) is the reference implementation, and
behavior must stay output-compatible with it unless a deliberate difference is documented in the README.

## Layout

This is a Cargo workspace. `crates/sidepulse-core` holds the shared logic, `crates/sidepulse-hook`
the per-event hook binary, and `crates/sidepulse-bar` the AppKit menu-bar app. Keep AppKit out of
core and the hook, so the hook stays small and fast to start.

Core modules (`crates/sidepulse-core/src/`):

- `crates/sidepulse-hook/src/main.rs` is the hook entry point. It must always exit 0 and print nothing.
- `hook.rs` handles payload formatting, routing and the main flow (`hook.py`).
- `origin.rs` detects which app launched the agent (`origin.py`), using native process
  ancestry with no subprocesses.
- `providers.rs` holds event names, payload normalization and log-path detection (`providers.py`).
- `collector.rs` holds the per-event status rules (the matching parts of `collector.py`).
- `audit.rs`, `logrot.rs` and `ipc.rs` port the audit record, rotating JSONL appends
  and the event-socket client.
- `pyjson.rs` reproduces Python's JSON and string semantics so records match byte for byte.
- `monitor.rs` holds the live agent state: ingest, snapshot, aggregate and a bounded `latest.json`.
- `devices.rs`, `battery.rs`, `lid.rs` and `power.rs` handle LED programs and writes, battery and
  lid reads, and keep-awake and keepalive, all native with no subprocesses.

Bar modules (`crates/sidepulse-bar/src/`):

- `app.rs` holds the main-thread state and refresh logic.
- `ui.rs` covers the status item and menu.
- `led.rs` is the LED worker thread.
- `server.rs` is the event socket.
- `session.rs` handles session rows and open actions.
- `settings.rs` reads and writes `settings.json` and keeps keys this port doesn't use.

`scripts/parity.py` is the differential harness against the Python hook.

## Efficiency rules

- Don't add polling where a notification or event exists. Every timer needs a reason and a
  tolerance.
- Don't spawn subprocesses on hot or periodic paths. Use IOKit, IOPM or libc.
- Keep state bounded, and debounce writes to disk.
- Never do I/O to the SD card on the main thread.
- Measure the RSS/footprint and CPU of a release build before and after any change that claims a
  performance gain, and update the README tables.

## Commands

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

All four must pass before a commit. CI (`.github/workflows/ci.yml`) runs them on macOS.

## Rules

1. **Parity first.** When you port or change behavior, cite the Python function it mirrors in a doc
   comment. Add a unit test for the edge case, and run `scripts/parity.py` against a local Python
   SidePulse install when output could change. Document any intentional difference in the README's
   Compatibility section.
2. **The hook must never block or break the agent.** No panics reach the user (the panic hook is
   silenced and `main` catches unwinds), nothing goes to stdout or stderr, the exit status is
   always 0, and every network or socket step has a timeout (currently 200 ms).
3. **Keep it fast.** The hook runs on every agent event. Don't spawn subprocesses on the hot path,
   compile regexes lazily (`LazyLock`), and avoid reading large files. Re-measure with
   `scripts/parity.py` when a change could affect per-event cost.
4. **Don't add dependencies lightly.** Prefer std and the existing crates (`serde_json` with
   `preserve_order`, `regex`, `chrono`, `libc`, `shlex`, `socket2`). Explain any new dependency in
   the PR.
5. **Privacy.** Hook payloads contain prompts, file paths and tool output. Never commit real hook
   logs, corpora, parity output or screenshots of them. Tests use synthetic data only.
6. **Unsafe code** is allowed only for system interfaces: libc, IOKit, CoreFoundation and IOPM FFI
   (`crates/sidepulse-core/src/power/iokit.rs`), and objc2/AppKit calls in `sidepulse-bar`. Each
   block needs a `// SAFETY:` comment explaining why the call is sound.
7. **Scope.** Keep changes small and focused, and follow the phase plan in the README. Update the
   README's tables and commands whenever behavior, performance claims or setup steps change.
