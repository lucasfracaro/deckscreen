//! Virtual input on the PC: Sunshine turns the Deck's buttons, sticks and trackpads into
//! a virtual gamepad/mouse/keyboard through /dev/uinput and /dev/uhid (libvirtualhid's
//! uinput backend). If those nodes are missing or not writable by the user running
//! Sunshine, the picture streams fine but every input is silently dropped.
//!
//! `deckscreen-host fix-input` repairs the usual causes: kernel modules not loaded,
//! Sunshine's udev rule missing (AppImage installs, a failed package scriptlet) and the
//! user not being in the `input` group.

use crate::util;
use std::fs::OpenOptions;
use std::path::Path;

pub const NODES: [&str; 2] = ["/dev/uinput", "/dev/uhid"];

const RULES_NAME: &str = "60-sunshine.rules";
const MODULES_NAME: &str = "60-sunshine.conf";

/// Same access rules Sunshine ships (src_assets/linux/misc/60-sunshine.rules).
const RULES: &str = r#"# Installed by DeckScreen (copy of Sunshine's rule): lets Sunshine create virtual input devices
KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput", GROUP="input", MODE="0660", TAG+="uaccess"
KERNEL=="uhid", GROUP="input", MODE="0660", TAG+="uaccess"
SUBSYSTEM=="hidraw", KERNEL=="hidraw*", IMPORT{parent}="HID_*"
SUBSYSTEM=="hidraw", KERNEL=="hidraw*", ENV{HID_PHYS}=="libvirtualhid/uhid/*", GROUP="input", MODE="0660", TAG+="uaccess"
SUBSYSTEM=="input", KERNEL=="event*", ATTRS{phys}=="libvirtualhid/uhid/*", GROUP="input", MODE="0660", TAG+="uaccess"
SUBSYSTEM=="hidraw", KERNEL=="hidraw*", ENV{HID_NAME}=="Sunshine (libvirtualhid)*", GROUP="input", MODE="0660", TAG+="uaccess"
SUBSYSTEMS=="input", ATTRS{name}=="Sunshine (libvirtualhid)*", GROUP="input", MODE="0660", TAG+="uaccess"
"#;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Node {
    Ok,
    /// Exists but the current user cannot open it for writing.
    NoAccess,
    /// Module not loaded (or not available).
    Missing,
}

pub fn node_state(path: &str) -> Node {
    if !Path::new(path).exists() {
        return Node::Missing;
    }
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(_) => Node::Ok,
        Err(_) => Node::NoAccess,
    }
}

/// The running kernel's modules were removed by an update (reboot pending).
pub fn reboot_pending() -> Option<String> {
    let kernel = util::run_stdout("uname", &["-r"])?.trim().to_string();
    (!kernel.is_empty() && !Path::new(&format!("/lib/modules/{kernel}")).exists()).then_some(kernel)
}

fn exists_any(dirs: &[&str], name: &str) -> bool {
    dirs.iter().any(|d| Path::new(d).join(name).exists())
}

pub fn rules_installed() -> bool {
    exists_any(
        &["/etc/udev/rules.d", "/usr/lib/udev/rules.d", "/lib/udev/rules.d"],
        RULES_NAME,
    )
}

fn modules_configured() -> bool {
    exists_any(
        &["/etc/modules-load.d", "/usr/lib/modules-load.d", "/lib/modules-load.d"],
        MODULES_NAME,
    )
}

pub fn in_input_group() -> bool {
    util::run_stdout("id", &["-nG"])
        .map(|s| s.split_whitespace().any(|g| g == "input"))
        .unwrap_or(false)
}

fn user_listed_in_input_group() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    std::fs::read_to_string("/etc/group")
        .ok()
        .and_then(|g| {
            g.lines()
                .find(|l| l.starts_with("input:"))
                .map(|l| l.rsplit(':').next().unwrap_or("").split(',').any(|u| u == user))
        })
        .unwrap_or(false)
}

/// One line for doctor/installer. `None` = everything fine.
pub fn problem() -> Option<(&'static str, String)> {
    let states: Vec<(&str, Node)> = NODES.iter().map(|n| (*n, node_state(n))).collect();
    if states.iter().all(|(_, s)| *s == Node::Ok) {
        return None;
    }
    if let Some(k) = reboot_pending() {
        return Some((
            "fail",
            format!("o kernel foi atualizado e o PC não reiniciou ({k}): reinicie o PC"),
        ));
    }
    let bad: Vec<String> = states
        .iter()
        .filter(|(_, s)| *s != Node::Ok)
        .map(|(n, s)| {
            format!(
                "{n} {}",
                if *s == Node::Missing {
                    "não existe"
                } else {
                    "sem permissão"
                }
            )
        })
        .collect();
    let mut detail = bad.join(", ");
    if !in_input_group() && user_listed_in_input_group() {
        detail.push_str(" — você já está no grupo input: saia e entre na sessão (ou reinicie)");
    } else {
        detail.push_str(" — rode: deckscreen-host fix-input");
    }
    Some(("fail", detail))
}

fn sudo_install(content: &str, dest: &str) -> bool {
    let tmp = util::runtime_dir().join(format!(
        "deckscreen-{}",
        Path::new(dest).file_name().unwrap().to_string_lossy()
    ));
    if util::write_file(&tmp, content, 0o644).is_err() {
        return false;
    }
    let ok = util::run_interactive("sudo", &["install", "-D", "-m", "0644", &tmp.to_string_lossy(), dest]);
    let _ = std::fs::remove_file(&tmp);
    ok
}

