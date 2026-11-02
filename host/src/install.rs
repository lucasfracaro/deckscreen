//! `deckscreen-host install` / `uninstall`: works on any distro (pacman, apt,
//! dnf, zypper) and from the AppImage, a pacman package or a plain binary.

use crate::config::Config;
use crate::display::{self, Desktop};
use crate::json::Json;
use crate::net;
use crate::sunshine;
use crate::util::{self, ask_yes};
use std::path::{Path, PathBuf};

const UNIT: &str = include_str!("../assets/deckscreen-host.service");
const SUNSHINE_DROPIN: &str = include_str!("../assets/sunshine-wait-kwin.conf");
const STOP_DESKTOP: &str = include_str!("../assets/deckscreen-stop.desktop");
const PLASMOID_META: &str = include_str!("../assets/plasmoid/metadata.json");
const PLASMOID_QML: &str = include_str!("../assets/plasmoid/contents/ui/main.qml");
pub const PLASMOID_ID: &str = "org.deckscreen.status";
const SYSTEM_UNIT: &str = "/usr/lib/systemd/user/deckscreen-host.service";

const TCP_PORTS: [&str; 4] = ["47984", "47989", "48010", "47555"];
const UDP_PORTS: [&str; 4] = ["47998", "47999", "48000", "47556"];

pub struct Opts {
    pub yes: bool,
    pub no_firewall: bool,
    pub no_wol: bool,
}

fn say(s: &str) {
    println!("\x1b[1m==>\x1b[0m {s}");
}
fn ok(s: &str) {
    println!("  \x1b[32m✔\x1b[0m {s}");
}
fn warn(s: &str) {
    println!("  \x1b[33m!\x1b[0m {s}");
}
fn fail(s: &str) -> i32 {
    eprintln!("\x1b[31m✘ {s}\x1b[0m");
    1
}

// ------------------------------------------------------------------ distro packages

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Pm {
    Pacman,
    Apt,
    Dnf,
    Zypper,
    Unknown,
}

pub fn package_manager() -> Pm {
    if util::which("pacman").is_some() {
        Pm::Pacman
    } else if util::which("apt-get").is_some() {
        Pm::Apt
    } else if util::which("dnf").is_some() {
        Pm::Dnf
    } else if util::which("zypper").is_some() {
        Pm::Zypper
    } else {
        Pm::Unknown
    }
}

/// Package that provides a binary, per package manager.
pub fn package_for(bin: &str, pm: Pm) -> &'static str {
    match (bin, pm) {
        ("krfb-virtualmonitor", _) => "krfb",
        ("kscreen-doctor", _) => "kscreen",
        ("notify-send", Pm::Apt) => "libnotify-bin",
        ("notify-send", Pm::Zypper) => "libnotify-tools",
        ("notify-send", _) => "libnotify",
        ("gdbus", Pm::Apt) => "libglib2.0-bin",
        ("gdbus", Pm::Zypper) => "glib2-tools",
        ("gdbus", _) => "glib2",
        ("curl", _) => "curl",
        ("ip", _) => "iproute2",
        ("python3", Pm::Pacman) => "python",
        ("python3", _) => "python3",
        _ => "",
    }
}

fn install_packages(pm: Pm, pkgs: &[&str]) -> bool {
    let mut args: Vec<&str> = match pm {
        Pm::Pacman => vec!["pacman", "-S", "--needed", "--noconfirm"],
        Pm::Apt => vec!["apt-get", "install", "-y"],
        Pm::Dnf => vec!["dnf", "install", "-y"],
        Pm::Zypper => vec!["zypper", "--non-interactive", "install"],
        Pm::Unknown => return false,
    };
    args.extend_from_slice(pkgs);
    util::run_interactive("sudo", &args)
}

fn required_bins(d: Desktop) -> Vec<&'static str> {
    let mut v = vec!["curl", "ip", "notify-send"];
    if d == Desktop::Kde {
        v.extend(["krfb-virtualmonitor", "kscreen-doctor", "gdbus"]);
    }
    if d == Desktop::Gnome {
        v.push("python3");
    }
    v
}

