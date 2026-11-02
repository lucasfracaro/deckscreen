//! The Deck's screen on the PC: desktop detection and one backend per desktop.
//!
//!  * KDE Plasma (Wayland): krfb-virtualmonitor creates the output, kscreen-doctor
//!    places it, a KWin script moves windows (Wii U mode).
//!  * Hyprland (experimental): `hyprctl output create headless`, monitor and
//!    window rules through `hyprctl keyword`.
//!  * Anything else: mirror only (Sunshine streams the main screen).

use crate::config::{Profile, Settings};
use crate::json::Json;
use crate::util;
use std::process::{Child, Command, Stdio};

pub const VM_NAME: &str = "DeckScreen";
pub const KDE_OUTPUT: &str = "Virtual-DeckScreen";
pub const HYPR_OUTPUT: &str = "DeckScreen";
const VNC_PORT: &str = "5959";
const KWIN_SCRIPT: &str = "deckscreen-window-rules";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desktop {
    Kde,
    Hyprland,
    Gnome,
    Other,
}

impl Desktop {
    pub fn id(&self) -> &'static str {
        match self {
            Desktop::Kde => "kde",
            Desktop::Hyprland => "hyprland",
            Desktop::Gnome => "gnome",
            Desktop::Other => "other",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Desktop::Kde => "KDE Plasma",
            Desktop::Hyprland => "Hyprland",
            Desktop::Gnome => "GNOME",
            Desktop::Other => "outra área de trabalho",
        }
    }
}

pub fn is_wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE")
        .map(|v| v == "wayland")
        .unwrap_or(false)
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn process_running(name: &str) -> bool {
    util::run("pgrep", &["-x", name])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn detect_desktop() -> Desktop {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() && util::which("hyprctl").is_some() {
        return Desktop::Hyprland;
    }
    let cur = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    if cur.contains("kde") {
        return Desktop::Kde;
    }
    if cur.contains("hyprland") {
        return Desktop::Hyprland;
    }
    if cur.contains("gnome") {
        return Desktop::Gnome;
    }
    if process_running("kwin_wayland") {
        return Desktop::Kde;
    }
    if process_running("Hyprland") {
        return Desktop::Hyprland;
    }
    if process_running("gnome-shell") {
        return Desktop::Gnome;
    }
    Desktop::Other
}

/// "virtual" when the Deck can be a real extra monitor, otherwise "mirror".
pub fn capability(d: Desktop) -> &'static str {
    match d {
        Desktop::Kde if is_wayland() => "virtual",
        Desktop::Hyprland => "virtual",
        _ => "mirror",
    }
}

#[derive(Clone, Debug)]
pub struct StartReq {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub client: String,
}

pub trait Backend: Send {
    fn name(&self) -> &'static str;
    /// Create and configure the Deck screen. Returns the output name ("" = mirror only).
    fn start(&mut self, req: &StartReq, s: &Settings, p: &Profile) -> Result<String, String>;
    fn stop(&mut self);
    /// False when the virtual screen died on its own.
    fn alive(&mut self) -> bool;
    /// Called once at daemon start: undo leftovers from a crash.
    fn recover(&mut self) {}
}

pub fn backend_for(d: Desktop) -> Box<dyn Backend> {
    match capability(d) {
        "virtual" if d == Desktop::Kde => Box::new(KdeBackend::new(Box::new(RealKScreen))),
        "virtual" => Box::new(HyprBackend::default()),
        _ => Box::new(MirrorBackend),
    }
}

// ============================================================================ geometry

/// Size in desktop (logical) coordinates for a kscreen-doctor output object.
pub fn logical_size(o: &Json) -> (i64, i64) {
    let size = o.get("size").cloned().unwrap_or_default();
    let (mut w, mut h) = (size.i64_at("width", 0), size.i64_at("height", 0));
    let cur = match o.get("currentModeId") {
        Some(Json::Str(s)) => s.clone(),
        Some(Json::Num(n)) => format!("{}", *n as i64),
        _ => String::new(),
    };
    if let Some(modes) = o.get("modes").and_then(|m| m.as_arr()) {
        for m in modes {
            let id = match m.get("id") {
                Some(Json::Str(s)) => s.clone(),
                Some(Json::Num(n)) => format!("{}", *n as i64),
                _ => continue,
            };
            if id == cur {
                let ms = m.get("size").cloned().unwrap_or_default();
                w = ms.i64_at("width", w);
                h = ms.i64_at("height", h);
            }
        }
    }
    let scale = o.f64_at("scale", 1.0);
    let scale = if scale <= 0.0 { 1.0 } else { scale };
    if matches!(o.i64_at("rotation", 1), 2 | 8) {
        std::mem::swap(&mut w, &mut h);
    }
    ((w as f64 / scale).round() as i64, (h as f64 / scale).round() as i64)
}