/// Interactive repair. Returns true when every node ends up writable now.
pub fn fix(yes: bool) -> bool {
    if problem().is_none() {
        println!("  \x1b[32m✔\x1b[0m controle virtual OK (/dev/uinput e /dev/uhid acessíveis)");
        return true;
    }
    if let Some(k) = reboot_pending() {
        println!("  \x1b[33m!\x1b[0m O kernel foi atualizado ({k} não está mais instalado): reinicie o PC.");
        return false;
    }
    println!("  O Sunshine precisa de /dev/uinput e /dev/uhid para criar o controle e o mouse virtuais.");
    println!("  Vou: carregar os módulos uinput/uhid, instalar a regra udev do Sunshine (se faltar)");
    println!("  e colocar você no grupo 'input'. Isso usa sudo.");
    if !util::ask_yes("Corrigir agora?", yes) {
        return false;
    }
    if !modules_configured() {
        sudo_install("uinput\nuhid\n", &format!("/etc/modules-load.d/{MODULES_NAME}"));
    }
    if !rules_installed() {
        sudo_install(RULES, &format!("/etc/udev/rules.d/{RULES_NAME}"));
    }
    util::run_interactive("sudo", &["modprobe", "uinput"]);
    util::run_interactive("sudo", &["modprobe", "uhid"]);
    util::run_interactive("sudo", &["udevadm", "control", "--reload-rules"]);
    util::run_interactive("sudo", &["udevadm", "trigger", "--property-match=DEVNAME=/dev/uinput"]);
    util::run_interactive("sudo", &["udevadm", "trigger", "--property-match=DEVNAME=/dev/uhid"]);
    if !in_input_group() && !user_listed_in_input_group() {
        let user = std::env::var("USER").unwrap_or_default();
        if !user.is_empty() {
            util::run_interactive("sudo", &["usermod", "-aG", "input", &user]);
        }
    }
    util::sleep_ms(500);
    match problem() {
        None => {
            println!("  \x1b[32m✔\x1b[0m controle virtual OK");
            if let Some(s) = crate::sunshine::service_name() {
                util::run("systemctl", &["--user", "restart", &s]);
                println!("  \x1b[32m✔\x1b[0m Sunshine reiniciado ({s})");
            }
            true
        }
        Some((_, d)) => {
            println!("  \x1b[33m!\x1b[0m ainda falta: {d}");
            println!("  \x1b[33m!\x1b[0m Saia e entre na sessão (ou reinicie o PC) para o grupo 'input' valer.");
            false
        }
    }
}

/// Recent Sunshine log lines that explain dropped input (empty = nothing suspicious).
pub fn sunshine_log_errors() -> Vec<String> {
    let Some(unit) = crate::sunshine::service_name() else {
        return vec![];
    };
    let Some(out) = util::run_stdout(
        "journalctl",
        &["--user", "-u", &unit, "-n", "600", "--no-pager", "-o", "cat"],
    ) else {
        return vec![];
    };
    log_input_errors(&out)
}

pub fn log_input_errors(log: &str) -> Vec<String> {
    const NEEDLES: [&str; 6] = [
        "unable to create libvirtualhid",
        "[libvirtualhid]",
        "is disabled due to",
        "/dev/uinput",
        "/dev/uhid",
        "permission denied",
    ];
    let mut v: Vec<String> = log
        .lines()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            (l.contains("error") || l.contains("warning") || l.contains("fatal"))
                && NEEDLES.iter().any(|n| l.contains(n))
        })
        .map(|l| l.trim().chars().take(200).collect())
        .collect();
    v.dedup();
    let n = v.len();
    if n > 3 {
        v.drain(..n - 3);
    }
    v
}

/// Names of the virtual devices Sunshine has created right now (only while streaming).
pub fn sunshine_devices() -> Vec<String> {
    std::fs::read_to_string("/proc/bus/input/devices")
        .map(|t| parse_devices(&t))
        .unwrap_or_default()
}

pub fn parse_devices(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("N: Name=\""))
        .map(|n| n.trim_end_matches('"').to_string())
        .filter(|n| n.to_ascii_lowercase().contains("sunshine"))
        .collect();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_sunshine_devices() {
        let t = "I: Bus=0003\nN: Name=\"Logitech USB Receiver\"\n\nN: Name=\"Sunshine (libvirtualhid) DualSense\"\nN: Name=\"Sunshine (libvirtualhid) Mouse\"\n";
        assert_eq!(
            super::parse_devices(t),
            vec!["Sunshine (libvirtualhid) DualSense", "Sunshine (libvirtualhid) Mouse"]
        );
    }

    use super::*;

    #[test]
    fn picks_input_errors_only() {
        let log = "[2026-11-02 10:00:00]: Info: Gamepad 0 connected\n\
                   [2026-11-02 10:00:01]: Warning: Unable to create libvirtualhid runtime\n\
                   [2026-11-02 10:00:02]: Error: [libvirtualhid] open /dev/uinput: Permission denied\n\
                   [2026-11-02 10:00:03]: Warning: Unknown mouse button: 9\n";
        let v = log_input_errors(log);
        assert_eq!(v.len(), 2);
        assert!(v[0].contains("Unable to create"));
        assert!(v[1].contains("Permission denied"));
    }

    #[test]
    fn missing_node_is_reported() {
        assert_eq!(node_state("/dev/definitely-not-here"), Node::Missing);
    }
}
