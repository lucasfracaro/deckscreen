//! Hybrid laptops (NVIDIA + AMD/Intel integrated): keep Sunshine on the integrated GPU so
//! the NVIDIA one can sleep (runtime D3).
//!
//! Left alone, Sunshine probes NVENC first (a CUDA context) and glvnd/Vulkan load NVIDIA's
//! libraries, which wakes the dGPU and keeps it awake. On a hybrid laptop KWin composites
//! on the integrated GPU anyway, so its screencast buffers live there: encoding with
//! VA-API on that GPU is also the zero-copy path. Games still run on the NVIDIA (PRIME
//! offload) and wake it only while they run.
//!
//! Pinning = `encoder = vaapi` + `adapter_name = /dev/dri/by-path/…-render` in
//! sunshine.conf, plus a systemd drop-in that limits EGL/Vulkan to Mesa for Sunshine only.

use crate::config::Config;
use crate::sunshine;
use crate::util;
use std::path::{Path, PathBuf};

pub const MODES: [&str; 3] = ["auto", "igpu", "any"];
const DROPIN: &str = "deckscreen-gpu.conf";
const NVIDIA: u16 = 0x10de;
const AMD: u16 = 0x1002;
const INTEL: u16 = 0x8086;

#[derive(Debug, Clone, PartialEq)]
pub struct Gpu {
    /// /dev/dri/renderD128
    pub node: String,
    /// 0000:05:00.0
    pub pci: String,
    pub vendor: u16,
    /// amdgpu, i915, xe, nvidia, nouveau
    pub driver: String,
}

impl Gpu {
    /// Stable path: renderD numbers can swap between boots.
    pub fn adapter_path(&self, root: &Path) -> String {
        let by = format!("/dev/dri/by-path/pci-{}-render", self.pci);
        if root.join(by.trim_start_matches('/')).exists() {
            by
        } else {
            self.node.clone()
        }
    }

    pub fn label(&self) -> String {
        let v = match self.vendor {
            NVIDIA => "NVIDIA",
            AMD => "AMD",
            INTEL => "Intel",
            _ => "GPU",
        };
        format!("{v} ({}, {})", self.driver, self.pci)
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default().trim().to_string()
}

/// Render nodes from sysfs. `root` is "/" except in tests.
pub fn list(root: &Path) -> Vec<Gpu> {
    let drm = root.join("sys/class/drm");
    let mut v: Vec<Gpu> = std::fs::read_dir(&drm)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let dev = e.path().join("device");
            let vendor = u16::from_str_radix(read(&dev.join("vendor")).trim_start_matches("0x"), 16).ok()?;
            let real = std::fs::canonicalize(&dev).ok()?;
            let pci = real.file_name()?.to_string_lossy().into_owned();
            let driver = std::fs::canonicalize(dev.join("driver"))
                .ok()
                .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            Some(Gpu {
                node: format!("/dev/dri/{name}"),
                pci,
                vendor,
                driver,
            })
        })
        .collect();
    v.sort_by(|a, b| a.node.cmp(&b.node));
    v
}

/// (integrated, nvidia) when this is an NVIDIA + AMD/Intel machine with both GPUs active.
pub fn hybrid(gpus: &[Gpu]) -> Option<(Gpu, Gpu)> {
    let nv = gpus.iter().find(|g| g.vendor == NVIDIA)?;
    let ig = gpus
        .iter()
        .find(|g| g.vendor == INTEL)
        .or_else(|| gpus.iter().find(|g| g.vendor == AMD))?;
    Some((ig.clone(), nv.clone()))
}

fn mesa_files(root: &Path, ig: &Gpu) -> (Option<String>, Option<String>) {
    let egl = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
    let icd = match ig.vendor {
        INTEL => "/usr/share/vulkan/icd.d/intel_icd.x86_64.json",
        _ => "/usr/share/vulkan/icd.d/radeon_icd.x86_64.json",
    };
    let have = |p: &str| root.join(p.trim_start_matches('/')).exists().then(|| p.to_string());
    (have(egl), have(icd))
}