fn pos(o: &Json) -> (i64, i64) {
    let p = o.get("pos").cloned().unwrap_or_default();
    (p.i64_at("x", 0), p.i64_at("y", 0))
}

fn enabled(o: &Json) -> bool {
    o.bool_at("enabled", false)
}

pub fn primary_of(outputs: &[Json]) -> Option<&Json> {
    let en: Vec<&Json> = outputs.iter().filter(|o| enabled(o)).collect();
    en.iter()
        .find(|o| o.i64_at("priority", 0) == 1)
        .copied()
        .or_else(|| en.first().copied())
}

/// Place the virtual output next to the main monitor on the requested side.
/// If that needs negative coordinates the other monitors are shifted instead
/// (they are restored on teardown). Returns (name, x, y) for every enabled output.
pub fn plan_layout(outputs: &[Json], vm: &str, position: &str) -> Vec<(String, i64, i64)> {
    let Some(vm_o) = outputs.iter().find(|o| o.str_at("name") == vm) else {
        return vec![];
    };
    let (vw, vh) = logical_size(vm_o);
    let others: Vec<Json> = outputs
        .iter()
        .filter(|o| enabled(o) && o.str_at("name") != vm)
        .cloned()
        .collect();
    if others.is_empty() {
        return vec![(vm.to_string(), 0, 0)];
    }
    let prim = primary_of(&others).unwrap_or(&others[0]);
    let (px, py) = pos(prim);
    let (pw, ph) = logical_size(prim);
    let boxes: Vec<(i64, i64, i64, i64)> = others
        .iter()
        .map(|o| {
            let (x, y) = pos(o);
            let (w, h) = logical_size(o);
            (x, y, x + w, y + h)
        })
        .collect();
    let min_x = boxes.iter().map(|b| b.0).min().unwrap();
    let min_y = boxes.iter().map(|b| b.1).min().unwrap();
    let max_x = boxes.iter().map(|b| b.2).max().unwrap();
    let max_y = boxes.iter().map(|b| b.3).max().unwrap();
    let (vx, vy) = match position {
        "left" => (min_x - vw, py + (ph - vh) / 2),
        "above" => (px + (pw - vw) / 2, min_y - vh),
        "below" => (px + (pw - vw) / 2, max_y),
        _ => (max_x, py + (ph - vh) / 2),
    };
    let mut layout: Vec<(String, i64, i64)> = others
        .iter()
        .map(|o| {
            let (x, y) = pos(o);
            (o.str_at("name"), x, y)
        })
        .collect();
    layout.push((vm.to_string(), vx, vy));
    let dx = -layout.iter().map(|l| l.1).min().unwrap().min(0);
    let dy = -layout.iter().map(|l| l.2).min().unwrap().min(0);
    layout.into_iter().map(|(n, x, y)| (n, x + dx, y + dy)).collect()
}

// ============================================================================ KDE

pub trait KScreen: Send {
    fn outputs(&self) -> Result<Vec<Json>, String>;
    fn apply(&self, ops: &[String]) -> bool;
    fn spawn_krfb(&self, w: u32, h: u32) -> Result<Child, String>;
    fn kwin(&self, method: &str, args: &[&str]) -> bool;
    fn kill_stale(&self);
}

pub struct RealKScreen;

impl KScreen for RealKScreen {
    fn outputs(&self) -> Result<Vec<Json>, String> {
        let o = util::run("kscreen-doctor", &["-j"]).ok_or("kscreen-doctor não encontrado")?;
        if !o.status.success() {
            return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
        }
        let text = String::from_utf8_lossy(&o.stdout);
        let start = text.find('{').ok_or("saída do kscreen-doctor sem JSON")?;
        let j = Json::parse(&text[start..])?;
        Ok(j.get("outputs").and_then(|x| x.as_arr()).cloned().unwrap_or_default())
    }

