# Agent rules for sidepulse-rust

## What this is

A Rust port of SidePulse components. The Python project
[inteliwear/sidepulse](https://github.com/inteliwear/sidepulse) is the reference implementation, and
behavior must stay output-compatible with it unless a deliberate difference is documented in the README.

## Layout

- `src/bin/sidepulse-hook.rs` is the hook entry point. It must always exit 0 and print nothing.
- `src/hook.rs` handles payload formatting, routing and the main flow (`hook.py`).
- `src/origin.rs` detects which app launched the agent (`origin.py`), using native process
  ancestry with no subprocesses.
- `src/providers.rs` holds event names, payload normalization and log-path detection (`providers.py`).
- `src/collector.rs` holds the per-event status rules (the matching parts of `collector.py`).
- `src/audit.rs`, `src/logrot.rs` and `src/ipc.rs` port the audit record, rotating JSONL appends
  and the event-socket client.
- `src/pyjson.rs` reproduces Python's JSON and string semantics so records match byte for byte.
- `scripts/parity.py` is the differential harness against the Python hook.

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
6. **Unsafe code** is allowed only for libc system calls. Each block needs a `// SAFETY:` comment
   explaining why the call is sound.
7. **Scope.** Keep changes small and focused, and follow the phase plan in the README. Update the
   README's tables and commands whenever behavior, performance claims or setup steps change.
