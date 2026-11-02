//! busylight: Kuando/Plenom Busylight status indicator for Claude Code.
//!
//! Subcommands:
//!   busylight daemon            own the device, merge session states, keep the light alive
//!   busylight state <s>         Claude Code hook: record this session's state (hook JSON on stdin)
//!                               s = idle | busy | attention | pre-tool | end
//!   busylight test              cycle red / yellow / green / off and exit
//!   busylight set <name|r,g,b>  set a colour once and exit
//!
//! States: idle = green, busy = yellow, attention = red (Claude needs you).
//! Several sessions merge: attention > busy > idle. Sessions whose Claude
//! process has exited are dropped. The colour is re-sent every few seconds
//! because the Busylight turns itself off without traffic.
//!
//! Optional config, reloaded live: ~/.config/claude-busylight/config.json

use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VENDOR_TAG: &str = "HID_ID=0003:000027BB";
static STOP: AtomicBool = AtomicBool::new(false);

type Rgb = [u8; 3];

// ---------------------------------------------------------------- config

#[derive(Deserialize, Default)]
#[serde(default)]
struct UserConfig {
    colors: HashMap<String, Rgb>,
    blink: HashMap<String, Vec<u8>>,
    no_session: Option<String>,
    refresh_seconds: Option<f64>,
    poll_seconds: Option<f64>,
    stale_seconds: Option<f64>,
}

struct Config {
    colors: HashMap<String, Rgb>,
    blink: HashMap<String, (u8, u8)>,
    no_session_idle: bool,
    refresh: Duration,
    poll: Duration,
    stale: Option<Duration>,
}

impl Config {
    fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("claude-busylight").join("config.json")
    }

    fn load() -> Config {
        let mut colors: HashMap<String, Rgb> = HashMap::from([
            ("idle".into(), [0, 100, 0]),
            ("busy".into(), [100, 45, 0]),
            ("attention".into(), [100, 0, 0]),
            ("off".into(), [0, 0, 0]),
        ]);
        let mut cfg = Config {
            colors: HashMap::new(),
            blink: HashMap::new(),
            no_session_idle: false,
            refresh: Duration::from_secs(5),
            poll: Duration::from_millis(300),
            stale: None,
        };
        let user: UserConfig = match fs::read_to_string(Self::path()) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(u) => u,
                Err(e) => {
                    log(&format!("config error, using defaults: {e}"));
                    UserConfig::default()
                }
            },
            Err(_) => UserConfig::default(),
        };
        colors.extend(user.colors);
        cfg.colors = colors;
        for (k, v) in user.blink {
            if v.len() >= 2 {
                cfg.blink.insert(k, (v[0], v[1]));
            }
        }
        cfg.no_session_idle = user.no_session.as_deref() == Some("idle");
        if let Some(s) = user.refresh_seconds {
            cfg.refresh = Duration::from_secs_f64(s.max(0.5));
        }
        if let Some(s) = user.poll_seconds {
            cfg.poll = Duration::from_secs_f64(s.max(0.05));
        }
        cfg.stale = user.stale_seconds.filter(|s| *s > 0.0).map(Duration::from_secs_f64);
        cfg
    }

    fn color(&self, state: &str) -> Rgb {
        self.colors.get(state).copied().unwrap_or([0, 0, 0])
    }

    fn blink(&self, state: &str) -> Option<(u8, u8)> {
        self.blink.get(state).copied()
    }
}

fn log(msg: &str) {
    // journald stamps each line; keep the output plain.
    eprintln!("{msg}");
}

// ---------------------------------------------------------------- device

/// The 64-byte Kuando command: one step that jumps to itself, plus a leading
/// hidraw report-id byte (the device uses no report ids, so it is 0).
fn packet(rgb: Rgb, blink: Option<(u8, u8)>) -> [u8; 65] {
    let mut buf = [0u8; 65];
    let b = &mut buf[1..];
    b[0] = 0x10; // jump to step 0 (loop forever)
    b[1] = 0; // repeat count, 0 = infinite
    b[2] = rgb[0].min(100);
    b[3] = rgb[1].min(100);
    b[4] = rgb[2].min(100);
    let (on, off) = blink.unwrap_or((0, 0)); // tenths of a second; 0/0 = steady
    b[5] = on;
    b[6] = off;
    b[7] = 0x80; // audio update: ringtone 0, volume 0 (silent)
    b[56] = 0x06; // sensitivity
    b[57] = 0x0A; // timeout
    b[58] = 0x00; // trigger
    b[59] = 0xFF;
    b[60] = 0xFF;
    b[61] = 0xFF;
    let chk: u32 = b[..62].iter().map(|&x| x as u32).sum();
    b[62] = (chk >> 8) as u8;
    b[63] = chk as u8;
    buf
}

