//! Is there a newer Sunshine? Answers for each way DeckScreen may have installed it:
//! the CachyOS/Arch repo package, an AUR package (sunshine-bin, ...) or the official
//! AppImage in ~/Applications. Results are cached (network: mirrors, AUR, GitHub) so the
//! doctor and the daemon's status stay fast. `deckscreen-host update-sunshine` applies it.

use crate::json::Json;
use crate::{sunshine, util};
use std::cmp::Ordering;
use std::path::PathBuf;

const MAX_AGE_S: u64 = 6 * 3600;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Check {
    /// repo | aur | appimage
    pub source: String,
    pub package: String,
    pub current: String,
    /// "" = unknown (offline, rate limited...)
    pub latest: String,
    pub checked: u64,
}

impl Check {
    pub fn newer(&self) -> bool {
        !self.latest.is_empty()
            && !self.current.is_empty()
            && cmp_versions(&self.latest, &self.current) == Ordering::Greater
    }

    /// The command that updates it.
    pub fn how(&self) -> String {
        match self.source.as_str() {
            "repo" => "sudo pacman -Syu".into(),
            "aur" => format!("{} -Syu", aur_helper().unwrap_or("paru")),
            _ => "deckscreen-host update-sunshine".into(),
        }
    }

    fn to_json(&self) -> Json {
        obj! {
            "source" => self.source.clone(), "package" => self.package.clone(), "current" => self.current.clone(),
            "latest" => self.latest.clone(), "checked" => self.checked as f64,
        }
    }

    fn from_json(j: &Json) -> Check {
        Check {
            source: j.str_at("source"),
            package: j.str_at("package"),
            current: j.str_at("current"),
            latest: j.str_at("latest"),
            checked: j.get("checked").and_then(|v| v.as_f64()).unwrap_or(0.0) as u64,
        }
    }
}

/// Compare versions like "2025.924.154138-1", "v2026.1.0", "2026.1102.1-2" by their numbers.
pub fn cmp_versions(a: &str, b: &str) -> Ordering {
    let nums = |s: &str| -> Vec<u64> {
        s.trim_start_matches(['v', 'V'])
            .split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (x, y) = (nums(a), nums(b));
    for i in 0..x.len().max(y.len()) {
        match x.get(i).unwrap_or(&0).cmp(y.get(i).unwrap_or(&0)) {
            Ordering::Equal => continue,
            o => return o,
        }
    }
    Ordering::Equal
}

fn aur_helper() -> Option<&'static str> {
    ["paru", "yay"].into_iter().find(|h| util::which(h).is_some())
}

fn cache_file() -> PathBuf {
    util::data_dir().join("sunshine-update.json")
}

fn appimage_tag_file() -> PathBuf {
    util::data_dir().join("Sunshine.AppImage.version")
}

/// What is installed and how (no network).
pub fn installed() -> Option<Check> {
    let bin = sunshine::find_binary()?;
    if bin.to_lowercase().ends_with(".appimage") {
        let tag = std::fs::read_to_string(appimage_tag_file()).unwrap_or_default();
        return Some(Check {
            source: "appimage".into(),
            package: "Sunshine.AppImage".into(),
            current: tag.trim().to_string(),
            ..Default::default()
        });
    }
    let owner = util::run_stdout("pacman", &["-Qqo", &bin])?;
    let package = owner.lines().next()?.trim().to_string();
    let current = util::run_stdout("pacman", &["-Q", &package])?
        .split_whitespace()
        .nth(1)?
        .to_string();
    let in_repo = util::run("pacman", &["-Si", &package]).is_some_and(|o| o.status.success());
    Some(Check {
        source: if in_repo { "repo" } else { "aur" }.into(),
        package,
        current,
        ..Default::default()
    })
}

/// `checkupdates` line → new version of `package`.
pub fn parse_checkupdates(out: &str, package: &str) -> Option<String> {
    out.lines()
        .find_map(|l| match l.split_whitespace().collect::<Vec<_>>()[..] {
            [name, _cur, "->", new] if name == package => Some(new.to_string()),
            _ => None,
        })
}

fn latest_repo(c: &Check) -> String {
    if util::which("checkupdates").is_some() {
        // syncs a private copy of the databases: no root, doesn't touch pacman's own
        if let Some(o) = util::run("timeout", &["40", "checkupdates"]) {
            match o.status.code() {
                Some(0) => {
                    let out = String::from_utf8_lossy(&o.stdout);
                    return parse_checkupdates(&out, &c.package).unwrap_or_else(|| c.current.clone());
                }
                Some(2) => return c.current.clone(), // no updates at all
                _ => {}
            }
        }
    }
    // fallback: the local sync database (as fresh as the last pacman -Sy)
    util::run_stdout("pacman", &["-Si", &c.package])
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("Version"))
                .and_then(|l| l.split(':').nth(1))
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_default()
}

fn latest_aur(c: &Check) -> String {
    let url = format!("https://aur.archlinux.org/rpc/v5/info?arg[]={}", c.package);
    util::run_stdout("curl", &["-fsSL", "-m", "15", "-g", &url])
        .and_then(|t| Json::parse(&t).ok())
        .and_then(|j| {
            j.get("results")
                .and_then(|r| r.as_arr())
                .and_then(|r| r.first())
                .map(|r| r.str_at("Version"))
        })
        .unwrap_or_default()
}

