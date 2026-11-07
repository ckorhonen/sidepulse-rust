# sidepulse-rust

An unofficial native port of [SidePulse](https://github.com/inteliwear/sidepulse) agent-status
components to Rust. SidePulse shows what your coding agents (Claude Code, Codex, Grok) are doing on a
small LED device and in the macOS menu bar.

This project is not affiliated with or endorsed by Inteliwear. "SidePulse" refers to their product.
The Python project is the reference implementation, and this port stays output-compatible with it.

## Why

Agents run SidePulse's hook on every event: each prompt, every tool call before and after it runs, and
each stop. The agent waits for the hook to finish, so the hook's start-up cost is added to every tool
call. The Python hook takes about 95 ms per event (around 200 ms under load) and up to 25 MB, partly
because it runs `/bin/ps` once for every parent process to work out which app launched the agent.

The Rust hook does the same work in about 7 ms with about 6 MB of memory.

| Replaying 181 real and synthetic events | Python hook | Rust hook |
|---|---|---|
| Wall time, median / p95 | 95.5 / 198 ms | **6.8 / 12.7 ms** |
| CPU per event, median | 90.6 ms | **4.9 ms** |
| Peak memory, median | 23.6 MB | **6.2 MB** |

Measured on an M3 Max under heavy unrelated load. Expect lower absolute numbers on an idle machine.

## Status

| Phase | Component | Status |
|---|---|---|
| 1 | `sidepulse-hook`: drop-in replacement for `sidepulse/hook_entry.py` | Done |
| 2 | Status-bar core: event socket, bounded `latest.json`, lid and battery state via IOKit | Planned |
| 3 | Menu-bar UI and LED writer | Planned |

## Requirements

- macOS. Process ancestry uses `proc_pidinfo` and `sysctl(KERN_PROCARGS2)`. On other systems the
  crate builds, but origin detection falls back to environment variables.
- Rust 1.80 or newer.
- For the hook to update the menu bar, a running SidePulse app (the Python one for now) listening on
  `~/.local/state/sidepulse/agent-monitor/events.sock`. Without it, the hook still writes its logs.

## Build and install

```sh
cargo build --release
install -m 755 target/release/sidepulse-hook ~/.local/bin/sidepulse-hook
```

## Use it

The binary takes the same arguments as the Python hook, so you swap only the executable in your agent's
hook configuration. For Claude Code, in `~/.claude/settings.json`:

```json
{
  "type": "command",
  "command": "/Users/you/.local/bin/sidepulse-hook --provider claude --log /Users/you/.local/state/sidepulse/agent-monitor/claude.jsonl"
}
```

Use `--provider codex` or `--provider grok` for those agents. Claude Code reads hooks when a session
starts, so restart the session afterwards. Running `sidepulse setup` from the Python package rewrites
the hooks back to the Python command.

Try it by hand:

```sh
echo '{"hook_event_name":"Stop","session_id":"demo","last_assistant_message":"Done."}' \
  | XDG_STATE_HOME=/tmp/sp-demo target/release/sidepulse-hook --provider claude \
      --log /tmp/sp-demo/sidepulse/agent-monitor/claude.jsonl
cat /tmp/sp-demo/sidepulse/agent-monitor/claude.jsonl /tmp/sp-demo/sidepulse/agent-monitor/event-status.jsonl
```

## What the hook does

For each payload on stdin, in the same order as the Python hook:

1. Adds a `logged_at` timestamp and, for Codex, wraps the payload under `event`.
2. Routes Grok-style payloads that arrive through the Claude hook to the Grok log.
3. Records which app launched the agent (`agent_origin*` fields), based on `SIDEPULSE_AGENT_ORIGIN`,
   the VS Code and app-bundle environment, the parent process chain, then `TERM_PROGRAM`.
4. Sends `{"provider", "line"}` to the app's Unix socket, giving up after 200 ms.
5. Appends the line to the provider log, locked with `flock` and rotated at 32 MB with 3 backups.
6. Appends a status-audit record to `event-status.jsonl`, rotated at 16 MB with 3 backups. It uses
   the Python collector's status rules for modes, question detection and display names.

It always exits 0 and prints nothing, so a hook failure never blocks or clutters the agent.

## Compatibility

`scripts/parity.py` replays payloads through both implementations, each with its own temporary
`XDG_STATE_HOME`, and diffs every log and audit record. On the corpus above, all 353 records match
once timestamps stamped at run time are masked. The only known difference is the wording of
`parse_error` messages when the input isn't valid JSON.

```sh
python3 scripts/parity.py \
  --rust target/release/sidepulse-hook \
  --python /path/to/python3 --python-hook /path/to/site-packages/sidepulse/hook_entry.py \
  [--corpus ~/.local/state/sidepulse/agent-monitor/claude.jsonl --max-real 150]
```

Without `--corpus` it runs only the built-in synthetic cases. Hook logs contain your prompts and tool
output, so keep corpora and harness output out of the repository.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs these on macOS for every push and pull request. See [AGENTS.md](AGENTS.md) for the rules that
coding agents and contributors follow in this repository.

## License

MIT. See [LICENSE](LICENSE). It includes the copyright notice of the original Python project.
