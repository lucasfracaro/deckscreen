//! DeckScreen host — turns a Steam Deck into a second screen + controller for
//! a Linux PC (Wii U GamePad style). Orchestrates Sunshine (PC) and Moonlight
//! (Deck); creates the Deck's screen on the PC.

pub mod config;
pub mod crypto;
pub mod daemon;
pub mod display;
pub mod doctor;
pub mod gpu;
pub mod http;
pub mod input;
pub mod install;
#[macro_use]
pub mod json;
pub mod net;
pub mod pairing;
pub mod sunshine;
#[macro_use]
pub mod updates;
pub mod util;

use json::Json;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "DeckScreen — Steam Deck como segunda tela e controle do PC

Uso: deckscreen-host <comando>

Instalação
  install [--yes] [--no-firewall] [--no-wol]   instala/atualiza tudo (pode rodar de novo)
  uninstall [--yes]                            remove do PC
  doctor                                       diagnóstico completo
  report                                       gera um .tar.gz com logs para pedir ajuda

Uso diário
  status          estado atual (tela, configurações, Decks)
  stop            remove a tela do Deck e restaura os monitores
  pending         pedidos de pareamento
  accept [ID]     aceita um pedido (sem ID: o único pendente)
  deny [ID]       recusa um pedido
  devices         Decks pareados
  forget ID       esquece um Deck

Interno
  daemon [-v]     o serviço (usado pelo systemd)
  hook start|stop chamado pelo Sunshine ao abrir/fechar o app DeckScreen
  sunshine-login [set]   mostra (ou troca) o login da interface web do Sunshine
  fix-input [--yes]      corrige o controle/mouse virtual (uinput, uhid, grupo input)
  update-sunshine [--yes] procura e instala uma versão nova do Sunshine
  gpu [auto|igpu|any]    notebook híbrido: Sunshine na GPU integrada para a NVIDIA poder dormir
  setup           só reconfigura o Sunshine
  gnome-virtual on|off   força o monitor extra no GNOME (Sunshine com portal_virtual_display)
  version
";

fn flag(args: &[String], f: &str) -> bool {
    args.iter().any(|a| a == f)
}

fn local(method: &str, path: &str, body: Option<&Json>, timeout: u64) -> Result<(u16, Json), i32> {
    daemon::local_call(method, path, body, timeout).map_err(|e| {
        eprintln!("O serviço não está respondendo ({e}).\n  systemctl --user status deckscreen-host");
        1
    })
}

fn cmd_hook(action: &str) -> i32 {
    util::init_log_file();
    // Never fail the stream because of us: always exit 0.
    let res = if action == "start" {
        let env_u = |k: &str, d: i64| std::env::var(k).ok().and_then(|v| v.parse::<i64>().ok()).unwrap_or(d);
        let body = obj! {
            "width" => env_u("SUNSHINE_CLIENT_WIDTH", 1280),
            "height" => env_u("SUNSHINE_CLIENT_HEIGHT", 800),
            "fps" => env_u("SUNSHINE_CLIENT_FPS", 60),
            "client" => std::env::var("SUNSHINE_CLIENT_NAME").unwrap_or_default(),
        };
        daemon::local_call("POST", "/local/start", Some(&body), 45)
    } else {
        daemon::local_call("POST", "/local/stop", Some(&Json::obj()), 30)
    };
    match res {
        Ok((code, j)) => info!("hook {action} -> {code} {}", j.to_compact()),
        Err(e) => error!("hook {action}: o serviço não respondeu ({e})"),
    }
    0
}

fn pick_pending(id: Option<&String>) -> Result<String, i32> {
    if let Some(id) = id {
        return Ok(id.clone());
    }
    let (_, j) = local("GET", "/local/pending", None, 8)?;
    let pend = j.get("pending").and_then(|p| p.as_arr()).cloned().unwrap_or_default();
    match pend.len() {
        1 => Ok(pend[0].str_at("id")),
        0 => {
            println!("Nenhum pedido pendente.");
            Err(1)
        }
        _ => {
            println!("Há mais de um pedido; informe o ID (veja 'deckscreen-host pending').");
            Err(1)
        }
    }
}

