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
| 2 | Status-bar core: event socket, bounded `latest.json`, lid and battery state via IOKit | Done |
| 3 | `sidepulse-bar`: menu bar, LED output, battery display, keep-awake, lid animations | Done |
| 4 | Setup and Settings windows, Screen Bar, eject-guard installer | Not started (use the Python CLI) |

## `sidepulse-bar`

A native replacement for the Python menu-bar app (`python -m sidepulse status-bar`). It reads and
writes the same files, so you can switch back and forth: `settings.json`, `latest.json`, the hook logs
and `events.sock`.

| Same workload, live on an M3 Max | Python app | Rust app |
|---|---|---|
| Memory footprint | 509 MB (grows over days) | **29 MB** (peak 31 MB) |
| CPU at idle | 3–21% | **~0%** |
| Child processes | `ioreg` every second, plus `caffeinate` and `touch` | none (only `sudo -n pmset` for the optional closed-lid helper) |

What makes the difference:

- **Bounded state.** `latest.json` keeps statuses from the last 6 hours, at most 256. It's written as
  compact JSON at most every 2 seconds instead of on every event. The Python app's file had grown to
  10k statuses and 12.6 MB, rewritten with indentation on every hook event.
- **Native system reads.** Battery and adapter data come from IOKit (about 0.4 ms) and the lid state
  from `IOPMrootDomain` (about 16 µs), instead of spawning `ioreg` (about 50 ms each, 86,400 times a
  day for the lid alone).
- **Native power control.** Keep-awake uses IOPM assertions instead of a `caffeinate` child, and
  keepalive touches use `utimes` instead of `/usr/bin/touch`.
- **Event-driven.** The socket thread blocks in `accept`; Python woke every 0.5 s. The menu is rebuilt
  only when its contents change.
- **No blocking writes.** A single LED worker thread owns every SD-card write, so a sleeping card
  never stalls the menu bar. The newest request always wins (Python dropped a sync that arrived while
  one was in flight), and the charging pulse is rewritten at its intended wattage-based cadence
  instead of every 15 s.

Install it as the login agent in place of the Python app:

```sh
cargo build --release
install -m 755 target/release/sidepulse-bar ~/.local/bin/sidepulse-bar
launchctl bootout gui/$(id -u)/io.sidepulse.agentstatus   # stop the Python app
# then load a LaunchAgent that runs ~/.local/bin/sidepulse-bar (see AGENTS.md for the plist used)
```

For a side-by-side test that doesn't touch your real device or state:

```sh
XDG_STATE_HOME=/tmp/sp/state XDG_CONFIG_HOME=/tmp/sp/config SIDEPULSE_VOLUMES=/tmp/sp/vol \
  target/release/sidepulse-bar
```

`SIDEPULSE_VOLUMES` replaces `/Volumes` as the place to look for devices. Create
`/tmp/sp/vol/SidePulsePro/LEDS.LED` to get a fake 8-LED device.

Not ported yet: the Setup and Settings windows (hook install, transcript toggles, debug export,
animation editor), the notch Screen Bar, composited app icons in session rows, and the brightness
slider. Brightness is a 25/50/75/100% submenu, and other values set elsewhere are kept and shown.

## Requirements

- macOS. Process ancestry uses `proc_pidinfo` and `sysctl(KERN_PROCARGS2)`. On other systems the
  crate builds, but origin detection falls back to environment variables.
- Rust 1.85 or newer.
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

`sidepulse-bar` differences from the Python app, all deliberate:

- It keeps state bounded (see above), so `latest.json` holds the last 6 hours instead of everything.
- It has no `system_profiler` fallback when detecting the model's full-charge wattage.
- LED writes and keepalive touches never create missing directories, so an unmounted volume can't
  leave a stray folder under `/Volumes`.
- The charging pulse cadence and LED syncs work as described above.

It keeps the Python app's behavior quirks, such as a finished tool call disappearing after about two
minutes and `include_stale` having no effect, so the menu shows the same sessions.

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