/// systemd drop-in for Sunshine: only Mesa's EGL/Vulkan, so NVIDIA's libraries never open the GPU.
pub fn dropin_text(egl: Option<&str>, icd: Option<&str>) -> String {
    let mut t = String::from(
        "# Installed by DeckScreen: keep Sunshine on the integrated GPU (deckscreen-host gpu)\n[Service]\n",
    );
    if let Some(e) = egl {
        t.push_str(&format!("Environment=__EGL_VENDOR_LIBRARY_FILENAMES={e}\n"));
    }
    if let Some(i) = icd {
        t.push_str(&format!(
            "Environment=VK_DRIVER_FILES={i}\nEnvironment=VK_ICD_FILENAMES={i}\n"
        ));
    }
    t.push_str("Environment=__GLX_VENDOR_LIBRARY_NAME=mesa\n");
    t
}

/// sunshine.conf values: pin to `adapter`, or undo a pin DeckScreen made.
pub fn conf_values(old: &str, adapter: Option<&str>) -> Vec<(&'static str, Option<String>)> {
    match adapter {
        Some(a) => vec![("encoder", Some("vaapi".into())), ("adapter_name", Some(a.into()))],
        None => {
            let ours = sunshine::conf_get(old, "adapter_name").is_some_and(|a| a.starts_with("/dev/dri/"));
            if ours && sunshine::conf_get(old, "encoder").as_deref() == Some("vaapi") {
                vec![("encoder", None), ("adapter_name", None)]
            } else {
                vec![]
            }
        }
    }
}

pub fn mode(cfg: &Config) -> String {
    let m = cfg.data.get("sunshine").map(|s| s.str_at("gpu")).unwrap_or_default();
    if MODES.contains(&m.as_str()) {
        m
    } else {
        "auto".into()
    }
}

/// The adapter to pin to, if any, for this machine and mode.
pub fn target(mode: &str) -> Option<(Gpu, Gpu)> {
    if mode == "any" {
        return None;
    }
    hybrid(&list(Path::new("/")))
}

fn dropin_paths() -> Vec<PathBuf> {
    let dir = util::user_unit_dir();
    let mut names = vec![
        "app-dev.lizardbyte.app.Sunshine.service".to_string(),
        "sunshine.service".to_string(),
    ];
    if let Some(s) = sunshine::service_name() {
        if !names.contains(&s) {
            names.push(s);
        }
    }
    names
        .into_iter()
        .map(|n| dir.join(format!("{n}.d")).join(DROPIN))
        .collect()
}

/// Write sunshine.conf + drop-in for the current mode. Returns human notes. Does not restart Sunshine.
pub fn apply(cfg: &Config) -> Vec<String> {
    let mode = mode(cfg);
    let conf = util::sunshine_dir().join("sunshine.conf");
    let old = std::fs::read_to_string(&conf).unwrap_or_default();
    let mut notes = vec![];
    match target(&mode) {
        Some((ig, nv)) => {
            let adapter = ig.adapter_path(Path::new("/"));
            let vals = conf_values(&old, Some(&adapter));
            let refs: Vec<(&str, Option<&str>)> = vals.iter().map(|(k, v)| (*k, v.as_deref())).collect();
            let _ = std::fs::create_dir_all(util::sunshine_dir());
            let _ = std::fs::write(&conf, sunshine::merge_conf(&old, &refs));
            let (egl, icd) = mesa_files(Path::new("/"), &ig);
            let text = dropin_text(egl.as_deref(), icd.as_deref());
            if let Some(svc) = sunshine::service_name() {
                let p = util::user_unit_dir().join(format!("{svc}.d")).join(DROPIN);
                let _ = util::write_file(&p, &text, 0o644);
            }
            notes.push(format!(
                "Sunshine na GPU integrada: {} (VA-API, {adapter}); a {} pode dormir",
                ig.label(),
                nv.label()
            ));
        }
        None => {
            remove();
            if mode == "any" {
                notes.push("Sunshine escolhe a GPU sozinho".into());
            }
        }
    }
    util::run("systemctl", &["--user", "daemon-reload"]);
    notes
}

