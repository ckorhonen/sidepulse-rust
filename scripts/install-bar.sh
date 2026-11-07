#!/bin/bash
# Install sidepulse-bar as the login agent, replacing the Python SidePulse menu-bar app.
#   scripts/install-bar.sh            install (builds release first) and switch over
#   scripts/install-bar.sh --revert   stop sidepulse-bar and restore the Python app's agent
set -euo pipefail

LABEL=io.sidepulse.bar
PY_LABEL=io.sidepulse.agentstatus
AGENTS="$HOME/Library/LaunchAgents"
PLIST="$AGENTS/$LABEL.plist"
PY_PLIST="$AGENTS/$PY_LABEL.plist"
BACKUP="$HOME/Library/LaunchAgents.disabled-sidepulse-python"
BIN="$HOME/.local/bin/sidepulse-bar"
STATE="${XDG_STATE_HOME:-$HOME/.local/state}/sidepulse/agent-monitor"
DOMAIN="gui/$(id -u)"

if [[ "${1:-}" == "--revert" ]]; then
  launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
  rm -f "$PLIST"
  if [[ -f "$BACKUP/$PY_LABEL.plist" ]]; then
    mv "$BACKUP/$PY_LABEL.plist" "$PY_PLIST"
    launchctl bootstrap "$DOMAIN" "$PY_PLIST"
    echo "Restored the Python SidePulse app ($PY_LABEL)."
  else
    echo "No backed-up Python agent found; run 'sidepulse setup' to reinstall it."
  fi
  exit 0
fi

cd "$(dirname "$0")/.."
cargo build --release -p sidepulse-bar
mkdir -p "$(dirname "$BIN")" "$STATE" "$AGENTS"
install -m 755 target/release/sidepulse-bar "$BIN"

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key><array><string>$BIN</string></array>
  <key>RunAtLoad</key><true/>
  <!-- Restart after a crash, but stay stopped after Quit (exit 0). -->
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>$STATE/sidepulse-bar.out.log</string>
  <key>StandardErrorPath</key><string>$STATE/sidepulse-bar.err.log</string>
</dict>
</plist>
EOF
plutil -lint "$PLIST" >/dev/null

# Stop the Python app first: both apps own the same events.sock and LED device.
if launchctl print "$DOMAIN/$PY_LABEL" >/dev/null 2>&1; then
  launchctl bootout "$DOMAIN/$PY_LABEL" || true
fi
if [[ -f "$PY_PLIST" ]]; then
  mkdir -p "$BACKUP"
  mv "$PY_PLIST" "$BACKUP/"
fi

launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
launchctl bootstrap "$DOMAIN" "$PLIST"
echo "sidepulse-bar installed as $LABEL (logs: $STATE/sidepulse-bar.out.log)."
echo "Revert with: scripts/install-bar.sh --revert"