/// PyGObject (used by the GNOME helper to talk to Mutter).
pub fn has_pygobject() -> bool {
    util::run(
        "python3",
        &[
            "-c",
            "import gi; gi.require_version('Gio', '2.0'); from gi.repository import Gio",
        ],
    )
    .map(|o| o.status.success())
    .unwrap_or(false)
}

fn pygobject_package(pm: Pm) -> &'static str {
    match pm {
        Pm::Pacman => "python-gobject",
        Pm::Apt => "python3-gi",
        _ => "python3-gobject",
    }
}

// ------------------------------------------------------------------ Sunshine

fn arch() -> String {
    util::run_stdout("uname", &["-m"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "x86_64".into())
}

fn download_sunshine_appimage() -> Result<String, String> {
    download_appimage_from("LizardByte/Sunshine", "Sunshine.AppImage")
}

/// Latest `*_<arch>.AppImage` of a GitHub repo's releases → ~/Applications/<dest_name>.
fn download_appimage_from(repo: &str, dest_name: &str) -> Result<String, String> {
    let api = format!("https://api.github.com/repos/{repo}/releases/latest");
    let (ok, out) = util::run_text("curl", &["-fsSL", &api]);
    if !ok {
        return Err(format!("não consegui consultar o GitHub: {}", out.trim()));
    }
    let j = Json::parse(&out)?;
    let want = format!("_{}.AppImage", arch());
    let url = j
        .get("assets")
        .and_then(|a| a.as_arr())
        .and_then(|a| {
            a.iter()
                .find(|x| x.str_at("name").ends_with(&want))
                .map(|x| x.str_at("browser_download_url"))
        })
        .ok_or_else(|| format!("a release mais recente de {repo} não tem AppImage para {}", arch()))?;
    let dir = util::home().join("Applications");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join(dest_name);
    println!("  baixando {url}");
    if !util::run_interactive("curl", &["-fL", "--progress-bar", "-o", &dest.to_string_lossy(), &url]) {
        return Err("download falhou".into());
    }
    util::run("chmod", &["+x", &dest.to_string_lossy()]);
    Ok(dest.to_string_lossy().into_owned())
}

fn ensure_sunshine(pm: Pm, yes: bool) -> Result<String, String> {
    if let Some(b) = sunshine::find_binary() {
        ok(&format!("Sunshine: {b}"));
        return Ok(b);
    }
    if sunshine::is_flatpak_install() {
        warn("Você tem o Sunshine em Flatpak: ele não consegue capturar a tela virtual. Vou instalar a versão AppImage ao lado.");
    } else {
        println!("  O Sunshine não está instalado (é ele que transmite a tela, o som e o controle).");
    }
    if !ask_yes("Instalar o Sunshine agora?", yes) {
        return Err("o Sunshine é necessário".into());
    }
    if pm == Pm::Pacman {
        if util::run("pacman", &["-Si", "sunshine"])
            .map(|o| o.status.success())
            .unwrap_or(false)
            && util::run_interactive("sudo", &["pacman", "-S", "--needed", "--noconfirm", "sunshine"])
        {
            return sunshine::find_binary().ok_or_else(|| "instalado, mas não encontrado no PATH".into());
        }
        for helper in ["paru", "yay"] {
            if util::which(helper).is_some() && util::run_interactive(helper, &["-S", "--needed", "sunshine-bin"]) {
                return sunshine::find_binary().ok_or_else(|| "instalado, mas não encontrado no PATH".into());
            }
        }
    }
    let app = download_sunshine_appimage()?;
    println!("  Configurando o Sunshine (regras de controle virtual; pode pedir sua senha)…");
    if !util::run_interactive(&app, &["--install"]) {
        warn("o '--install' do Sunshine não terminou bem; o controle virtual pode precisar de ajuste manual");
    }
    Ok(app)
}

/// GNOME: the Deck becomes an extra monitor only when Sunshine supports
/// `portal_virtual_display` (proposed to the official project). Until then: mirror.
fn report_gnome_support(desktop: Desktop) -> bool {
    let on = sunshine::refresh_gnome_virtual(desktop);
    if on {
        ok("o Sunshine tem portal_virtual_display: o Deck vai virar um monitor extra no GNOME");
    } else {
        warn("GNOME: por enquanto o Deck espelha a tela e funciona como controle.");
        println!(
            "    Virar monitor extra no GNOME depende da opção portal_virtual_display, proposta ao Sunshine oficial."
        );
        println!("    Quando ela chegar numa versão do Sunshine, o DeckScreen ativa sozinho.");
    }
    on
}

/// `deckscreen-host sunshine-login [set]`: show the Sunshine web UI login that
/// DeckScreen uses, or change it (in Sunshine and in DeckScreen at once).
pub fn sunshine_login(set: bool) -> i32 {
    let mut cfg = Config::load_default();
    let (url, user, pass) = cfg.sunshine();
    if !set {
        if user.is_empty() {
            println!("Nenhum login salvo. Rode 'deckscreen-host install' ou 'deckscreen-host sunshine-login set'.");
            return 1;
        }
        let web = url.replace("127.0.0.1", "localhost");
        println!("Interface web do Sunshine: {web}\n  usuário: {user}\n  senha:   {pass}");
        println!("\nPara trocar por um usuário/senha seus: deckscreen-host sunshine-login set");
        return 0;
    }
    if !util::is_tty() {
        return fail("rode num terminal");
    }
    let Some(bin) = sunshine::find_binary() else {
        return fail("Sunshine não encontrado");
    };
    let new_user = util::ask_line(&format!(
        "Novo usuário [{}]: ",
        if user.is_empty() { "deckscreen" } else { &user }
    ));
    let new_user = if new_user.is_empty() {
        if user.is_empty() {
            "deckscreen".to_string()
        } else {
            user
        }
    } else {
        new_user
    };
    let p1 = util::ask_password("Nova senha: ");
    let p2 = util::ask_password("Repita a senha: ");
    if p1.is_empty() || p1 != p2 {
        return fail("as senhas estão vazias ou não conferem");
    }
    let service = sunshine::service_name();
    if let Some(s) = &service {
        util::run("systemctl", &["--user", "stop", s]);
    }
    let okc = util::run(&bin, &["--creds", &new_user, &p1])
        .map(|o| o.status.success())
        .unwrap_or(false);
    if let Some(s) = &service {
        util::run("systemctl", &["--user", "start", s]);
    }
    if !okc {
        return fail("o Sunshine não aceitou o novo login");
    }
    cfg.set_sunshine_creds(&new_user, &p1);
    ok(&format!(
        "Login do Sunshine trocado para '{new_user}' (no Sunshine e no DeckScreen)."
    ));
    0
}

fn sunshine_username() -> String {
    std::fs::read_to_string(util::sunshine_dir().join("sunshine_state.json"))
        .ok()
        .and_then(|s| Json::parse(&s).ok())
        .map(|j| j.str_at("username"))
        .unwrap_or_default()
}

fn ensure_credentials(cfg: &mut Config, sun_bin: &str, service: Option<&str>, yes: bool) {
    let have = sunshine_username();
    let (_, saved_user, saved_pass) = cfg.sunshine();
    if !have.is_empty()
        && have == saved_user
        && !saved_pass.is_empty()
        && sunshine::reachable(&sunshine::api_from_config(cfg))
    {
        ok(&format!("Usando o login do Sunshine já salvo ({saved_user})."));
        return;
    }
    if have.is_empty() {
        let user = "deckscreen";
        let pass = crate::crypto::token(18);
        if let Some(s) = service {
            util::run("systemctl", &["--user", "stop", s]);
        }
        if util::run(sun_bin, &["--creds", user, &pass])
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            cfg.set_sunshine_creds(user, &pass);
            ok(&format!(
                "Criei um login para a interface web do Sunshine: usuário '{user}', senha '{pass}'."
            ));
            println!("    Para ver de novo: deckscreen-host sunshine-login · para trocar: deckscreen-host sunshine-login set");
        } else {
            warn("não consegui criar o login do Sunshine; crie um em https://localhost:47990 e rode o instalador de novo");
        }
        return;
    }
    println!("  O Sunshine já tem um login ('{have}'). Digite a senha da interface web (https://localhost:47990).");
    println!("  Ela fica só neste PC, em ~/.config/deckscreen/config.json (permissão 600).");
    if yes || !util::is_tty() {
        warn("sem terminal interativo: pulei a senha. Rode 'deckscreen-host install' num terminal depois.");
        return;
    }
    let pass = util::ask_password("Senha do Sunshine: ");
    cfg.set_sunshine_creds(&have, &pass);
}