fn run(args: Vec<String>) -> i32 {
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    let rest = &args[2.min(args.len())..];
    match cmd {
        "daemon" => daemon::run(flag(rest, "-v")),
        "hook" => cmd_hook(rest.first().map(String::as_str).unwrap_or("")),
        "install" => install::install(&install::Opts {
            yes: flag(rest, "--yes") || flag(rest, "-y"),
            no_firewall: flag(rest, "--no-firewall"),
            no_wol: flag(rest, "--no-wol"),
        }),
        "uninstall" => install::uninstall(flag(rest, "--yes") || flag(rest, "-y")),
        "doctor" => {
            if doctor::print(display::detect_desktop()) {
                0
            } else {
                1
            }
        }
        "report" => match doctor::report(display::detect_desktop()) {
            Ok(p) => {
                println!("Relatório salvo em {p}\n(sem senhas nem chaves) — anexe-o ao pedir ajuda.");
                0
            }
            Err(e) => {
                eprintln!("Falhou: {e}");
                1
            }
        },
        "setup" => {
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let cfg = config::Config::load_default();
            sunshine::refresh_gnome_virtual(display::detect_desktop());
            match sunshine::configure(display::detect_desktop(), &exe, cfg.settings().mode == "mirror") {
                Ok(n) => {
                    n.iter().for_each(|l| println!("  {l}"));
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "status" => match local("GET", "/local/status", None, 10) {
            Ok((_, j)) => {
                println!("{}", j.pretty());
                0
            }
            Err(c) => c,
        },
        "stop" => {
            let quiet = flag(rest, "--quiet");
            match daemon::local_call("POST", "/local/stop", Some(&Json::obj()), 30) {
                Ok((200, _)) => {
                    if !quiet {
                        println!("Tela do Deck removida, monitores restaurados.");
                    }
                    0
                }
                Ok((c, j)) => {
                    eprintln!("Erro {c}: {}", j.str_at("error"));
                    1
                }
                Err(e) => {
                    if !quiet {
                        eprintln!("O serviço não respondeu ({e}).");
                    }
                    if quiet {
                        0
                    } else {
                        1
                    }
                }
            }
        }
        "pending" => match local("GET", "/local/pending", None, 8) {
            Ok((_, j)) => {
                let pend = j.get("pending").and_then(|p| p.as_arr()).cloned().unwrap_or_default();
                if pend.is_empty() {
                    println!("Nenhum pedido de pareamento pendente.");
                }
                for p in pend {
                    println!(
                        "{}  {}  ({})  PIN {}",
                        p.str_at("id"),
                        p.str_at("device_name"),
                        p.str_at("ip"),
                        p.str_at("pin")
                    );
                }
                0
            }
            Err(c) => c,
        },
        "accept" | "deny" => {
            let id = match pick_pending(rest.first()) {
                Ok(id) => id,
                Err(c) => return c,
            };
            let accept = cmd == "accept";
            match local("POST", "/local/decide", Some(&obj! {"id" => id, "accept" => accept}), 8) {
                Ok((200, _)) => {
                    println!("{}", if accept { "Aceito." } else { "Recusado." });
                    0
                }
                Ok(_) => {
                    println!("Pedido não encontrado.");
                    1
                }
                Err(c) => c,
            }
        }
        "devices" => {
            let cfg = config::Config::load_default();
            let devs = cfg.devices();
            if devs.is_empty() {
                println!("Nenhum Deck pareado.");
            }
            for (id, name, added) in devs {
                println!("{id}  {name}  (desde {})", &util::format_utc(added)[..10]);
            }
            0
        }
        "forget" => match rest.first() {
            Some(id) => {
                let mut cfg = config::Config::load_default();
                if cfg.forget(id) {
                    println!("Removido.");
                    0
                } else {
                    println!("Dispositivo não encontrado.");
                    1
                }
            }
            None => {
                eprintln!("uso: deckscreen-host forget ID");
                2
            }
        },
        "gpu" => gpu::command(rest.first().map(String::as_str)),
        "update-sunshine" => updates::update(rest.iter().any(|a| a == "--yes" || a == "-y")),
        "fix-input" => {
            if input::fix(rest.iter().any(|a| a == "--yes" || a == "-y")) {
                0
            } else {
                1
            }
        }
        "sunshine-login" => install::sunshine_login(rest.first().map(String::as_str) == Some("set")),
        "gnome-virtual" => {
            // Advanced: force/disable GNOME extra-screen mode (e.g. testing a Sunshine
            // build with portal_virtual_display that we can't detect, like an AppImage).
            let mut cfg = config::Config::load_default();
            match rest.first().map(String::as_str) {
                Some("on") | Some("off") => {
                    cfg.set_portal_virtual(rest[0] == "on");
                    let d = display::detect_desktop();
                    let on = sunshine::refresh_gnome_virtual(d);
                    let exe = std::env::current_exe()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if d == display::Desktop::Gnome {
                        let _ = sunshine::configure(d, &exe, cfg.settings().mode == "mirror");
                    }
                    println!(
                        "GNOME monitor extra: {}. Reinicie: systemctl --user restart deckscreen-host (e o Sunshine)",
                        if on { "ativado" } else { "desativado" }
                    );
                    0
                }
                _ => {
                    println!(
                        "forçado: {} · detectado no Sunshine: {}\nuso: deckscreen-host gnome-virtual on|off",
                        cfg.portal_virtual(),
                        sunshine::supports_portal_virtual()
                    );
                    0
                }
            }
        }
        "version" | "--version" | "-V" => {
            println!("deckscreen-host {VERSION}");
            0
        }
        "" if !util::is_tty() => {
            // Double-clicked (AppImage) without a terminal: open one to install.
            open_terminal_installer()
        }
        "help" | "--help" | "-h" | "" => {
            print!("{HELP}");
            0
        }
        other => {
            eprintln!("comando desconhecido: {other}\n");
            eprint!("{HELP}");
            2
        }
    }
}

/// Started from a file manager: run the installer inside a terminal window.
fn open_terminal_installer() -> i32 {
    let exe = std::env::var("APPIMAGE")
        .ok()
        .or_else(|| std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let script = format!("'{exe}' install; echo; read -p 'Pressione Enter para fechar…' _");
    let terms: [(&str, &[&str]); 6] = [
        ("konsole", &["-e", "bash", "-c"]),
        ("gnome-terminal", &["--", "bash", "-c"]),
        ("kgx", &["--", "bash", "-c"]),
        ("xfce4-terminal", &["-x", "bash", "-c"]),
        ("alacritty", &["-e", "bash", "-c"]),
        ("xterm", &["-e", "bash", "-c"]),
    ];
    for (t, pre) in terms {
        if util::which(t).is_some() {
            let mut a: Vec<&str> = pre.to_vec();
            a.push(&script);
            return if util::run_interactive(t, &a) { 0 } else { 1 };
        }
    }
    eprintln!("Abra um terminal e rode: {exe} install");
    1
}

fn main() {
    util::ensure_session_env();
    display::set_gnome_virtual(config::peek_portal_virtual());
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(run(args));
}
