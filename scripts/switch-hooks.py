"""Point SidePulse agent hooks at the native sidepulse-hook binary (run on the target Mac).

Usage: python3 scripts/switch-hooks.py   (with the Python that has SidePulse installed)

Rewrites only commands that run SidePulse's Python hook_entry.py, keeping their --provider/--log
arguments, in ~/.claude/settings.json, ~/.codex/config.toml and ~/.grok/hooks/sidepulse.json.
Backs each file up as <file>.bak-sidepulse-rust first. For Codex, re-trusts the changed hooks the
same way `sidepulse setup` does: ask `codex app-server` (hooks/list) for each hook's currentHash and
store it as trusted_hash, using SidePulse's own update_codex_trusted_hashes TOML writer.
"""
import json
import os
import re
import select
import shutil
import subprocess
import sys
import time
from pathlib import Path

HOME = Path.home()
NATIVE = str(HOME / ".local/bin/sidepulse-hook")
PY_HOOK = re.compile(r"\S*python[0-9.]*\s+\S*sidepulse/hook_entry\.py(?=\s)")
CODEX_COMMAND = re.compile(r"(command\s*=\s*''')([^']*?)(''')")


def rewrite(command: str) -> str:
    return PY_HOOK.sub(NATIVE, command, count=1)


def backup(path: Path) -> None:
    dest = path.with_name(path.name + ".bak-sidepulse-rust")
    if not dest.exists():
        shutil.copy2(path, dest)


def switch_json(path: Path) -> int:
    if not path.exists():
        return 0
    data = json.loads(path.read_text())
    changed = 0
    for entries in (data.get("hooks") or {}).values():
        for entry in entries if isinstance(entries, list) else []:
            for hook in entry.get("hooks", []) if isinstance(entry, dict) else []:
                cmd = hook.get("command")
                if isinstance(cmd, str) and PY_HOOK.search(cmd):
                    hook["command"] = rewrite(cmd)
                    changed += 1
    if changed:
        backup(path)
        tmp = path.with_suffix(path.suffix + ".tmp")
        tmp.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
        os.replace(tmp, path)
    return changed


def switch_codex(path: Path) -> tuple[int, int]:
    if not path.exists():
        return 0, 0
    text = path.read_text()
    changed = sum(1 for m in CODEX_COMMAND.finditer(text) if PY_HOOK.search(m.group(2)))
    if not changed:
        return 0, 0
    new = CODEX_COMMAND.sub(lambda m: m.group(1) + rewrite(m.group(2)) + m.group(3), text)
    backup(path)
    path.write_text(new)
    return changed, retrust_codex(path)


# Non-interactive shells (ssh, launchd) often lack the PATH entries where the Codex CLI and the
# `node` its npm/bun shim needs are installed.
EXTRA_PATH = [str(HOME / ".bun/bin"), "/opt/homebrew/bin", "/usr/local/bin", str(HOME / ".local/bin")]


def codex_env() -> dict:
    env = dict(os.environ)
    env["PATH"] = os.pathsep.join(EXTRA_PATH + [env.get("PATH", "/usr/bin:/bin")])
    return env


def codex_cli():
    for c in ("/Applications/ChatGPT.app/Contents/Resources/codex", "/Applications/Codex.app/Contents/Resources/codex"):
        if Path(c).exists():
            return c
    return shutil.which("codex", path=codex_env()["PATH"])


def codex_hook_hashes(config: Path, timeout: float = 15.0) -> dict:
    cli = codex_cli()
    if not cli:
        return {}
    proc = subprocess.Popen([cli, "app-server", "--stdio"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, text=True, bufsize=1, cwd=str(HOME),
                            env=codex_env())

    def call(msg_id, method, params):
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "method": method, "params": params}) + "\n")
        proc.stdin.flush()
        deadline = time.time() + timeout
        while time.time() < deadline:
            ready, _, _ = select.select([proc.stdout], [], [], 0.5)
            if not ready:
                continue
            try:
                msg = json.loads(proc.stdout.readline())
            except json.JSONDecodeError:
                continue
            if msg.get("id") == msg_id:
                return msg
        return None

    try:
        call(1, "initialize", {"clientInfo": {"name": "sidepulse", "version": "0"}, "capabilities": None})
        resp = call(2, "hooks/list", {"cwds": [str(HOME)]})
    finally:
        proc.terminate()
    try:
        hooks = resp["result"]["data"][0]["hooks"]
    except (TypeError, KeyError, IndexError):
        return {}
    return {
        h["key"]: h["currentHash"]
        for h in hooks
        if isinstance(h, dict)
        and h.get("sourcePath") == str(config)
        and "sidepulse-hook" in str(h.get("command"))
        and h.get("key")
        and h.get("currentHash")
    }


def retrust_codex(path: Path) -> int:
    # SidePulse's own TOML writer: from the installed package, or a source checkout given by
    # SIDEPULSE_SRC (default ~/sidepulse/sidepulse/src).
    sys.path.insert(0, os.environ.get("SIDEPULSE_SRC", str(HOME / "sidepulse/sidepulse/src")))
    from sidepulse.install import update_codex_trusted_hashes

    hashes = codex_hook_hashes(path)
    if hashes:
        path.write_text(update_codex_trusted_hashes(path.read_text(), hashes))
    return len(hashes)


if __name__ == "__main__":
    if not Path(NATIVE).exists():
        raise SystemExit(f"{NATIVE} is missing; install the binary first")
    claude = switch_json(HOME / ".claude/settings.json")
    grok = switch_json(HOME / ".grok/hooks/sidepulse.json")
    codex, trusted = switch_codex(HOME / ".codex/config.toml")
    print(f"claude hooks switched: {claude}")
    print(f"grok hooks switched: {grok}")
    print(f"codex hooks switched: {codex}, re-trusted: {trusted}")