    fn apply(&self, ops: &[String]) -> bool {
        if ops.is_empty() {
            return true;
        }
        let args: Vec<&str> = ops.iter().map(String::as_str).collect();
        let (ok, out) = util::run_text("kscreen-doctor", &args);
        let ok = ok && !out.to_lowercase().contains("failed");
        if !ok {
            crate::warn!("kscreen-doctor {} falhou: {}", ops.join(" "), out.trim());
        }
        ok
    }

    fn spawn_krfb(&self, w: u32, h: u32) -> Result<Child, String> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(util::data_dir().join("krfb.log"))
            .map_err(|e| e.to_string())?;
        let log2 = log.try_clone().map_err(|e| e.to_string())?;
        let password = crate::crypto::token(12);
        use std::os::unix::process::CommandExt;
        Command::new("krfb-virtualmonitor")
            .args([
                "--name",
                VM_NAME,
                "--resolution",
                &format!("{w}x{h}"),
                "--password",
                &password,
                "--port",
                VNC_PORT,
            ])
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log2)
            .process_group(0)
            .spawn()
            .map_err(|e| format!("krfb-virtualmonitor não iniciou ({e}). Instale o pacote krfb."))
    }

    fn kwin(&self, method: &str, args: &[&str]) -> bool {
        let m = format!("org.kde.kwin.Scripting.{method}");
        let mut a = vec![
            "call",
            "--session",
            "--dest",
            "org.kde.KWin",
            "--object-path",
            "/Scripting",
            "--method",
            &m,
        ];
        a.extend_from_slice(args);
        util::run("gdbus", &a).map(|o| o.status.success()).unwrap_or(false)
    }

    fn kill_stale(&self) {
        let _ = util::run("pkill", &["-f", &format!("krfb-virtualmonitor.*--name {VM_NAME}( |$)")]);
    }
}

#[derive(Clone, Debug)]
struct Snap {
    name: String,
    enabled: bool,
    x: i64,
    y: i64,
    primary: bool,
}

fn restore_file() -> std::path::PathBuf {
    util::data_dir().join("restore.json")
}

fn snaps_to_json(v: &[Snap]) -> Json {
    Json::Arr(
        v.iter()
            .map(|s| crate::obj! {"name" => s.name.clone(), "enabled" => s.enabled, "x" => s.x, "y" => s.y, "primary" => s.primary})
            .collect(),
    )
}