// ------------------------------------------------------------------ self install

fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("deckscreen-host"))
}

pub fn is_packaged(exe: &Path) -> bool {
    exe.starts_with("/usr/") && Path::new(SYSTEM_UNIT).exists()
}

/// Where the binary will live and whether it had to be copied there.
fn install_binary() -> Result<PathBuf, String> {
    let exe = current_exe();
    if is_packaged(&exe) {
        ok(&format!("instalado pelo gerenciador de pacotes: {}", exe.display()));
        return Ok(exe);
    }
    let dest = util::bin_dir().join("deckscreen-host");
    if exe != dest {
        std::fs::create_dir_all(util::bin_dir()).map_err(|e| e.to_string())?;
        // copy to a temp name and rename: works even while the old one is running
        let tmp = dest.with_extension("new");
        std::fs::copy(&exe, &tmp).map_err(|e| format!("não consegui copiar para {}: {e}", dest.display()))?;
        util::run("chmod", &["755", &tmp.to_string_lossy()]);
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    }
    ok(&format!("{}", dest.display()));
    Ok(dest)
}

fn install_kde_extras(bin: &str) {
    let dir = util::data_dir().join("plasmoid");
    let _ = util::write_file(&dir.join("metadata.json"), PLASMOID_META, 0o644);
    let _ = util::write_file(&dir.join("contents/ui/main.qml"), PLASMOID_QML, 0o644);
    let d = dir.to_string_lossy().into_owned();
    let tool = ["kpackagetool6", "kpackagetool"]
        .into_iter()
        .find(|t| util::which(t).is_some());
    match tool {
        Some(t) => {
            let upgraded = util::run(t, &["-t", "Plasma/Applet", "-u", &d])
                .map(|o| o.status.success())
                .unwrap_or(false);
            if upgraded
                || util::run(t, &["-t", "Plasma/Applet", "-i", &d])
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            {
                ok("widget \"DeckScreen\" instalado (clique com o botão direito na bandeja → Configurar → Entradas para mostrá-lo)");
            } else {
                warn("não consegui instalar o widget do Plasma");
            }
        }
        None => warn("kpackagetool6 não encontrado; widget do Plasma não instalado"),
    }
    let desktop = util::data_home().join("applications").join("deckscreen-stop.desktop");
    let _ = util::write_file(&desktop, &STOP_DESKTOP.replace("@BIN@", bin), 0o644);
    ok("atalho \"DeckScreen: desconectar o Steam Deck\" no menu (dá para pôr um atalho de teclado nele)");
}

