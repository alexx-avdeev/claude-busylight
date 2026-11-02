# claude-busylight

Turn a **Kuando Busylight** into a status light for [Claude Code](https://code.claude.com):

| Light  | State     | Meaning |
|--------|-----------|---------|
| 🟢 green  | idle      | Claude finished and is waiting for your next prompt |
| 🟡 yellow | busy      | Claude is working: thinking, running tools |
| 🔴 red    | attention | Claude needs you: permission prompt, a question, an MCP elicitation |
| ⚫ off    | none      | no Claude Code session is running (configurable) |

Several Claude Code sessions are merged: red beats yellow beats green.

Works with Kuando / Plenom Busylight Alpha and Omega (USB vendor `27bb`) on Linux.
One small Rust binary, no Python, no runtime dependencies.

## Why a daemon

The Busylight turns itself off a few seconds after the last USB packet, so a
fire-and-forget hook cannot keep it lit. A tiny daemon (`busylight daemon`,
~1 MB RSS) owns the device, merges the state of every session and re-sends the
colour every 5 seconds. Hooks only write a state file and exit in about a millisecond.

```
Claude Code ──hooks──▶ build/busylight state <s> ──▶ $XDG_RUNTIME_DIR/claude-busylight/sessions/<session_id>
                                                                  │
                                          build/busylight daemon ◀──┘  (systemd --user service)
                                                                  │  raw HID writes, re-sent every 5 s
                                                                  ▼
                                                           /dev/hidrawN  (Busylight)
```

## Requirements

- Linux with systemd (user services) and udev
- A Rust toolchain to build the binary: `rustup` or `mise use -g rust@stable`
- `sudo` once, to install the udev rule that gives your user access to the device

## Install

### Option A: clone and run the installer (recommended)

```bash
git clone https://github.com/alexx-avdeev/claude-busylight.git ~/claude-busylight
~/claude-busylight/install.sh
```

The installer is idempotent. It:

1. installs `udev/60-kuando-busylight.rules` (sudo) so your user can open the device,
2. builds the Rust binary into `build/busylight`,
3. installs and starts the `claude-busylight` systemd user service,
4. registers the folder as a local plugin marketplace and installs the `busylight` plugin
   at user scope.

Then restart your Claude Code sessions or run `/reload-plugins`.

### Option B: install the plugin from GitHub, then set up the system parts

At the Claude Code prompt:

```
/plugin marketplace add alexx-avdeev/claude-busylight
/plugin install busylight@busylight-local
```

Then build the binary and install the udev rule and daemon from the installed copy:

```bash
"$(claude plugin list 2>/dev/null | awk '/busylight@/{f=1} f && /Read from:/{print $3; exit}')/install.sh"
```

The hooks are silent no-ops until the binary is built, so Claude Code keeps working either way.

## Check it

```bash
~/claude-busylight/build/busylight test            # red, yellow, green, off
~/claude-busylight/build/busylight set 0,0,100     # any r,g,b in 0..100
systemctl --user status claude-busylight
journalctl --user -u claude-busylight -f         # logs each state change
```

Open a Claude Code session: the light goes green. Send a prompt: yellow. Make Claude
ask you something or hit a permission prompt: red.

## Configure

Optional `~/.config/claude-busylight/config.json`, reloaded live:

```json
{
  "colors": { "idle": [0, 100, 0], "busy": [100, 45, 0], "attention": [100, 0, 0] },
  "blink": { "attention": [5, 5] },
  "no_session": "idle",
  "refresh_seconds": 5
}
```

| Key | Meaning |
|-----|---------|
| `colors` | per state, `[r, g, b]` with each channel 0..100 (the device's own scale) |
| `blink` | per state, `[on, off]` in tenths of a second; omit for steady |
| `no_session` | `"off"` (default) or `"idle"` when no Claude session is alive |
| `refresh_seconds` | how often the colour is re-sent (device keep-alive), default 5 |
| `poll_seconds` | how often state files are re-read, default 0.3 |
| `stale_seconds` | drop a session file older than this; default 0 = never, rely on the pid check |

## How the hooks map to states

| Hook | State |
|------|-------|
| `SessionStart`, `Stop`, `StopFailure`, `Notification(idle_prompt)` | idle |
| `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionDenied`, `PreCompact`, `ElicitationResult` | busy |
| `PreToolUse(AskUserQuestion)`, `Notification(permission_prompt\|elicitation_dialog)`, `Elicitation` | attention |
| `SessionEnd` | session removed |

Each state file holds `<state> <claude pid> <timestamp>`. The daemon drops a session as
soon as its Claude process is gone, so a crashed or killed session never leaves the light stuck.

## Layout

```
.claude-plugin/plugin.json       plugin manifest (name: busylight)
.claude-plugin/marketplace.json  makes this repo installable as a marketplace
hooks/hooks.json                 Claude Code hooks -> build/busylight state <s>
daemon/                          Rust crate: daemon, hook, test and set subcommands
udev/60-kuando-busylight.rules   device access for the logged-in user (uaccess)
systemd/claude-busylight.service user service template
install.sh                       one-shot, idempotent installer
```

## Troubleshooting

- **Light never turns on, journal says `no permission for /dev/hidrawN`**: the udev rule is
  missing or numbered above 70 (the `uaccess` tag must be set before `70-uaccess.rules`
  runs). Re-run `install.sh`, then unplug and replug the light. `getfacl /dev/hidrawN`
  should list your user.
- **Journal says `busylight not found`**: the device is not enumerated. Check
  `grep -l 27BB /sys/class/hidraw/*/device/uevent`.
- **Light stays yellow after pressing Esc**: interrupting a turn fires no `Stop` hook. It
  recovers on your next prompt or the 60 s `idle_prompt` notification.
- **Nothing happens in a session started before installing**: run `/reload-plugins` or
  restart the session.

## Uninstall

```bash
systemctl --user disable --now claude-busylight
rm ~/.config/systemd/user/claude-busylight.service
sudo rm /etc/udev/rules.d/60-kuando-busylight.rules
claude plugin uninstall busylight@busylight-local
claude plugin marketplace remove busylight-local
```

## License

[0BSD](LICENSE): do whatever you like, no attribution required.