/// Undo the pin (sunshine.conf keys DeckScreen set + drop-in). Used by "any" and uninstall.
pub fn remove() {
    let conf = util::sunshine_dir().join("sunshine.conf");
    if let Ok(old) = std::fs::read_to_string(&conf) {
        let vals = conf_values(&old, None);
        if !vals.is_empty() {
            let refs: Vec<(&str, Option<&str>)> = vals.iter().map(|(k, v)| (*k, v.as_deref())).collect();
            let _ = std::fs::write(&conf, sunshine::merge_conf(&old, &refs));
        }
    }
    for p in dropin_paths() {
        let _ = std::fs::remove_file(p);
    }
}

// ------------------------------------------------------------------ NVIDIA power state

/// runtime_status (active/suspended) and power/control (auto/on) of the NVIDIA GPU.
pub fn nvidia_power(root: &Path, nv: &Gpu) -> (String, String) {
    let dev = root.join("sys/bus/pci/devices").join(&nv.pci).join("power");
    (read(&dev.join("runtime_status")), read(&dev.join("control")))
}

/// Names of this user's processes holding /dev/nvidia* open.
pub fn nvidia_holders() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()))
        .filter_map(|e| {
            let fds = std::fs::read_dir(e.path().join("fd")).ok()?;
            let holds = fds.flatten().any(|fd| {
                std::fs::read_link(fd.path())
                    .map(|t| t.to_string_lossy().starts_with("/dev/nvidia"))
                    .unwrap_or(false)
            });
            holds.then(|| read(&e.path().join("comm")))
        })
        .filter(|n| !n.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Doctor lines (only on hybrid machines).
pub fn doctor_lines(cfg: &Config) -> Vec<(&'static str, &'static str, String)> {
    let Some((ig, nv)) = hybrid(&list(Path::new("/"))) else {
        return vec![];
    };
    let mode = mode(cfg);
    let conf = std::fs::read_to_string(util::sunshine_dir().join("sunshine.conf")).unwrap_or_default();
    let pinned = sunshine::conf_get(&conf, "encoder").as_deref() == Some("vaapi")
        && sunshine::conf_get(&conf, "adapter_name")
            .is_some_and(|a| a == ig.adapter_path(Path::new("/")) || a == ig.node);
    let mut v = vec![if mode == "any" {
        (
            "GPU do Sunshine",
            "ok",
            "qualquer uma (escolha sua: deckscreen-host gpu any)".to_string(),
        )
    } else if pinned {
        ("GPU do Sunshine", "ok", format!("integrada: {}", ig.label()))
    } else {
        (
            "GPU do Sunshine",
            "warn",
            "pode acordar a NVIDIA e mantê-la ligada: rode 'deckscreen-host gpu igpu'".to_string(),
        )
    }];
    let (state, control) = nvidia_power(Path::new("/"), &nv);
    let holders = nvidia_holders();
    let detail = match state.as_str() {
        "suspended" => "dormindo".to_string(),
        "" => "estado desconhecido".to_string(),
        s if holders.is_empty() => format!("{s} (nenhum programa seu está usando)"),
        s => format!("{s}; em uso por: {}", holders.join(", ")),
    };
    v.push(if control == "on" {
        (
            "NVIDIA em repouso",
            "warn",
            format!("{detail}. O desligamento automático está desativado (power/control=on): a NVIDIA nunca dorme, com ou sem o Sunshine"),
        )
    } else {
        ("NVIDIA em repouso", "ok", detail)
    });
    v
}

/// `deckscreen-host gpu [auto|igpu|any]`
pub fn command(arg: Option<&str>) -> i32 {
    let mut cfg = Config::load_default();
    let gpus = list(Path::new("/"));
    match arg {
        None => {
            println!("GPUs:");
            for g in &gpus {
                println!("  {}  {}", g.node, g.label());
            }
            match hybrid(&gpus) {
                Some((ig, _)) => println!("Notebook híbrido: integrada = {}", ig.label()),
                None => println!("Não é um notebook híbrido NVIDIA + integrada: nada a fazer."),
            }
            println!("Modo: {} (auto | igpu | any)", mode(&cfg));
            for (name, _, detail) in doctor_lines(&cfg) {
                println!("  {name}: {detail}");
            }
            0
        }
        Some(m) if MODES.contains(&m) => {
            cfg.data.entry_obj("sunshine").set("gpu", m);
            cfg.save();
            for n in apply(&cfg) {
                println!("  ✔ {n}");
            }
            if let Some(s) = sunshine::service_name() {
                util::run("systemctl", &["--user", "restart", &s]);
                println!("  ✔ Sunshine reiniciado ({s})");
            }
            0
        }
        Some(m) => {
            eprintln!("modo desconhecido: {m} (use auto, igpu ou any)");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fake_sysfs(root: &Path, gpus: &[(&str, &str, &str, &str)]) {
        for (render, pci, vendor, driver) in gpus {
            let dev = root.join("sys/devices/pci0000:00").join(pci);
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::write(dev.join("vendor"), format!("{vendor}\n")).unwrap();
            let drv = root.join("sys/bus/pci/drivers").join(driver);
            std::fs::create_dir_all(&drv).unwrap();
            let _ = symlink(&drv, dev.join("driver"));
            let node = root.join("sys/class/drm").join(render);
            std::fs::create_dir_all(&node).unwrap();
            symlink(&dev, node.join("device")).unwrap();
            let by = root.join("dev/dri/by-path");
            std::fs::create_dir_all(&by).unwrap();
            std::fs::write(by.join(format!("pci-{pci}-render")), "").unwrap();
        }
    }

    #[test]
    fn finds_hybrid_pair() {
        let root = std::env::temp_dir().join(format!("ds-gpu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        fake_sysfs(
            &root,
            &[
                ("renderD128", "0000:01:00.0", "0x10de", "nvidia"),
                ("renderD129", "0000:05:00.0", "0x1002", "amdgpu"),
            ],
        );
        let gpus = list(&root);
        assert_eq!(gpus.len(), 2);
        let (ig, nv) = hybrid(&gpus).unwrap();
        assert_eq!((ig.driver.as_str(), ig.pci.as_str()), ("amdgpu", "0000:05:00.0"));
        assert_eq!(nv.vendor, NVIDIA);
        assert_eq!(ig.adapter_path(&root), "/dev/dri/by-path/pci-0000:05:00.0-render");
        assert!(hybrid(&gpus[1..]).is_none(), "AMD only is not hybrid");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn conf_pin_and_unpin() {
        let pin = conf_values("", Some("/dev/dri/by-path/pci-0000:05:00.0-render"));
        assert_eq!(pin[0], ("encoder", Some("vaapi".to_string())));
        let ours = "encoder = vaapi\nadapter_name = /dev/dri/by-path/pci-0000:05:00.0-render\n";
        assert_eq!(conf_values(ours, None), vec![("encoder", None), ("adapter_name", None)]);
        // the user's own choice is left alone
        assert!(conf_values("encoder = nvenc\n", None).is_empty());
    }

    #[test]
    fn dropin_only_mesa() {
        let t = dropin_text(
            Some("/usr/share/glvnd/egl_vendor.d/50_mesa.json"),
            Some("/usr/share/vulkan/icd.d/radeon_icd.x86_64.json"),
        );
        assert!(t.contains("[Service]"));
        assert!(t.contains("__EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json"));
        assert!(t.contains("VK_DRIVER_FILES=/usr/share/vulkan/icd.d/radeon_icd.x86_64.json"));
        assert!(!dropin_text(None, None).contains("VK_DRIVER_FILES"));
    }
}