fn latest_appimage() -> String {
    util::run_stdout(
        "curl",
        &[
            "-fsSL",
            "-m",
            "15",
            "https://api.github.com/repos/LizardByte/Sunshine/releases/latest",
        ],
    )
    .and_then(|t| Json::parse(&t).ok())
    .map(|j| j.str_at("tag_name"))
    .unwrap_or_default()
}

/// Cached answer; refreshed (network) when older than 6 h or when the installed version changed.
pub fn check(force: bool) -> Option<Check> {
    let now_installed = installed()?;
    if !force {
        if let Some(c) = cached() {
            if c.source == now_installed.source
                && c.current == now_installed.current
                && util::now_secs().saturating_sub(c.checked) < MAX_AGE_S
            {
                return Some(c);
            }
        }
    }
    let mut c = now_installed;
    c.latest = match c.source.as_str() {
        "repo" => latest_repo(&c),
        "aur" => latest_aur(&c),
        _ => latest_appimage(),
    };
    c.checked = util::now_secs();
    let _ = std::fs::create_dir_all(util::data_dir());
    let _ = std::fs::write(cache_file(), c.to_json().to_compact());
    Some(c)
}

/// Last result without touching the network (for the status the Deck polls).
pub fn cached() -> Option<Check> {
    let t = std::fs::read_to_string(cache_file()).ok()?;
    Some(Check::from_json(&Json::parse(&t).ok()?))
}

/// One doctor line.
pub fn doctor_line() -> Option<(&'static str, String)> {
    let c = check(false)?;
    Some(if c.newer() {
        (
            "warn",
            format!("{} → {} disponível: rode '{}'", c.current, c.latest, c.how()),
        )
    } else if c.latest.is_empty() {
        (
            "ok",
            format!(
                "{} (não consegui consultar a versão mais nova agora)",
                if c.current.is_empty() { "?" } else { &c.current }
            ),
        )
    } else {
        ("ok", format!("{} (a mais nova)", c.current))
    })
}

/// `deckscreen-host update-sunshine`
pub fn update(yes: bool) -> i32 {
    let Some(c) = check(true) else {
        eprintln!("Sunshine não encontrado. Rode 'deckscreen-host install'.");
        return 1;
    };
    if !c.newer() {
        if c.latest.is_empty() {
            println!(
                "Não consegui consultar a versão mais nova (sem internet?). Instalada: {}",
                c.current
            );
            return 1;
        }
        println!("O Sunshine já está na versão mais nova ({}).", c.current);
        return 0;
    }
    println!("Sunshine {} → {} ({})", c.current, c.latest, c.source);
    let ok = match c.source.as_str() {
        "repo" => {
            println!("No Arch/CachyOS não se atualiza um pacote sozinho: isto atualiza o sistema todo.");
            util::ask_yes("Rodar 'sudo pacman -Syu' agora?", yes) && util::run_interactive("sudo", &["pacman", "-Syu"])
        }
        "aur" => match aur_helper() {
            Some(h) => util::ask_yes(&format!("Rodar '{h} -Syu' agora?"), yes) && util::run_interactive(h, &["-Syu"]),
            None => {
                println!("Atualize o pacote AUR '{}' com o seu gerenciador.", c.package);
                false
            }
        },
        _ => match crate::install::download_sunshine_appimage() {
            Ok(p) => {
                println!("  ✔ {p}");
                true
            }
            Err(e) => {
                eprintln!("  ✘ {e}");
                false
            }
        },
    };
    if !ok {
        return 1;
    }
    let _ = check(true);
    if let Some(s) = sunshine::service_name() {
        util::run("systemctl", &["--user", "restart", &s]);
        println!("  ✔ Sunshine reiniciado ({s})");
    }
    println!("  Se o kernel também foi atualizado, reinicie o PC.");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(cmp_versions("2026.1102.1-1", "2025.924.154138-2"), Ordering::Greater);
        assert_eq!(cmp_versions("v2025.924.154138", "2025.924.154138-1"), Ordering::Less);
        assert_eq!(cmp_versions("v2026.1.0", "2026.1.0"), Ordering::Equal);
        assert_eq!(cmp_versions("2026.1.0-2", "2026.1.0-1"), Ordering::Greater);
    }

    #[test]
    fn checkupdates_line() {
        let out = "linux-cachyos 7.3.0-4 -> 7.3.1-1\nsunshine 2025.924.154138-2 -> 2026.1102.1-1\n";
        assert_eq!(parse_checkupdates(out, "sunshine").as_deref(), Some("2026.1102.1-1"));
        assert_eq!(parse_checkupdates(out, "sunshine-bin"), None);
    }

    #[test]
    fn newer_and_how() {
        let mut c = Check {
            source: "repo".into(),
            package: "sunshine".into(),
            current: "2025.924.1-1".into(),
            latest: "2026.1.0-1".into(),
            checked: 1,
        };
        assert!(c.newer());
        assert_eq!(c.how(), "sudo pacman -Syu");
        assert_eq!(Check::from_json(&c.to_json()), c);
        c.latest = String::new();
        assert!(!c.newer());
        c.source = "appimage".into();
        assert_eq!(c.how(), "deckscreen-host update-sunshine");
    }
}
