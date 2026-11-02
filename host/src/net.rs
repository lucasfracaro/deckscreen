//! Network facts the Deck needs: which interface reaches it, MAC for
//! Wake-on-LAN, cable or Wi-Fi, and whether WoL is enabled.

use crate::json::Json;
use crate::util;

pub fn iface_for(ip: &str) -> Option<String> {
    let out = util::run_stdout("ip", &["route", "get", ip])?;
    let mut it = out.split_whitespace();
    while let Some(w) = it.next() {
        if w == "dev" {
            return it.next().map(str::to_string);
        }
    }
    None
}

pub fn local_ip_for(ip: &str) -> Option<String> {
    let out = util::run_stdout("ip", &["route", "get", ip])?;
    let mut it = out.split_whitespace();
    while let Some(w) = it.next() {
        if w == "src" {
            return it.next().map(str::to_string);
        }
    }
    None
}

pub fn mac_of(iface: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/address"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|m| m.len() == 17 && m != "00:00:00:00:00:00")
}

pub fn is_wireless(iface: &str) -> bool {
    std::path::Path::new(&format!("/sys/class/net/{iface}/wireless")).exists()
        || std::path::Path::new(&format!("/sys/class/net/{iface}/phy80211")).exists()
}

/// NetworkManager connection currently active on an interface.
pub fn nm_connection(iface: &str) -> Option<String> {
    let out = util::run_stdout("nmcli", &["-t", "-f", "NAME,DEVICE", "connection", "show", "--active"])?;
    out.lines().find_map(|l| {
        let (name, dev) = l.rsplit_once(':')?;
        (dev == iface).then(|| name.replace("\\:", ":"))
    })
}

/// "on" / "off" / "unknown"
pub fn wol_state(iface: &str) -> &'static str {
    if let Some(conn) = nm_connection(iface) {
        if let Some(v) = util::run_stdout(
            "nmcli",
            &["-g", "802-3-ethernet.wake-on-lan", "connection", "show", &conn],
        ) {
            let v = v.trim().to_lowercase();
            if v.contains("magic") {
                return "on";
            }
            if v.contains("ignore") || v.contains("default") {
                // NetworkManager leaves the driver setting alone; ask ethtool below
            } else if !v.is_empty() {
                return "off";
            }
        }
    }
    if let Some(out) = util::run_stdout("ethtool", &[iface]) {
        for l in out.lines() {
            let l = l.trim();
            if let Some(v) = l.strip_prefix("Wake-on:") {
                return if v.contains('g') { "on" } else { "off" };
            }
        }
    }
    "unknown"
}

pub fn info_for_peer(peer_ip: &str) -> Json {
    let Some(iface) = iface_for(peer_ip) else {
        return obj! {"iface" => Json::Null};
    };
    let wired = !is_wireless(&iface);
    obj! {
        "iface" => iface.clone(),
        "mac" => mac_of(&iface),
        "wired" => wired,
        "ip" => local_ip_for(peer_ip),
        "wol" => if wired { wol_state(&iface) } else { "unsupported" },
    }
}

/// Primary LAN IP of this PC (for the installer's final message).
pub fn primary_ip() -> Option<String> {
    local_ip_for("1.1.1.1")
}

/// Wired interfaces that are up (for the installer's WoL step).
pub fn wired_ifaces() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir("/sys/class/net") else {
        return vec![];
    };
    rd.flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n != "lo"
                && !n.starts_with("docker")
                && !n.starts_with("veth")
                && !n.starts_with("br-")
                && !n.starts_with("virbr")
        })
        .filter(|n| !is_wireless(n))
        .filter(|n| std::path::Path::new(&format!("/sys/class/net/{n}/device")).exists())
        .filter(|n| {
            std::fs::read_to_string(format!("/sys/class/net/{n}/operstate"))
                .map(|s| s.trim() == "up")
                .unwrap_or(false)
        })
        .collect()
}
