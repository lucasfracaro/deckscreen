//! Paths, logging, process helpers, time.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ------------------------------------------------------------------ paths

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn xdg(var: &str, fallback: &[&str]) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => fallback.iter().fold(home(), |p, s| p.join(s)),
    }
}

pub fn config_home() -> PathBuf {
    xdg("XDG_CONFIG_HOME", &[".config"])
}
pub fn data_home() -> PathBuf {
    xdg("XDG_DATA_HOME", &[".local", "share"])
}
pub fn config_dir() -> PathBuf {
    config_home().join("deckscreen")
}
pub fn data_dir() -> PathBuf {
    data_home().join("deckscreen")
}
pub fn sunshine_dir() -> PathBuf {
    config_home().join("sunshine")
}
pub fn user_unit_dir() -> PathBuf {
    config_home().join("systemd").join("user")
}
pub fn bin_dir() -> PathBuf {
    home().join(".local").join("bin")
}
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

pub fn uid() -> u32 {
    // /proc/self is owned by the effective user.
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(1000)
}

/// Make sure the variables needed to reach the Wayland session / D-Bus exist,
/// even when we're started by systemd or Sunshine with a thin environment.
/// Must be called at startup while the process is still single-threaded.
pub fn ensure_session_env() {
    let uid = uid();
    let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
    set_default("XDG_RUNTIME_DIR", &rt);
    set_default("DBUS_SESSION_BUS_ADDRESS", &format!("unix:path={rt}/bus"));
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && Path::new(&rt).join("wayland-0").exists() {
        set_default("WAYLAND_DISPLAY", "wayland-0");
    }
    // Hyprland: find the running instance if the variable wasn't imported.
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        if let Ok(rd) = fs::read_dir(Path::new(&rt).join("hypr")) {
            let newest = rd
                .flatten()
                .filter(|e| e.path().is_dir())
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH));
            if let Some(e) = newest {
                set_default("HYPRLAND_INSTANCE_SIGNATURE", &e.file_name().to_string_lossy());
            }
        }
    }
}

fn set_default(k: &str, v: &str) {
    if std::env::var_os(k).map(|x| x.is_empty()).unwrap_or(true) {
        std::env::set_var(k, v);
    }
}

// ------------------------------------------------------------------ files

pub fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(content.as_bytes())?;
    }
    fs::rename(&tmp, path)
}

pub fn write_file(path: &Path, content: &str, mode: u32) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

pub fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

// ------------------------------------------------------------------ logging

static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);

pub fn init_log_file() {
    let dir = data_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("deckscreen.log");
    // keep the log from growing forever
    if fs::metadata(&path).map(|m| m.len() > 4 * 1024 * 1024).unwrap_or(false) {
        let _ = fs::rename(&path, dir.join("deckscreen.log.1"));
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(path) {
        *LOG_FILE.lock().unwrap() = Some(f);
    }
}

pub fn log(level: &str, msg: &str) {
    let line = format!("{} {} {}\n", timestamp(), level, msg);
    eprint!("{line}");
    if let Ok(mut g) = LOG_FILE.lock() {
        if let Some(f) = g.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::util::log("INFO", &format!($($t)*)) } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::util::log("WARN", &format!($($t)*)) } }
#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::util::log("ERROR", &format!($($t)*)) } }

// ------------------------------------------------------------------ time

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "YYYY-MM-DD HH:MM:SSZ" (UTC) without any date crate.
pub fn timestamp() -> String {
    format_utc(now_secs())
}

pub fn format_utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

// ------------------------------------------------------------------ processes

pub fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Run a command and capture its output. Never panics.
pub fn run(cmd: &str, args: &[&str]) -> Option<Output> {
    Command::new(cmd).args(args).stdin(Stdio::null()).output().ok()
}

/// Run and return (success, stdout+stderr).
pub fn run_text(cmd: &str, args: &[&str]) -> (bool, String) {
    match run(cmd, args) {
        Some(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        None => (false, format!("não foi possível executar {cmd}")),
    }
}

pub fn run_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    let o = run(cmd, args)?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Run interactively (inherits the terminal). Returns success.
pub fn run_interactive(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "PC".into())
}

// ------------------------------------------------------------------ terminal prompts

pub fn is_tty() -> bool {
    Command::new("test")
        .args(["-t", "0"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn ask_yes(question: &str, assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }
    if !is_tty() {
        println!("  {question} [S/n] (sem terminal: não)");
        return false;
    }
    print!("  {question} [S/n] ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    if std::io::stdin().read_line(&mut s).is_err() {
        return false;
    }
    let s = s.trim().to_lowercase();
    s.is_empty() || s.starts_with('s') || s.starts_with('y')
}

/// Question whose default (Enter / no terminal) is NO — for destructive actions.
pub fn ask_no(question: &str) -> bool {
    if !is_tty() {
        return false;
    }
    print!("  {question} [s/N] ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    if std::io::stdin().read_line(&mut s).is_err() {
        return false;
    }
    let s = s.trim().to_lowercase();
    s.starts_with('s') || s.starts_with('y')
}

pub fn ask_line(question: &str) -> String {
    print!("  {question}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_string()
}

pub fn ask_password(question: &str) -> String {
    print!("  {question}");
    let _ = std::io::stdout().flush();
    let _ = Command::new("stty").arg("-echo").stdin(Stdio::inherit()).status();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    let _ = Command::new("stty").arg("echo").stdin(Stdio::inherit()).status();
    println!();
    s.trim_end_matches(['\n', '\r']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utc_format() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00Z");
        assert_eq!(format_utc(1_793_577_600), "2026-11-02 00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00:00Z");
    }
}

/// Tests write logs/state under a throwaway directory instead of $HOME.
#[cfg(test)]
pub fn test_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let d = std::env::temp_dir().join(format!("deckscreen-test-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", d.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", d.join("config"));
    });
}