fn snaps_from_json(j: &Json) -> Vec<Snap> {
    j.as_arr()
        .map(|a| {
            a.iter()
                .map(|o| Snap {
                    name: o.str_at("name"),
                    enabled: o.bool_at("enabled", true),
                    x: o.i64_at("x", 0),
                    y: o.i64_at("y", 0),
                    primary: o.bool_at("primary", false),
                })
                .filter(|s| !s.name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub struct KdeBackend {
    ks: Box<dyn KScreen>,
    proc: Option<Child>,
    output: Option<String>,
    snapshot: Vec<Snap>,
    script_loaded: bool,
    pub poll_ms: u64,
}

impl KdeBackend {
    pub fn new(ks: Box<dyn KScreen>) -> Self {
        KdeBackend {
            ks,
            proc: None,
            output: None,
            snapshot: vec![],
            script_loaded: false,
            poll_ms: 300,
        }
    }

    fn kill_proc(&mut self) {
        if let Some(mut p) = self.proc.take() {
            if p.try_wait().ok().flatten().is_none() {
                let _ = util::run("kill", &["-TERM", &p.id().to_string()]);
                for _ in 0..40 {
                    if p.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    util::sleep_ms(100);
                }
                let _ = p.kill();
                let _ = p.wait();
            }
        }
    }

    fn load_rules(&mut self, out: &str, rules: &[String], fullscreen: bool) {
        let path = util::data_dir().join("window-rules.js");
        if let Err(e) = util::write_file(&path, &render_kwin_script(out, rules, fullscreen), 0o644) {
            crate::warn!("não salvei o script do KWin: {e}");
            return;
        }
        self.ks.kwin("unloadScript", &[KWIN_SCRIPT]);
        let p = path.to_string_lossy().into_owned();
        if self.ks.kwin("loadScript", &[&p, KWIN_SCRIPT]) {
            self.ks.kwin("start", &[]);
            self.script_loaded = true;
            crate::info!("regras de janela ativas: {:?}", rules);
        } else {
            crate::warn!("não consegui carregar o script do KWin (gdbus/KWin indisponível?)");
        }
    }
}

impl Backend for KdeBackend {
    fn name(&self) -> &'static str {
        "kde"
    }

    fn start(&mut self, req: &StartReq, s: &Settings, p: &Profile) -> Result<String, String> {
        if self.proc.is_some() {
            self.stop();
        }
        let before = self
            .ks
            .outputs()
            .map_err(|e| format!("kscreen-doctor indisponível: {e}"))?;
        self.snapshot = before
            .iter()
            .filter(|o| o.str_at("name") != KDE_OUTPUT && o.bool_at("connected", true))
            .map(|o| {
                let (x, y) = pos(o);
                Snap {
                    name: o.str_at("name"),
                    enabled: enabled(o),
                    x,
                    y,
                    primary: o.i64_at("priority", 0) == 1,
                }
            })
            .collect();
        let before_names: Vec<String> = before.iter().map(|o| o.str_at("name")).collect();
        // persisted so a crash (or power cut) never leaves the PC without monitors
        let _ = util::write_file(&restore_file(), &snaps_to_json(&self.snapshot).to_compact(), 0o600);
        self.ks.kill_stale();
        self.proc = Some(self.ks.spawn_krfb(req.width, req.height)?);
        crate::info!(
            "krfb-virtualmonitor iniciado {}x{}@{} para '{}'",
            req.width,
            req.height,
            req.fps,
            req.client
        );

        let mut out = None;
        for _ in 0..50 {
            util::sleep_ms(self.poll_ms);
            if let Some(pr) = self.proc.as_mut() {
                if pr.try_wait().ok().flatten().is_some() {
                    break;
                }
            }
            let Ok(now) = self.ks.outputs() else { continue };
            let names: Vec<String> = now.iter().map(|o| o.str_at("name")).collect();
            if names.iter().any(|n| n == KDE_OUTPUT) {
                out = Some(KDE_OUTPUT.to_string());
                break;
            }
            if let Some(n) = names.iter().find(|n| !before_names.contains(n)) {
                out = Some(n.clone());
                break;
            }
        }
        let Some(out) = out else {
            self.kill_proc();
            self.snapshot.clear();
            let _ = std::fs::remove_file(restore_file());
            return Err(
                "a tela virtual não apareceu (você está na sessão Plasma Wayland? o krfb está instalado?)".into(),
            );
        };
        self.output = Some(out.clone());
        crate::info!("tela virtual: {out}");

        let (w, h, fps) = (req.width, req.height, req.fps);
        self.ks
            .apply(&[format!("output.{out}.addCustomMode.{w}.{h}.{}.full", fps * 1000)]);
        if !self.ks.apply(&[format!("output.{out}.mode.{w}x{h}@{fps}")]) {
            self.ks.apply(&[format!("output.{out}.mode.{w}x{h}")]);
        }
        if (s.scale - 1.0).abs() > 0.001 {
            self.ks.apply(&[format!("output.{out}.scale.{}", s.scale)]);
        }
        match s.orientation.as_str() {
            "portrait" => {
                self.ks.apply(&[format!("output.{out}.rotation.right")]);
            }
            "portrait_flipped" => {
                self.ks.apply(&[format!("output.{out}.rotation.left")]);
            }
            _ => {}
        }

        match s.mode.as_str() {
            "deck_only" => {
                // enable/primary first so there is never zero screens
                self.ks.apply(&[
                    format!("output.{out}.enable"),
                    format!("output.{out}.position.0,0"),
                    format!("output.{out}.primary"),
                ]);
                let off: Vec<String> = self
                    .snapshot
                    .iter()
                    .filter(|s| s.enabled)
                    .map(|s| format!("output.{}.disable", s.name))
                    .collect();
                self.ks.apply(&off);
            }
            "mirror" => {
                let prim = self
                    .snapshot
                    .iter()
                    .find(|s| s.enabled && s.primary)
                    .or_else(|| self.snapshot.iter().find(|s| s.enabled));
                if let Some(prim) = prim {
                    let prim = prim.name.clone();
                    self.ks
                        .apply(&[format!("output.{out}.enable"), format!("output.{out}.mirror.{prim}")]);
                }
            }
            _ => {
                let layout = self
                    .ks
                    .outputs()
                    .map(|o| plan_layout(&o, &out, &s.position))
                    .unwrap_or_default();
                let mut ops = vec![format!("output.{out}.enable")];
                ops.extend(layout.iter().map(|(n, x, y)| format!("output.{n}.position.{x},{y}")));
                self.ks.apply(&ops);
                if !p.rules.is_empty() {
                    self.load_rules(&out, &p.rules, p.fullscreen);
                }
            }
        }
        Ok(out)
    }

    fn stop(&mut self) {
        if self.script_loaded {
            self.ks.kwin("unloadScript", &[KWIN_SCRIPT]);
            self.script_loaded = false;
        }
        let snap = std::mem::take(&mut self.snapshot);
        let enable: Vec<String> = snap
            .iter()
            .filter(|s| s.enabled)
            .map(|s| format!("output.{}.enable", s.name))
            .collect();
        self.ks.apply(&enable);
        let had = self.proc.is_some() || self.output.is_some();
        self.kill_proc();
        self.ks.kill_stale();
        if !snap.is_empty() {
            for _ in 0..20 {
                match self.ks.outputs() {
                    Ok(o) if o.iter().any(|x| x.str_at("name") == KDE_OUTPUT) => util::sleep_ms(self.poll_ms.min(250)),
                    _ => break,
                }
            }
            let mut ops: Vec<String> = snap
                .iter()
                .filter(|s| s.enabled)
                .map(|s| format!("output.{}.position.{},{}", s.name, s.x, s.y))
                .collect();
            if let Some(p) = snap.iter().find(|s| s.enabled && s.primary) {
                ops.push(format!("output.{}.primary", p.name));
            }
            self.ks.apply(&ops);
        }
        if had {
            crate::info!("tela do Deck removida, monitores restaurados");
        }
        let _ = std::fs::remove_file(restore_file());
        self.output = None;
    }

    fn alive(&mut self) -> bool {
        match self.proc.as_mut() {
            Some(p) => p.try_wait().ok().flatten().is_none(),
            None => true,
        }
    }

    fn recover(&mut self) {
        let Ok(text) = std::fs::read_to_string(restore_file()) else {
            return;
        };
        let snap = snaps_from_json(&Json::parse(&text).unwrap_or_default());
        if snap.is_empty() {
            let _ = std::fs::remove_file(restore_file());
            return;
        }
        crate::warn!("a última sessão não terminou direito; restaurando os monitores");
        self.snapshot = snap;
        self.stop();
    }
}

pub fn render_kwin_script(target: &str, rules: &[String], fullscreen: bool) -> String {
    let rules_json = Json::from(rules.to_vec()).to_compact();
    format!(
        r#"// Gerado pelo DeckScreen: envia janelas para a tela do Steam Deck.
const TARGET = {target};
const RULES = {rules_json};
const FULLSCREEN = {fullscreen};

function targetOutput() {{
    const screens = workspace.screens;
    for (let i = 0; i < screens.length; i++) {{
        if (screens[i].name === TARGET) return screens[i];
    }}
    return null;
}}

function matches(w) {{
    const caption = (w.caption || "").toLowerCase();
    const klass = (w.resourceClass || "").toString().toLowerCase();
    for (const r of RULES) {{
        const needle = r.toLowerCase();
        if (needle && (caption.indexOf(needle) !== -1 || klass === needle)) return true;
    }}
    return false;
}}

function apply(w) {{
    if (!w || !w.normalWindow || !matches(w)) return;
    const out = targetOutput();
    if (!out) return;
    if (w.output !== out) workspace.sendClientToScreen(w, out);
    if (FULLSCREEN && !w.fullScreen) w.fullScreen = true;
}}

function watch(w) {{
    apply(w);
    w.captionChanged.connect(function () {{ apply(w); }});
}}

workspace.windowList().forEach(watch);
workspace.windowAdded.connect(watch);
"#,
        target = Json::from(target).to_compact(),
    )
}

// ============================================================================ Hyprland (experimental)

#[derive(Default)]
pub struct HyprBackend {
    output: Option<String>,
}

fn hyprctl(args: &[&str]) -> (bool, String) {
    let (ok, out) = util::run_text("hyprctl", args);
    // hyprctl prints "ok" on success for keyword/dispatch
    (ok && !out.to_lowercase().contains("error"), out)
}

fn hypr_monitors() -> Vec<Json> {
    util::run_stdout("hyprctl", &["-j", "monitors", "all"])
        .and_then(|s| Json::parse(&s).ok())
        .and_then(|j| j.as_arr().cloned())
        .unwrap_or_default()
}

/// Turn a plain substring into a regex-safe pattern.
fn regex_escape(s: &str) -> String {
    s.chars()
        .map(|c| {
            if "\\.+*?()|[]{}^$".contains(c) {
                format!("\\{c}")
            } else {
                c.to_string()
            }
        })
        .collect()
}

impl Backend for HyprBackend {
    fn name(&self) -> &'static str {
        "hyprland"
    }

    fn start(&mut self, req: &StartReq, s: &Settings, p: &Profile) -> Result<String, String> {
        self.stop();
        let before: Vec<String> = hypr_monitors().iter().map(|m| m.str_at("name")).collect();
        let (ok, out) = hyprctl(&["output", "create", "headless", HYPR_OUTPUT]);
        if !ok {
            return Err(format!("hyprctl output create falhou: {}", out.trim()));
        }
        let mut name = None;
        for _ in 0..30 {
            util::sleep_ms(200);
            let now = hypr_monitors();
            if let Some(m) = now.iter().find(|m| m.str_at("name") == HYPR_OUTPUT) {
                name = Some(m.str_at("name"));
                break;
            }
            if let Some(m) = now.iter().find(|m| !before.contains(&m.str_at("name"))) {
                name = Some(m.str_at("name"));
                break;
            }
        }
        let out = name.ok_or("o monitor headless não apareceu no Hyprland")?;
        self.output = Some(out.clone());
        let mons = hypr_monitors();
        let primary = mons
            .iter()
            .find(|m| m.bool_at("focused", false) && m.str_at("name") != out)
            .or_else(|| mons.iter().find(|m| m.str_at("name") != out))
            .map(|m| m.str_at("name"))
            .unwrap_or_default();
        let place = match s.position.as_str() {
            "left" => "auto-left",
            "above" => "auto-up",
            "below" => "auto-down",
            _ => "auto-right",
        };
        let transform = match s.orientation.as_str() {
            "portrait" => ",transform,1",
            "portrait_flipped" => ",transform,3",
            _ => "",
        };
        let mirror = if s.mode == "mirror" && !primary.is_empty() {
            format!(",mirror,{primary}")
        } else {
            String::new()
        };
        let rule = format!(
            "{out},{}x{}@{},{place},{}{transform}{mirror}",
            req.width, req.height, req.fps, s.scale
        );
        let (ok, msg) = hyprctl(&["keyword", "monitor", &rule]);
        if !ok {
            crate::warn!("hyprctl keyword monitor {rule}: {}", msg.trim());
        }
        if s.mode == "deck_only" {
            for m in mons
                .iter()
                .filter(|m| m.str_at("name") != out && !m.bool_at("disabled", false))
            {
                hyprctl(&["keyword", "monitor", &format!("{},disable", m.str_at("name"))]);
            }
        }
        if s.mode == "extend" {
            for r in &p.rules {
                let pat = format!("title:(.*{}.*)", regex_escape(r));
                for (kw, action) in [
                    ("windowrulev2", format!("monitor {out}")),
                    ("windowrulev2", "fullscreen".to_string()),
                ] {
                    if action == "fullscreen" && !p.fullscreen {
                        continue;
                    }
                    let (ok, _) = hyprctl(&["keyword", kw, &format!("{action},{pat}")]);
                    if !ok {
                        // newer Hyprland: unified "windowrule" syntax
                        hyprctl(&["keyword", "windowrule", &format!("{action},{pat}")]);
                    }
                }
            }
        }
        Ok(out)
    }

    fn stop(&mut self) {
        if let Some(out) = self.output.take() {
            hyprctl(&["output", "remove", &out]);
            // restores the user's monitor and window rules
            hyprctl(&["reload"]);
            crate::info!("monitor headless removido do Hyprland");
        }
    }

    fn alive(&mut self) -> bool {
        match &self.output {
            Some(o) => hypr_monitors().iter().any(|m| &m.str_at("name") == o),
            None => true,
        }
    }
}

// ============================================================================ mirror only

pub struct MirrorBackend;

impl Backend for MirrorBackend {
    fn name(&self) -> &'static str {
        "mirror"
    }
    fn start(&mut self, _: &StartReq, _: &Settings, _: &Profile) -> Result<String, String> {
        Ok(String::new())
    }
    fn stop(&mut self) {}
    fn alive(&mut self) -> bool {
        true
    }
}

// ============================================================================ tests

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    pub fn out(name: &str, x: i64, y: i64, w: i64, h: i64, en: bool, prio: i64) -> Json {
        Json::parse(&format!(
            r#"{{"name":"{name}","enabled":{en},"connected":true,"priority":{prio},"pos":{{"x":{x},"y":{y}}},
            "size":{{"width":{w},"height":{h}}},"scale":1,"rotation":1,"currentModeId":"m1",
            "modes":[{{"id":"m1","size":{{"width":{w},"height":{h}}},"refreshRate":60}}]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn geometry() {
        let mut o = out("A", 0, 0, 2560, 1440, true, 1);
        o.set("scale", 1.25);
        assert_eq!(logical_size(&o), (2048, 1152));
        let mut r = out("B", 0, 0, 1920, 1080, true, 1);
        r.set("rotation", 2);
        assert_eq!(logical_size(&r), (1080, 1920));

        let outs = vec![
            out("DP-1", 0, 0, 1920, 1080, true, 1),
            out("HDMI-A-1", 1920, 0, 1920, 1080, true, 2),
            out(KDE_OUTPUT, 9, 9, 1280, 800, true, 0),
        ];
        let l = plan_layout(&outs, KDE_OUTPUT, "right");
        assert!(l.contains(&(KDE_OUTPUT.to_string(), 3840, 140)));
        let outs = vec![
            out("DP-1", 0, 0, 1920, 1080, true, 1),
            out(KDE_OUTPUT, 0, 0, 1280, 800, true, 0),
        ];
        let l = plan_layout(&outs, KDE_OUTPUT, "left");
        assert!(l.contains(&(KDE_OUTPUT.to_string(), 0, 140)) && l.contains(&("DP-1".to_string(), 1280, 0)));
        let l = plan_layout(&outs, KDE_OUTPUT, "above");
        assert!(l.contains(&(KDE_OUTPUT.to_string(), 320, 0)) && l.contains(&("DP-1".to_string(), 0, 800)));
        assert!(plan_layout(&outs, KDE_OUTPUT, "below").contains(&(KDE_OUTPUT.to_string(), 320, 1080)));
    }

    #[test]
    fn kwin_script_escapes() {
        let js = render_kwin_script("Virtual-\"x\"", &["a\"b".to_string()], true);
        assert!(js.contains(r#"const TARGET = "Virtual-\"x\"";"#));
        assert!(js.contains(r#"["a\"b"]"#));
        assert!(js.contains("const FULLSCREEN = true;"));
    }

    /// Fake KWin/kscreen: the virtual output exists while the fake krfb "runs".
    #[derive(Clone)]
    pub struct FakeKs {
        pub outs: Arc<Mutex<Vec<Json>>>,
        pub applied: Arc<Mutex<Vec<String>>>,
        pub kwin_calls: Arc<Mutex<Vec<String>>>,
        pub vm_alive: Arc<Mutex<bool>>,
        pub appear: bool,
    }

    impl FakeKs {
        pub fn new() -> Self {
            FakeKs {
                outs: Arc::new(Mutex::new(vec![
                    out("DP-1", 0, 0, 2560, 1440, true, 1),
                    out("HDMI-A-1", 2560, 0, 1920, 1080, true, 2),
                ])),
                applied: Arc::new(Mutex::new(vec![])),
                kwin_calls: Arc::new(Mutex::new(vec![])),
                vm_alive: Arc::new(Mutex::new(false)),
                appear: true,
            }
        }
    }

    impl KScreen for FakeKs {
        fn outputs(&self) -> Result<Vec<Json>, String> {
            let mut v = self.outs.lock().unwrap().clone();
            if *self.vm_alive.lock().unwrap() && self.appear {
                v.push(out(KDE_OUTPUT, 0, 0, 1280, 800, true, 0));
            }
            Ok(v)
        }
        fn apply(&self, ops: &[String]) -> bool {
            let mut outs = self.outs.lock().unwrap();
            for op in ops {
                self.applied.lock().unwrap().push(op.clone());
                let parts: Vec<&str> = op.split('.').collect();
                for o in outs.iter_mut() {
                    if o.str_at("name") == parts[1] {
                        match parts[2] {
                            "disable" => o.set("enabled", false),
                            "enable" => o.set("enabled", true),
                            _ => {}
                        }
                    }
                }
            }
            true
        }
        fn spawn_krfb(&self, _w: u32, _h: u32) -> Result<Child, String> {
            *self.vm_alive.lock().unwrap() = true;
            Command::new("sleep").arg("30").spawn().map_err(|e| e.to_string())
        }
        fn kwin(&self, method: &str, _args: &[&str]) -> bool {
            self.kwin_calls.lock().unwrap().push(method.to_string());
            true
        }
        fn kill_stale(&self) {}
    }

    fn req() -> StartReq {
        StartReq {
            width: 1280,
            height: 800,
            fps: 60,
            client: "Deck".into(),
        }
    }

    fn settings(mode: &str) -> Settings {
        let mut s = Settings::default();
        s.mode = mode.into();
        s
    }

    #[test]
    fn kde_extend_start_stop() {
        util::test_env();
        let ks = FakeKs::new();
        let mut b = KdeBackend::new(Box::new(ks.clone()));
        b.poll_ms = 1;
        assert_eq!(
            b.start(&req(), &settings("extend"), &Profile::default()).unwrap(),
            KDE_OUTPUT
        );
        let applied = ks.applied.lock().unwrap().clone();
        assert!(applied.contains(&format!("output.{KDE_OUTPUT}.addCustomMode.1280.800.60000.full")));
        assert!(applied.contains(&format!("output.{KDE_OUTPUT}.position.4480,320")));
        assert!(!applied.iter().any(|a| a.ends_with(".disable")));
        assert!(b.alive());
        *ks.vm_alive.lock().unwrap() = false;
        b.stop();
        let applied = ks.applied.lock().unwrap().clone();
        assert!(applied.contains(&"output.DP-1.position.0,0".to_string()));
        assert_eq!(applied.last().unwrap(), "output.DP-1.primary");
    }

    #[test]
    fn kde_deck_only_restores() {
        util::test_env();
        let ks = FakeKs::new();
        let mut b = KdeBackend::new(Box::new(ks.clone()));
        b.poll_ms = 1;
        b.start(&req(), &settings("deck_only"), &Profile::default()).unwrap();
        assert!(ks.outs.lock().unwrap().iter().all(|o| !o.bool_at("enabled", true)));
        b.stop();
        assert!(ks.outs.lock().unwrap().iter().all(|o| o.bool_at("enabled", false)));
    }

    #[test]
    fn kde_mirror_rotation_and_rules() {
        util::test_env();
        let ks = FakeKs::new();
        let mut b = KdeBackend::new(Box::new(ks.clone()));
        b.poll_ms = 1;
        let mut s = settings("mirror");
        s.orientation = "portrait".into();
        b.start(&req(), &s, &Profile::default()).unwrap();
        let applied = ks.applied.lock().unwrap().clone();
        assert!(applied.contains(&format!("output.{KDE_OUTPUT}.mirror.DP-1")));
        assert!(applied.contains(&format!("output.{KDE_OUTPUT}.rotation.right")));
        b.stop();

        let p = Profile {
            rules: vec!["GamePad View".into()],
            fullscreen: true,
            command: None,
        };
        b.start(&req(), &settings("extend"), &p).unwrap();
        assert!(ks.kwin_calls.lock().unwrap().contains(&"loadScript".to_string()));
        b.stop();
        assert_eq!(ks.kwin_calls.lock().unwrap().last().unwrap(), "unloadScript");
    }

    #[test]
    fn kde_fails_cleanly() {
        util::test_env();
        let mut ks = FakeKs::new();
        ks.appear = false;
        let mut b = KdeBackend::new(Box::new(ks.clone()));
        b.poll_ms = 1;
        let e = b
            .start(&req(), &settings("deck_only"), &Profile::default())
            .unwrap_err();
        assert!(e.contains("não apareceu"));
        assert!(ks.outs.lock().unwrap().iter().all(|o| o.bool_at("enabled", false)));
    }

    #[test]
    fn regex() {
        assert_eq!(regex_escape("a.b (c)"), "a\\.b \\(c\\)");
    }
}