fn setup_firewall(yes: bool) {
    let mark = || {
        let mut cfg = Config::load_default();
        cfg.data.set("firewall_configured", true);
        cfg.save();
    };
    let active = |svc: &str| {
        util::run("systemctl", &["is-active", "--quiet", svc])
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if util::which("ufw").is_some() && active("ufw") {
        if ask_yes(
            "O firewall (ufw) está ativo. Liberar as portas do Sunshine/DeckScreen? (pede sua senha)",
            yes,
        ) {
            for p in TCP_PORTS {
                util::run_interactive("sudo", &["ufw", "allow", &format!("{p}/tcp")]);
            }
            for p in UDP_PORTS {
                util::run_interactive("sudo", &["ufw", "allow", &format!("{p}/udp")]);
            }
            mark();
            ok("portas liberadas no ufw");
        }
    } else if util::which("firewall-cmd").is_some() && active("firewalld") {
        if ask_yes(
            "O firewalld está ativo. Liberar as portas do Sunshine/DeckScreen? (pede sua senha)",
            yes,
        ) {
            for p in TCP_PORTS {
                util::run_interactive(
                    "sudo",
                    &["firewall-cmd", "-q", "--permanent", &format!("--add-port={p}/tcp")],
                );
            }
            for p in UDP_PORTS {
                util::run_interactive(
                    "sudo",
                    &["firewall-cmd", "-q", "--permanent", &format!("--add-port={p}/udp")],
                );
            }
            util::run_interactive("sudo", &["firewall-cmd", "-q", "--reload"]);
            mark();
            ok("portas liberadas no firewalld");
        }
    } else {
        ok("nenhum firewall ativo detectado");
    }
}

fn setup_wol(yes: bool) {
    let ifaces = net::wired_ifaces();
    if ifaces.is_empty() {
        ok("PC sem cabo de rede ativo: Wake-on-LAN não se aplica (ele precisa de cabo)");
        return;
    }
    for iface in ifaces {
        match net::wol_state(&iface) {
            "on" => ok(&format!("Wake-on-LAN já ativo em {iface}")),
            _ => {
                if !ask_yes(
                    &format!("Ativar Wake-on-LAN em {iface}? (o Deck poderá ligar o PC da suspensão)"),
                    yes,
                ) {
                    continue;
                }
                let mut done = false;
                if let Some(conn) = net::nm_connection(&iface) {
                    let args = [
                        "connection",
                        "modify",
                        conn.as_str(),
                        "802-3-ethernet.wake-on-lan",
                        "magic",
                    ];
                    done = util::run("nmcli", &args).map(|o| o.status.success()).unwrap_or(false)
                        || util::run_interactive("sudo", &[&["nmcli"][..], &args[..]].concat());
                }
                if util::which("ethtool").is_some() {
                    done |= util::run_interactive("sudo", &["ethtool", "-s", &iface, "wol", "g"]);
                }
                if done {
                    ok(&format!(
                        "Wake-on-LAN ativado em {iface}. Confira também se está ligado na BIOS/UEFI."
                    ));
                } else {
                    warn("não consegui ativar; ative no NetworkManager/BIOS manualmente");
                }
            }
        }
    }
}

// ------------------------------------------------------------------ install / uninstall

pub fn install(o: &Opts) -> i32 {
    if util::uid() == 0 {
        return fail("Rode como seu usuário normal, NÃO com sudo.");
    }
    println!(
        "\n\x1b[1mDeckScreen {}\x1b[0m — Steam Deck como segunda tela e controle\n",
        crate::VERSION
    );
    let desktop = display::detect_desktop();
    let pm = package_manager();

    say("Área de trabalho");
    match display::capability(desktop) {
        "virtual" => ok(&format!("{}: o Deck vai virar um monitor extra", desktop.label())),
        _ => {
            if desktop == Desktop::Kde {
                warn("KDE Plasma em X11: entre na sessão \"Plasma (Wayland)\" para o Deck virar monitor extra.");
            }
            warn(&format!(
                "{}: modo espelhar (o Deck mostra a tela principal e funciona como controle)",
                desktop.label()
            ));
        }
    }

    say("Dependências");
    let missing: Vec<&str> = required_bins(desktop)
        .into_iter()
        .filter(|b| util::which(b).is_none())
        .collect();
    if !missing.is_empty() {
        let pkgs: Vec<&str> = missing
            .iter()
            .map(|b| package_for(b, pm))
            .filter(|p| !p.is_empty())
            .collect();
        println!("  Faltando: {}", pkgs.join(" "));
        if pm == Pm::Unknown || !ask_yes("Instalar agora? (pede sua senha)", o.yes) || !install_packages(pm, &pkgs) {
            warn(&format!("instale manualmente: {}", pkgs.join(" ")));
        }
    }
    if desktop == Desktop::Gnome
        && display::is_wayland()
        && sunshine::refresh_gnome_virtual(desktop)
        && !has_pygobject()
    {
        let pkg = pygobject_package(pm);
        println!("  Faltando: {pkg} (o DeckScreen usa para posicionar o monitor no GNOME)");
        if pm == Pm::Unknown || !ask_yes("Instalar agora? (pede sua senha)", o.yes) || !install_packages(pm, &[pkg]) {
            warn(&format!("instale manualmente: {pkg}"));
        }
    }
    if util::which("curl").is_none() {
        return fail("curl é obrigatório.");
    }
    ok("dependências conferidas");

    say("Sunshine");
    let sun_bin = match ensure_sunshine(pm, o.yes) {
        Ok(b) => b,
        Err(e) => return fail(&e),
    };

    let mut cfg = Config::load_default();
    if desktop == Desktop::Gnome && display::is_wayland() {
        say("GNOME");
        report_gnome_support(desktop);
    }

    say("Controle virtual (botões e trackpad do Deck)");
    if let Some(k) = crate::input::reboot_pending() {
        warn(&format!(
            "O kernel foi atualizado e o PC ainda não reiniciou ({k}): o controle virtual"
        ));
        warn("só funciona depois de reiniciar. Pode continuar a instalação e reiniciar no final.");
    } else {
        crate::input::fix(o.yes);
    }

    say("Instalando o DeckScreen");
    let bin = match install_binary() {
        Ok(b) => b,
        Err(e) => return fail(&e),
    };
    let bin_s = bin.to_string_lossy().into_owned();

    say("Login da interface web do Sunshine (para aceitar o PIN do Deck sozinho)");
    let service = sunshine::service_name();
    ensure_credentials(&mut cfg, &sun_bin, service.as_deref(), o.yes);

    say("Configurando o Sunshine");
    match sunshine::configure(desktop, &bin_s, cfg.settings().mode == "mirror") {
        Ok(notes) => notes.iter().for_each(|n| ok(n)),
        Err(e) => return fail(&format!("não consegui configurar o Sunshine: {e}")),
    }
    ok(&format!(
        "backups em {}/*.deckscreen-bak.*",
        util::sunshine_dir().display()
    ));

    say("Serviços");
    let unit_dir = util::user_unit_dir();
    let user_unit = unit_dir.join("deckscreen-host.service");
    if is_packaged(&bin) {
        let _ = std::fs::remove_file(&user_unit); // the package ships its own unit
    } else if let Err(e) = util::write_file(&user_unit, &UNIT.replace("@BIN@", &bin_s), 0o644) {
        return fail(&format!("não consegui criar o serviço: {e}"));
    }
    let service = service.or_else(sunshine::service_name);
    if let (Some(s), Desktop::Kde) = (&service, desktop) {
        let _ = util::write_file(
            &unit_dir.join(format!("{s}.d")).join("deckscreen.conf"),
            SUNSHINE_DROPIN,
            0o644,
        );
    }
    util::run("systemctl", &["--user", "daemon-reload"]);
    util::run("systemctl", &["--user", "enable", "deckscreen-host.service"]);
    util::run("systemctl", &["--user", "restart", "deckscreen-host.service"]);
    util::sleep_ms(800);
    if util::run(
        "systemctl",
        &["--user", "is-active", "--quiet", "deckscreen-host.service"],
    )
    .map(|o| o.status.success())
    .unwrap_or(false)
    {
        ok("deckscreen-host ativo");
    } else {
        warn(
            "o serviço não subiu: veja 'systemctl --user status deckscreen-host' (precisa de uma sessão gráfica ativa)",
        );
    }
    match &service {
        Some(s) => {
            util::run("systemctl", &["--user", "enable", s]);
            util::run("systemctl", &["--user", "restart", s]);
            ok(&format!("Sunshine reiniciado ({s})"));
        }
        None => warn("não achei o serviço de usuário do Sunshine: abra o Sunshine pelo menu de aplicativos"),
    }

    if desktop == Desktop::Kde {
        say("Integração com o KDE");
        install_kde_extras(&bin_s);
    }
    if !o.no_firewall {
        say("Firewall");
        setup_firewall(o.yes);
    }
    if !o.no_wol {
        say("Wake-on-LAN");
        setup_wol(o.yes);
    }

    say("Checagem final");
    util::sleep_ms(2500);
    let all_ok = crate::doctor::print(desktop);
    let ip = net::primary_ip().unwrap_or_else(|| "?".into());
    println!(
        "\x1b[32m\x1b[1m{}\x1b[0m
  No Steam Deck: ⋯ → Decky → DeckScreen → \"Procurar PC\" → \"Parear\".
  Aqui no PC aparece uma notificação com o PIN: confira e clique em \"Aceitar\".

  IP deste PC: {ip}  (se a busca automática não achar, digite no Deck)

  Comandos:  deckscreen-host doctor | status | stop | accept | report
",
        if all_ok {
            "Pronto!"
        } else {
            "Instalado, mas veja os itens marcados com ✘ acima."
        }
    );
    if desktop == Desktop::Gnome && display::gnome_virtual() {
        println!("  \x1b[1mGNOME, só na 1ª conexão:\x1b[0m aparece aqui no PC a janela \"Compartilhar tela\".");
        println!("  Escolha \"Monitor virtual\", marque \"Lembrar\" e clique em Compartilhar. Depois é automático.\n");
    }
    if !std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|p| Path::new(p) == util::bin_dir())
        && !is_packaged(&bin)
    {
        warn(&format!(
            "Adicione {} ao PATH para usar os comandos acima.",
            util::bin_dir().display()
        ));
    }
    0
}

