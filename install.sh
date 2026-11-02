#!/usr/bin/env bash
# One-shot installer for claude-busylight. Safe to re-run.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "== 1/4 udev rule (needs sudo): lets your user write to the Busylight"
if ! cmp -s "$ROOT/udev/60-kuando-busylight.rules" /etc/udev/rules.d/60-kuando-busylight.rules 2>/dev/null; then
  # 60- so it sorts before 70-uaccess.rules; an earlier version was wrongly numbered 99.
  sudo rm -f /etc/udev/rules.d/99-kuando-busylight.rules
  sudo install -m 644 "$ROOT/udev/60-kuando-busylight.rules" /etc/udev/rules.d/60-kuando-busylight.rules
  sudo udevadm control --reload-rules
  sudo udevadm trigger --subsystem-match=hidraw --action=add
else
  echo "   already installed"
fi

echo "== 2/4 build the Rust binary (needs cargo; install with: mise use -g rust@stable)"
if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.local/share/mise/shims/cargo" ]; then
  export PATH="$HOME/.local/share/mise/shims:$PATH"
fi
command -v cargo >/dev/null 2>&1 || { echo "cargo not found. Install Rust (e.g. 'mise use -g rust@stable' or rustup) and re-run."; exit 1; }
(cd "$ROOT/daemon" && cargo build --release --quiet)
mkdir -p "$ROOT/build"
install -m 755 "$ROOT/daemon/target/release/busylight" "$ROOT/build/busylight"

echo "== 3/4 systemd user service: the daemon that drives the light"
mkdir -p "$HOME/.config/systemd/user"
sed "s|__PLUGIN_ROOT__|$ROOT|" "$ROOT/systemd/claude-busylight.service" > "$HOME/.config/systemd/user/claude-busylight.service"
systemctl --user daemon-reload
systemctl --user enable --now claude-busylight.service
systemctl --user restart claude-busylight.service

echo "== 4/4 Claude Code plugin: the hooks that report state"
case "$ROOT" in
  "$HOME/.claude/plugins/"*)
    echo "   running from an installed plugin copy; hooks are already registered" ;;
  *)
    if ! claude plugin marketplace list 2>/dev/null | grep -q "busylight-local"; then
      claude plugin marketplace add "$ROOT"
    fi
    if ! claude plugin list 2>/dev/null | grep -q "busylight@busylight-local"; then
      claude plugin install busylight@busylight-local --scope user
    fi ;;
esac

echo
echo "Done. Quick checks:"
echo "  $ROOT/build/busylight test                # cycles red / yellow / green / off"
echo "  systemctl --user status claude-busylight"
echo "  journalctl --user -u claude-busylight -f"
echo "Restart any running Claude Code sessions (or run /reload-plugins) to pick up the hooks."