fn find_device() -> Option<PathBuf> {
    let dir = fs::read_dir("/sys/class/hidraw").ok()?;
    for entry in dir.flatten() {
        let uevent = entry.path().join("device").join("uevent");
        if fs::read_to_string(uevent).map(|s| s.contains(VENDOR_TAG)).unwrap_or(false) {
            return Some(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    None
}

struct Light {
    file: Option<fs::File>,
    warned: bool,
}

impl Light {
    fn new() -> Light {
        Light { file: None, warned: false }
    }

    fn open(&mut self) -> bool {
        let Some(path) = find_device() else {
            if !self.warned {
                log("busylight not found (unplugged?) - waiting");
                self.warned = true;
            }
            return false;
        };
        match fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(&path) {
            Ok(f) => {
                self.file = Some(f);
                self.warned = false;
                log(&format!("opened {}", path.display()));
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                if !self.warned {
                    log(&format!(
                        "no permission for {}: install udev/60-kuando-busylight.rules and replug",
                        path.display()
                    ));
                    self.warned = true;
                }
                false
            }
            Err(e) => {
                if !self.warned {
                    log(&format!("open failed: {e}"));
                    self.warned = true;
                }
                false
            }
        }
    }

    fn send(&mut self, data: &[u8; 65]) -> bool {
        if self.file.is_none() && !self.open() {
            return false;
        }
        match self.file.as_mut().unwrap().write_all(data) {
            Ok(()) => true,
            Err(e) => {
                log(&format!("write failed, will reopen: {e}"));
                self.file = None;
                false
            }
        }
    }
}

// ---------------------------------------------------------------- sessions

fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("claude-busylight").join("sessions")
}

fn priority(state: &str) -> u8 {
    match state {
        "attention" => 3,
        "busy" => 2,
        "idle" => 1,
        _ => 0,
    }
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// The winning state across live sessions, or None when there is none.
fn aggregate(cfg: &Config) -> Option<&'static str> {
    let mut best: Option<&'static str> = None;
    let Ok(entries) = fs::read_dir(state_dir()) else { return None };
    let now = now_secs();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "tmp") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let mut parts = text.split_whitespace();
        let Some(state) = parts.next() else { continue };
        let pid = parts.next().and_then(|p| p.parse::<u32>().ok());
        let ts = parts.next().and_then(|t| t.parse::<f64>().ok()).unwrap_or(now);
        let dead = pid.is_some_and(|p| !pid_alive(p));
        let stale = cfg.stale.is_some_and(|s| now - ts > s.as_secs_f64());
        if dead || stale {
            let _ = fs::remove_file(&path);
            continue;
        }
        let state: &'static str = match state {
            "attention" => "attention",
            "busy" => "busy",
            "idle" => "idle",
            _ => continue,
        };
        if best.is_none_or(|b| priority(state) > priority(b)) {
            best = Some(state);
        }
    }
    best
}

// ---------------------------------------------------------------- daemon

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn run_daemon() -> ExitCode {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
    }
    let dir = state_dir();
    let _ = fs::create_dir_all(&dir);
    log(&format!("state dir {}", dir.display()));

    let mut cfg = Config::load();
    let mut cfg_mtime = fs::metadata(Config::path()).and_then(|m| m.modified()).ok();
    let mut light = Light::new();
    let mut shown: Option<[u8; 65]> = None;
    let mut last_sent = Instant::now() - Duration::from_secs(3600);

    while !STOP.load(Ordering::SeqCst) {
        let mtime = fs::metadata(Config::path()).and_then(|m| m.modified()).ok();
        if mtime != cfg_mtime {
            cfg = Config::load();
            cfg_mtime = mtime;
            shown = None;
            log("config reloaded");
        }

        let state = aggregate(&cfg).unwrap_or(if cfg.no_session_idle { "idle" } else { "off" });
        let data = packet(cfg.color(state), cfg.blink(state));
        let changed = shown.as_ref() != Some(&data);
        if changed || last_sent.elapsed() >= cfg.refresh {
            if light.send(&data) {
                if changed {
                    log(&format!("-> {state}"));
                }
                shown = Some(data);
                last_sent = Instant::now();
            } else {
                shown = None;
            }
        }
        std::thread::sleep(cfg.poll);
    }
    light.send(&packet(cfg.color("off"), None));
    log("stopped");
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------- hook

