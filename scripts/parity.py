"""Replay hook payloads through the Python and Rust SidePulse hooks and diff the outputs.

Each implementation gets its own temporary XDG_STATE_HOME, so a running SidePulse app and its socket
are never touched. Run-time timestamps (`audited_at`, stamped `logged_at`) are masked before diffing.

    python3 scripts/parity.py --rust target/release/sidepulse-hook \
        --python /path/to/python3 --python-hook /path/to/sidepulse/hook_entry.py \
        [--corpus ~/.local/state/sidepulse/agent-monitor/claude.jsonl --max-real 150]

Hook logs contain prompts and tool output: never commit corpora or this script's output.
"""
import argparse
import json
import re
import os
import resource
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ORIGIN_KEYS = ("agent_origin", "agent_origin_kind", "agent_origin_source", "agent_origin_confidence")


def real_payloads(source: Path, limit: int):
    lines = source.read_text(encoding="utf-8", errors="replace").splitlines()[-limit * 3:]
    out = []
    for i, line in enumerate(lines):
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(obj, dict):
            continue
        for k in ORIGIN_KEYS:
            obj.pop(k, None)
        if i % 2:  # half without logged_at so both stamp their own
            obj.pop("logged_at", None)
        out.append(("claude", json.dumps(obj, ensure_ascii=False)))
        if len(out) >= limit:
            break
    return out


def synthetic_payloads():
    cases = [
        ("claude", ""),
        ("claude", "{bad json"),
        ("claude", "[1, 2]"),
        ("claude", '"just a string"'),
        ("claude", json.dumps({"hook_event_name": "Stop", "session_id": "abcdef123456", "cwd": str(Path.home() / "Repos/sidepulse"),
                               "last_assistant_message": "Done.\n\nWant me to push it?"})),
        ("claude", json.dumps({"hook_event_name": "Stop", "session_id": "s2", "last_assistant_message": "All set. Anything else?"})),
        ("claude", json.dumps({"hook_event_name": "Notification", "session_id": "s3", "notification_type": "idle_prompt", "message": "Done"})),
        ("claude", json.dumps({"hook_event_name": "Notification", "session_id": "s3", "message": "Claude needs your permission to use Bash"})),
        ("claude", json.dumps({"hook_event_name": "Notification", "session_id": "s3", "notification_type": None})),
        ("claude", json.dumps({"hook_event_name": "PostToolUse", "session_id": "s4", "tool_name": "Bash", "tool_response": {"exit_code": 2}})),
        ("claude", json.dumps({"hook_event_name": "PostToolUse", "session_id": "s4", "tool_response": "Traceback (most recent call last)"})),
        ("claude", json.dumps({"hook_event_name": "PostToolUse", "session_id": "s4", "tool_response": {"exit_code": 0.0, "success": True}})),
        ("claude", json.dumps({"hook_event_name": "UserPromptSubmit", "session_id": "s5", "cwd": "/tmp",
                               "prompt": "## My request for Codex: please fix `foo()` in '/Users/me/a b.py' and /tmp/x/y.log <tag>now</tag> " + "word " * 40})),
        ("claude", json.dumps({"hook_event_name": "UserPromptSubmit", "session_id": "s5", "prompt": "<task-notification>ignore"})),
        ("claude", json.dumps({"hook_event_name": "Stop", "agent_id": "agent-123456789", "session_id": "s6",
                               "last_assistant_message": "x\n<!-- sidepulse status: blocked -->\n"})),
        ("claude", json.dumps({"hook_event_name": "Stop", "session_id": "s7", "sidepulse_status": "Long Task Progress"})),
        ("claude", json.dumps({"hook_event_name": "pre-tool-use", "session_id": "s8", "tool_name": "Read", "logged_at": "2026-11-07T10:00:00.5-05:00"})),
        ("claude", json.dumps({"hook_event_name": "SessionStart", "session_id": "s9", "agent_origin": "Pinned Origin"})),
        ("claude", json.dumps({"hook_event_name": "Stop", "session_id": "s10", "sidepulse_origin": {"label": " Structured  Origin "}})),
        ("claude", json.dumps({"hook_event_name": "Stop", "session_id": "é漢字", "message": "  ünïcode\tmessage    here", "cwd": "relative/dir"})),
        ("claude", json.dumps({"hookEventName": "SubagentStart", "sessionId": "g1", "workspaceRoot": "/tmp"})),  # grok via claude
        ("claude", json.dumps({"hook_event_name": "Stop", "transcript_path": "/Users/x/.grok/sessions/a.jsonl", "session_id": "g2"})),
        ("claude", json.dumps({"hook_event_name": "Bogus", "session_id": "s11"})),
        ("claude", json.dumps({"session_id": "no-event"})),
        ("claude", json.dumps({"hook_event_name": "Stop", "session_id": 12345, "duration": 1.50, "big": 10**3, "nested": {"b": [1, {"z": None, "a": True}]}})),
        ("codex", json.dumps({"hook_event_name": "UserPromptSubmit", "session_id": "c1", "prompt": "Generate 0 to 3 hyperpersonalized suggestions"})),
        ("codex", json.dumps({"hook_event_name": "PreToolUse", "session_id": "c2", "turn_id": "t", "tool_name": "shell", "cwd": str(Path.home())})),
        ("codex", json.dumps({"hook_event_name": "Stop", "session_id": "c3", "last_assistant_message": "Which branch should I use?"})),
        ("codex", "not json"),
        ("grok", json.dumps({"hookEventName": "StopFailure", "sessionId": "g3"})),
    ]
    # A real codex session id so the session_index title lookup is exercised.
    index = Path.home() / ".codex/session_index.jsonl"
    try:
        last = json.loads(index.read_text().splitlines()[-1])
        cases.append(("codex", json.dumps({"hook_event_name": "Stop", "session_id": last.get("id"), "cwd": "/tmp"})))
    except Exception:
        pass
    return cases