pub fn uninstall(yes: bool) -> i32 {
    say("Removendo o DeckScreen do PC");
    let _ = crate::daemon::local_call("POST", "/local/stop", Some(&Json::obj()), 20);
    util::run("systemctl", &["--user", "disable", "--now", "deckscreen-host.service"]);
    let unit_dir = util::user_unit_dir();
    let _ = std::fs::remove_file(unit_dir.join("deckscreen-host.service"));
    for s in ["app-dev.lizardbyte.app.Sunshine.service", "sunshine.service"] {
        let _ = std::fs::remove_file(unit_dir.join(format!("{s}.d")).join("deckscreen.conf"));
    }
    util::run("systemctl", &["--user", "daemon-reload"]);
    for t in ["kpackagetool6", "kpackagetool"] {
        if util::which(t).is_some() {
            util::run(t, &["-t", "Plasma/Applet", "-r", PLASMOID_ID]);
            break;
        }
    }
    let _ = std::fs::remove_file(util::data_home().join("applications").join("deckscreen-stop.desktop"));
    sunshine::unconfigure();
    ok("Sunshine: app DeckScreen e output_name removidos (o Sunshine continua instalado)");
    let local_bin = util::bin_dir().join("deckscreen-host");
    if local_bin.exists() {
        let _ = std::fs::remove_file(&local_bin);
        ok(&format!("{} removido", local_bin.display()));
    }
    if !yes && util::ask_no("Apagar também a configuração (Decks pareados) e os logs?") {
        let _ = std::fs::remove_dir_all(util::config_dir());
        let _ = std::fs::remove_dir_all(util::data_dir());
        ok("configuração apagada");
    }
    if let Some(s) = sunshine::service_name() {
        util::run("systemctl", &["--user", "restart", &s]);
    }
    ok("pronto. No Deck: Decky → ⚙ → Plugins → DeckScreen → Desinstalar.");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn package_names() {
        assert_eq!(package_for("notify-send", Pm::Apt), "libnotify-bin");
        assert_eq!(package_for("notify-send", Pm::Pacman), "libnotify");
        assert_eq!(package_for("krfb-virtualmonitor", Pm::Dnf), "krfb");
        assert_eq!(package_for("gdbus", Pm::Zypper), "glib2-tools");
        assert!(required_bins(Desktop::Kde).contains(&"krfb-virtualmonitor"));
        assert!(!required_bins(Desktop::Gnome).contains(&"krfb-virtualmonitor"));
        assert!(UNIT.contains("@BIN@") && PLASMOID_META.contains(PLASMOID_ID));
    }
}