#[derive(Deserialize, Default)]
#[serde(default)]
struct HookInput {
    session_id: String,
    tool_name: String,
}

/// Walk up from our parent to the `claude` process so the daemon can drop
/// this session once that process is gone.
fn find_claude_pid() -> u32 {
    let mut pid = unsafe { libc::getppid() } as u32;
    let fallback = pid;
    for _ in 0..8 {
        if pid <= 1 {
            break;
        }
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let comm = comm.trim();
        if comm.starts_with("claude") {
            return pid;
        }
        if comm == "node" || comm == "bun" {
            let cmd = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let is_claude = cmd
                .split(|&b| b == 0)
                .any(|arg| arg == b"claude" || arg.ends_with(b"/claude"));
            if is_claude {
                return pid;
            }
        }
        // /proc/<pid>/stat: "pid (comm) state ppid ..."; comm may hold spaces.
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { break };
        let Some(rest) = stat.rsplit_once(") ").map(|(_, r)| r) else { break };
        let Some(ppid) = rest.split_whitespace().nth(1).and_then(|p| p.parse().ok()) else { break };
        pid = ppid;
    }
    fallback
}

fn run_state(state: &str) -> ExitCode {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let hook: HookInput = serde_json::from_str(&input).unwrap_or_default();

    let mut sid: String = hook
        .session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .collect();
    if sid.is_empty() {
        sid = format!("pid-{}", unsafe { libc::getppid() });
    }

    let state = match state {
        "pre-tool" => {
            if hook.tool_name == "AskUserQuestion" {
                "attention"
            } else {
                "busy"
            }
        }
        s => s,
    };

    let dir = state_dir();
    let file = dir.join(&sid);
    if state == "end" {
        let _ = fs::remove_file(&file);
        return ExitCode::SUCCESS;
    }
    if !matches!(state, "idle" | "busy" | "attention") {
        eprintln!("busylight state: unknown state {state:?}");
        return ExitCode::from(2);
    }
    if fs::create_dir_all(&dir).is_err() {
        return ExitCode::SUCCESS; // never fail the hook
    }
    let line = format!("{state} {} {}\n", find_claude_pid(), now_secs() as u64);
    let tmp = dir.join(format!("{sid}.tmp"));
    if fs::write(&tmp, line).is_ok() {
        let _ = fs::rename(&tmp, &file);
    }
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------- cli

fn parse_rgb(cfg: &Config, spec: &str) -> Option<Rgb> {
    if let Some(c) = cfg.colors.get(spec) {
        return Some(*c);
    }
    let v: Vec<u8> = spec.split(',').map(|s| s.trim().parse::<u8>().ok()).collect::<Option<_>>()?;
    (v.len() == 3).then(|| [v[0], v[1], v[2]])
}

fn run_test() -> ExitCode {
    let cfg = Config::load();
    let mut light = Light::new();
    for s in ["attention", "busy", "idle", "off"] {
        let ok = light.send(&packet(cfg.color(s), cfg.blink(s)));
        println!("{s:<10} {}", if ok { "sent" } else { "FAILED" });
        if !ok {
            return ExitCode::FAILURE;
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    ExitCode::SUCCESS
}

fn run_set(spec: &str) -> ExitCode {
    let cfg = Config::load();
    let Some(rgb) = parse_rgb(&cfg, spec) else {
        eprintln!("unknown colour {spec:?}: use idle|busy|attention|off or r,g,b (0..100)");
        return ExitCode::from(2);
    };
    let mut light = Light::new();
    if light.send(&packet(rgb, cfg.blink(spec))) { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: busylight daemon | state <idle|busy|attention|pre-tool|end> | test | set <name|r,g,b>"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["daemon"] => run_daemon(),
        ["state", s] => run_state(s),
        ["test"] => run_test(),
        ["set", spec] => run_set(spec),
        _ => usage(),
    }
}