def run(cmd, payload, env):
    start = time.perf_counter()
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    proc.stdin.write(payload.encode("utf-8"))
    proc.stdin.close()
    _, status, usage = os.wait4(proc.pid, 0)
    wall = time.perf_counter() - start
    out, err = proc.stdout.read(), proc.stderr.read()
    return {
        "exit": os.waitstatus_to_exitcode(status),
        "wall": wall,
        "cpu": usage.ru_utime + usage.ru_stime,
        "maxrss_mb": usage.ru_maxrss / (1024 * 1024),
        "stdout": out.decode(errors="replace"),
        "stderr": err.decode(errors="replace"),
    }


def read_jsonl(path: Path):
    if not path.exists():
        return []
    return [json.loads(l) for l in path.read_text(encoding="utf-8").split("\n") if l.strip()]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--rust", required=True, help="path to the sidepulse-hook binary")
    ap.add_argument("--python", default=sys.executable, help="python interpreter with SidePulse installed")
    ap.add_argument("--python-hook", required=True, help="path to sidepulse/hook_entry.py")
    ap.add_argument("--corpus", type=Path, help="optional real hook log (JSONL) to replay")
    ap.add_argument("--max-real", type=int, default=150)
    args = ap.parse_args()
    rust_bin = args.rust
    cases = synthetic_payloads() + (real_payloads(args.corpus.expanduser(), args.max_real) if args.corpus else [])
    roots = {impl: Path(tempfile.mkdtemp(prefix=f"hookparity-{impl}-")) for impl in ("py", "rs")}
    stats = {"py": [], "rs": []}
    problems = []
    for idx, (provider, payload) in enumerate(cases):
        for impl in ("py", "rs"):
            env = dict(os.environ, XDG_STATE_HOME=str(roots[impl]))
            log = roots[impl] / "sidepulse/agent-monitor" / f"{provider}.jsonl"
            cmd = [args.python, args.python_hook] if impl == "py" else [rust_bin]
            cmd += ["--provider", provider, "--log", str(log)]
            r = run(cmd, payload, env)
            stats[impl].append(r)
            if r["exit"] != 0 or r["stdout"] or r["stderr"]:
                problems.append(f"case {idx} {impl}: exit={r['exit']} stdout={r['stdout'][:80]!r} stderr={r['stderr'][:200]!r}")

    def collect(impl):
        base = roots[impl] / "sidepulse/agent-monitor"
        files = sorted(p.relative_to(base) for p in base.glob("*.jsonl"))
        return {str(f): read_jsonl(base / f) for f in files}

    py_out, rs_out = collect("py"), collect("rs")
    mismatches = []
    if sorted(py_out) != sorted(rs_out):
        mismatches.append(f"file sets differ: py={sorted(py_out)} rs={sorted(rs_out)}")
    # The two runs stamp "now" a moment apart, and that stamp also appears inside raw_preview.
    stamp = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|\+00:00)")
    # Known, documented difference: serde_json and Python word JSON parse errors differently.
    parse_error = re.compile(r'"parse_error": "(?:[^"\\]|\\.)*"')
    known = 0

    def canon(obj, mask_errors):
        obj = dict(obj)
        obj.pop("audited_at", None)
        text = stamp.sub("<now>", json.dumps(obj, sort_keys=True, ensure_ascii=False))
        return parse_error.sub('"parse_error": "<msg>"', text) if mask_errors else text

    for name in sorted(set(py_out) & set(rs_out)):
        a, b = py_out[name], rs_out[name]
        if len(a) != len(b):
            mismatches.append(f"{name}: line counts py={len(a)} rs={len(b)}")
        for i, (x, y) in enumerate(zip(a, b)):
            if canon(x, False) == canon(y, False):
                continue
            if canon(x, True) == canon(y, True):
                known += 1
                continue
            sx, sy = canon(x, True), canon(y, True)
            at = next((j for j, (p, q) in enumerate(zip(sx, sy)) if p != q), min(len(sx), len(sy)))
            mismatches.append(f"{name}[{i}] py: ...{sx[max(0, at - 80):at + 120]}\n      rs: ...{sy[max(0, at - 80):at + 120]}")
    print(f"cases: {len(cases)}  files: {sorted(py_out)}")
    print(f"lines compared: {sum(len(v) for v in py_out.values())}")
    print(f"process problems: {len(problems)}")
    for p in problems[:10]:
        print("  ", p)
    print(f"known parse_error wording differences: {known}")
    print(f"mismatches: {len(mismatches)}")
    for m in mismatches[:25]:
        print("  ", m[:600])
    for impl in ("py", "rs"):
        s = stats[impl]
        walls = sorted(r["wall"] for r in s)
        cpus = sorted(r["cpu"] for r in s)
        rss = sorted(r["maxrss_mb"] for r in s)
        med = lambda v: v[len(v) // 2]
        p95 = lambda v: v[int(len(v) * 0.95) - 1]
        print(f"{impl}: n={len(s)} wall median {med(walls)*1000:.1f} ms p95 {p95(walls)*1000:.1f} ms | "
              f"cpu median {med(cpus)*1000:.1f} ms total {sum(cpus):.2f} s | maxrss median {med(rss):.1f} MB")
    print("output dirs (delete when done):", roots["py"], roots["rs"])
    return 1 if mismatches or problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
