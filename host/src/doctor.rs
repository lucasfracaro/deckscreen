//! `deckscreen-host doctor` (and the Deck's "Diagnóstico") plus `report`.

use crate::config::Config;
use crate::display::{self, Desktop, HYPR_OUTPUT, KDE_OUTPUT};
use crate::json::Json;
use crate::net;
use crate::obj;
use crate::sunshine::{self, APP_NAME};
use crate::util;

pub struct Check {
    pub label: String,
    /// "ok" | "warn" | "fail"
    pub status: &'static str,
    pub detail: String,
}

fn c(label: &str, status: &'static str, detail: impl Into<String>) -> Check {
    Check {
        label: label.into(),
        status,
        detail: detail.into(),
    }
}

fn tool(label: &str, bin: &str, pkg_hint: &str, required: bool) -> Check {
    match util::which(bin) {
        Some(p) => c(label, "ok", p.to_string_lossy()),
        None => c(
            label,
            if required { "fail" } else { "warn" },
            format!("{bin} não encontrado (pacote {pkg_hint})"),
        ),
    }
}

fn user_service_active(unit: &str) -> bool {
    util::run("systemctl", &["--user", "is-active", "--quiet", unit])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn checks(desktop: Desktop) -> Vec<Check> {
    sunshine::refresh_gnome_virtual(desktop);
    let mut v = vec![];
    let cap = display::capability(desktop);
    v.push(c(
        "Área de trabalho",
        if cap == "virtual" { "ok" } else { "warn" },
        if cap == "virtual" {
            format!("{}: o Deck vira um monitor extra", desktop.label())
        } else {
            format!(
                "{}: só espelhamento (tela extra exige KDE Plasma Wayland ou Hyprland)",
                desktop.label()
            )
        },
    ));
    if desktop == Desktop::Kde {
        v.push(c(
            "Sessão Wayland",
            if display::is_wayland() { "ok" } else { "fail" },
            std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        ));
        v.push(tool("krfb (tela virtual)", "krfb-virtualmonitor", "krfb", true));
        v.push(tool("kscreen-doctor", "kscreen-doctor", "kscreen", true));
        v.push(tool("gdbus (Modo Wii U)", "gdbus", "glib2", false));
    }
    if desktop == Desktop::Gnome {
        v.push(c(
            "Sessão Wayland",
            if display::is_wayland() { "ok" } else { "fail" },
            std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        ));
        v.push(if display::gnome_virtual() {
            c(
                "Sunshine do DeckScreen",
                "ok",
                "monitor virtual do GNOME ativado (portal_virtual_display)",
            )
        } else {
            c(
                "Sunshine do DeckScreen",
                "warn",
                "não instalado: o Deck só espelha. Rode 'deckscreen-host install' para instalar",
            )
        });
        if display::gnome_virtual() {
            v.push(if crate::install::has_pygobject() {
                c("PyGObject (posicionar o monitor)", "ok", "python3-gi")
            } else {
                c(
                    "PyGObject (posicionar o monitor)",
                    "fail",
                    "instale python3-gi / python-gobject",
                )
            });
        }
    }
    if desktop == Desktop::Hyprland {
        v.push(tool("hyprctl", "hyprctl", "hyprland", true));
    }
    v.push(tool(
        "notify-send (aprovar pareamento)",
        "notify-send",
        "libnotify",
        false,
    ));
    v.push(tool("curl", "curl", "curl", true));

    match sunshine::find_binary() {
        Some(b) => v.push(c("Sunshine instalado", "ok", b)),
        None if sunshine::is_flatpak_install() => v.push(c(
            "Sunshine instalado",
            "warn",
            "versão Flatpak: a captura da tela virtual não funciona nela; use o pacote ou o AppImage",
        )),
        None => v.push(c("Sunshine instalado", "fail", "não encontrado")),
    }
    match sunshine::service_name() {
        Some(s) => v.push(c(
            "Serviço do Sunshine",
            if user_service_active(&s) { "ok" } else { "fail" },
            s,
        )),
        None => v.push(c("Serviço do Sunshine", "warn", "serviço de usuário não encontrado")),
    }
    v.extend(virtual_input_checks());
    let cfg = Config::load_default();
    let api = sunshine::api_from_config(&cfg);
    v.push(if cfg.sunshine().1.is_empty() {
        c(
            "Acesso à API do Sunshine",
            "fail",
            "sem usuário salvo: rode 'deckscreen-host install'",
        )
    } else if sunshine::reachable(&api) {
        c("Acesso à API do Sunshine", "ok", "usuário e senha funcionando")
    } else {
        c(
            "Acesso à API do Sunshine",
            "fail",
            "Sunshine parado ou senha errada: rode 'deckscreen-host install'",
        )
    });

    let conf = std::fs::read_to_string(util::sunshine_dir().join("sunshine.conf")).unwrap_or_default();
    let (want_cap, want_out) = match desktop {
        Desktop::Kde => (Some("kwin"), Some(KDE_OUTPUT)),
        Desktop::Hyprland => (Some("wlr"), Some(HYPR_OUTPUT)),
        _ => (None, None),
    };
    if desktop == Desktop::Gnome && display::gnome_virtual() {
        let cap_now = sunshine::conf_get(&conf, "capture").unwrap_or_default();
        let pv = sunshine::conf_get(&conf, "portal_virtual_display").unwrap_or_default();
        v.push(c(
            "Configuração do Sunshine",
            if cap_now == "portal" && !pv.is_empty() {
                "ok"
            } else {
                "fail"
            },
            format!("capture = {cap_now:?}, portal_virtual_display = {pv:?}"),
        ));
    }
    if let (Some(wc), Some(wo)) = (want_cap, want_out) {
        let cap_now = sunshine::conf_get(&conf, "capture").unwrap_or_default();
        let out_now = sunshine::conf_get(&conf, "output_name").unwrap_or_default();
        v.push(c(
            "Configuração do Sunshine",
            if cap_now == wc && out_now == wo { "ok" } else { "fail" },
            format!("capture = {cap_now:?}, output_name = {out_now:?} (esperado {wc} / {wo})"),
        ));
    }
    let apps = std::fs::read_to_string(util::sunshine_dir().join("apps.json"))
        .ok()
        .and_then(|s| Json::parse(&s).ok());
    let has_app = apps
        .and_then(|a| a.get("apps").and_then(|x| x.as_arr()).cloned())
        .map(|a| a.iter().any(|x| x.str_at("name") == APP_NAME))
        .unwrap_or(false);
    v.push(c(
        "App DeckScreen no Sunshine",
        if has_app { "ok" } else { "fail" },
        if has_app {
            "presente"
        } else {
            "ausente: rode 'deckscreen-host install'"
        },
    ));
    v.push(c(
        "Serviço DeckScreen",
        if user_service_active("deckscreen-host.service") {
            "ok"
        } else {
            "warn"
        },
        "systemctl --user status deckscreen-host",
    ));

    match net::primary_ip().zip(net::iface_for("1.1.1.1")) {
        Some((ip, iface)) => {
            let wired = !net::is_wireless(&iface);
            v.push(c(
                "Rede do PC",
                if wired { "ok" } else { "warn" },
                format!(
                    "{ip} via {iface} ({})",
                    if wired {
                        "cabo"
                    } else {
                        "Wi-Fi: use cabo para menos atraso"
                    }
                ),
            ));
            if wired {
                let wol = net::wol_state(&iface);
                v.push(c(
                    "Wake-on-LAN",
                    if wol == "on" { "ok" } else { "warn" },
                    match wol {
                        "on" => "ativo".to_string(),
                        "off" => "desligado: 'deckscreen-host install' pode ativar".to_string(),
                        _ => "não consegui verificar (precisa de nmcli ou ethtool)".to_string(),
                    },
                ));
            }
        }
        None => v.push(c("Rede do PC", "fail", "sem rota de rede")),
    }
    let fw_active = util::run("systemctl", &["is-active", "--quiet", "ufw"])
        .map(|o| o.status.success())
        .unwrap_or(false)
        || util::run("systemctl", &["is-active", "--quiet", "firewalld"])
            .map(|o| o.status.success())
            .unwrap_or(false);
    if fw_active {
        v.push(if cfg.data.bool_at("firewall_configured", false) {
            c("Firewall", "ok", "ativo, com as portas liberadas pelo instalador")
        } else {
            c(
                "Firewall",
                "warn",
                "ativo: libere TCP 47984,47989,48010,47555 e UDP 47998-48000,47556 (o install faz isso)",
            )
        });
    }
    v
}

/// Sunshine creates the virtual controller/mouse through /dev/uinput and /dev/uhid.
fn virtual_input_checks() -> Vec<Check> {
    let label = "Controle virtual (uinput/uhid)";
    let mut v = vec![match crate::input::problem() {
        None => c(label, "ok", "/dev/uinput e /dev/uhid acessíveis"),
        Some((status, detail)) => c(label, status, detail),
    }];
    let errs = crate::input::sunshine_log_errors();
    if !errs.is_empty() {
        v.push(c(
            "Entrada no log do Sunshine",
            "warn",
            format!("{} (rode: deckscreen-host fix-input)", errs.join(" | ")),
        ));
    }
    v
}

pub fn checks_json(desktop: Desktop) -> Json {
    let list: Vec<Json> = checks(desktop)
        .into_iter()
        .map(|c| obj! {"label" => c.label, "status" => c.status, "detail" => c.detail})
        .collect();
    obj! {"checks" => list}
}

pub fn print(desktop: Desktop) -> bool {
    let mut all_ok = true;
    println!("DeckScreen {} — diagnóstico\n", crate::VERSION);
    for ch in checks(desktop) {
        let icon = match ch.status {
            "ok" => "\x1b[32m✔\x1b[0m",
            "warn" => "\x1b[33m!\x1b[0m",
            _ => {
                all_ok = false;
                "\x1b[31m✘\x1b[0m"
            }
        };
        println!("  {icon} {:<34} {}", ch.label, ch.detail);
    }
    println!();
    all_ok
}

/// Bundle logs + diagnostics (no passwords/keys) into ~/deckscreen-relatorio-*.tar.gz
pub fn report(desktop: Desktop) -> Result<String, String> {
    let dir = std::env::temp_dir().join(format!("deckscreen-relatorio-{}", util::now_secs()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut doctor = String::new();
    for ch in checks(desktop) {
        doctor.push_str(&format!("[{}] {}: {}\n", ch.status, ch.label, ch.detail));
    }
    let _ = std::fs::write(dir.join("doctor.txt"), doctor);
    let _ = std::fs::write(
        dir.join("config-sem-segredos.json"),
        Config::load_default().redacted().pretty(),
    );
    let env: Vec<String> = [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
        "WAYLAND_DISPLAY",
        "DESKTOP_SESSION",
    ]
    .iter()
    .map(|k| format!("{k}={}", std::env::var(k).unwrap_or_default()))
    .collect();
    let _ = std::fs::write(
        dir.join("ambiente.txt"),
        env.join("\n") + "\n" + &util::run_text("uname", &["-a"]).1,
    );
    for (name, cmd, args) in [
        ("kscreen-doctor.txt", "kscreen-doctor", vec!["-o"]),
        ("hyprctl-monitors.txt", "hyprctl", vec!["monitors", "all"]),
        (
            "journal-deckscreen.txt",
            "journalctl",
            vec!["--user", "-u", "deckscreen-host", "-n", "400", "--no-pager"],
        ),
        (
            "journal-sunshine.txt",
            "journalctl",
            vec![
                "--user",
                "-u",
                "app-dev.lizardbyte.app.Sunshine",
                "-n",
                "300",
                "--no-pager",
            ],
        ),
    ] {
        if util::which(cmd).is_some() {
            let _ = std::fs::write(dir.join(name), util::run_text(cmd, &args).1);
        }
    }
    for f in [
        "deckscreen.log",
        "deckscreen.log.1",
        "krfb.log",
        "app.log",
        "window-rules.js",
    ] {
        let _ = std::fs::copy(util::data_dir().join(f), dir.join(f));
    }
    for f in ["sunshine.conf", "apps.json", "sunshine.log"] {
        let _ = std::fs::copy(util::sunshine_dir().join(f), dir.join(format!("sunshine-{f}")));
    }
    let out = util::home().join(format!("deckscreen-relatorio-{}.tar.gz", util::now_secs()));
    let parent = dir.parent().unwrap().to_string_lossy().into_owned();
    let base = dir.file_name().unwrap().to_string_lossy().into_owned();
    let (ok, msg) = util::run_text("tar", &["-czf", &out.to_string_lossy(), "-C", &parent, &base]);
    let _ = std::fs::remove_dir_all(&dir);
    if ok {
        Ok(out.to_string_lossy().into_owned())
    } else {
        Err(msg)
    }
}
