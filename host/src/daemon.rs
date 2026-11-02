//! The long-running user service: LAN API for the Deck (signed requests),
//! local control socket (Sunshine hooks, CLI, KDE widget), UDP discovery and
//! a watchdog that restores the monitors if anything goes wrong.

use crate::config::{resolve_profile, Config};
use crate::crypto;
use crate::display::{self, Backend, Desktop, StartReq};
use crate::http::{self, Handler, Request, Response};
use crate::json::Json;
use crate::net;
use crate::obj;
use crate::pairing::Pairing;
use crate::sunshine::{self, SunshineApi};
use crate::util;
use crate::VERSION;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

pub const API_PORT: u16 = 47555;
pub const DISCOVERY_PORT: u16 = 47556;
pub const LOCAL_PORT: u16 = 47557;
pub const DISCOVERY_MAGIC: &[u8] = b"DECKSCREEN_DISCOVER_V1";
const MAX_SKEW: i64 = 300;

// ============================================================================ the Deck screen session

pub struct Screen {
    backend: Box<dyn Backend>,
    pub output: Option<String>,
    pub session: Option<Json>,
    pub last_error: Option<String>,
    app: Option<Child>,
}

impl Screen {
    pub fn new(backend: Box<dyn Backend>) -> Screen {
        Screen {
            backend,
            output: None,
            session: None,
            last_error: None,
            app: None,
        }
    }

    pub fn active(&self) -> bool {
        self.session.is_some()
    }

    pub fn start(&mut self, req: &StartReq, cfg: &Config) -> bool {
        if self.session.is_some() {
            self.stop(true);
        }
        let s = cfg.settings();
        let p = resolve_profile(&s);
        self.last_error = None;
        match self.backend.start(req, &s, &p) {
            Ok(out) => {
                self.output = Some(out.clone()).filter(|o| !o.is_empty());
                self.session = Some(obj! {
                    "client" => req.client.clone(), "width" => req.width, "height" => req.height, "fps" => req.fps,
                    "mode" => if self.output.is_some() { s.mode.clone() } else { "mirror".to_string() },
                    "profile" => s.profile.clone(), "started" => util::now_secs(),
                });
                if s.open_app && s.mode != "mirror" {
                    if let Some(cmd) = p.command.as_ref() {
                        self.launch_app(cmd);
                    } else if s.profile == "cemu" {
                        crate::warn!("perfil Cemu: não encontrei o Cemu instalado (cemu, flatpak info.cemu.Cemu ou ~/Applications)");
                    }
                }
                true
            }
            Err(e) => {
                crate::error!("{e}");
                self.last_error = Some(e);
                false
            }
        }
    }

    fn launch_app(&mut self, cmd: &[String]) {
        if let Some(c) = self.app.as_mut() {
            if c.try_wait().ok().flatten().is_none() {
                return; // still running from a previous connection
            }
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(util::data_dir().join("app.log"));
        let (out, err) = match log {
            Ok(f) => (Stdio::from(f.try_clone().unwrap()), Stdio::from(f)),
            Err(_) => (Stdio::null(), Stdio::null()),
        };
        use std::os::unix::process::CommandExt;
        match Command::new(&cmd[0])
            .args(&cmd[1..])
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0)
            .spawn()
        {
            Ok(c) => {
                crate::info!("app do perfil aberto: {:?} (pid {})", cmd, c.id());
                self.app = Some(c);
            }
            Err(e) => crate::warn!("não consegui abrir {:?}: {e}", cmd),
        }
    }

    pub fn stop(&mut self, close_app: bool) {
        if close_app {
            if let Some(mut c) = self.app.take() {
                if c.try_wait().ok().flatten().is_none() {
                    let _ = util::run("kill", &["-TERM", &format!("-{}", c.id())]);
                    util::sleep_ms(300);
                    let _ = c.try_wait();
                }
            }
        }
        self.backend.stop();
        self.output = None;
        self.session = None;
    }

    pub fn watchdog(&mut self) {
        if self.session.is_some() && !self.backend.alive() {
            crate::warn!("a tela virtual caiu sozinha; restaurando os monitores");
            self.stop(false);
            self.last_error = Some("a tela virtual caiu; monitores restaurados".into());
        }
        if let Some(c) = self.app.as_mut() {
            if c.try_wait().ok().flatten().is_some() {
                self.app = None;
            }
        }
    }

    pub fn status(&self) -> Json {
        obj! {
            "active" => self.active(),
            "output" => self.output.clone(),
            "session" => self.session.clone().unwrap_or(Json::Null),
            "last_error" => self.last_error.clone(),
            "backend" => self.backend.name(),
        }
    }
}

// ============================================================================ shared state

pub struct State {
    pub cfg: Arc<Mutex<Config>>,
    pub screen: Mutex<Screen>,
    pub pairing: Arc<Pairing>,
    pub api: Arc<dyn SunshineApi>,
    pub desktop: Desktop,
    nonces: Mutex<HashMap<String, u64>>,
    cache: Mutex<HashMap<String, (u64, Json)>>,
    /// GNOME: Sunshine must switch between virtual-monitor and screen capture
    /// (0 = nothing pending, 1 = mirror, 2 = virtual). Applied when not streaming.
    pending_sunshine: std::sync::atomic::AtomicU8,
}

impl State {
    pub fn new(
        cfg: Arc<Mutex<Config>>,
        backend: Box<dyn Backend>,
        api: Arc<dyn SunshineApi>,
        desktop: Desktop,
        notify: bool,
    ) -> Arc<State> {
        let pairing = Pairing::new(cfg.clone(), api.clone(), notify);
        Arc::new(State {
            cfg,
            screen: Mutex::new(Screen::new(backend)),
            pairing,
            api,
            desktop,
            nonces: Mutex::new(HashMap::new()),
            cache: Mutex::new(HashMap::new()),
            pending_sunshine: std::sync::atomic::AtomicU8::new(0),
        })
    }

    fn cached(&self, key: &str, ttl: u64, f: impl FnOnce() -> Json) -> Json {
        let now = util::now_secs();
        if let Some((t, v)) = self.cache.lock().unwrap().get(key) {
            if now.saturating_sub(*t) < ttl {
                return v.clone();
            }
        }
        let v = f();
        self.cache.lock().unwrap().insert(key.to_string(), (now, v.clone()));
        v
    }

    pub fn diagnostics(&self) -> Json {
        let api = self.api.clone();
        let desktop = self.desktop;
        self.cached("diag", 15, move || {
            obj! {
                "desktop" => desktop.id(),
                "capability" => display::capability(desktop),
                "wayland" => display::is_wayland(),
                "krfb" => util::which("krfb-virtualmonitor").is_some(),
                "gnome_virtual" => display::gnome_virtual(),
                "kscreen" => util::which("kscreen-doctor").is_some(),
                "sunshine_api" => sunshine::reachable(api.as_ref()),
            }
        })
    }

    pub fn status_json(&self, peer_ip: Option<&str>) -> Json {
        let cfg_settings = self.cfg.lock().unwrap().settings();
        let mut st = obj! {
            "display" => self.screen.lock().unwrap().status(),
            "settings" => cfg_settings.to_json(),
            "diagnostics" => self.diagnostics(),
            "name" => util::hostname(),
            "version" => VERSION,
            "desktop" => self.desktop.id(),
            "capability" => display::capability(self.desktop),
        };
        if let Some(ip) = peer_ip {
            let ip = ip.to_string();
            st.set(
                "network",
                self.cached(&format!("net-{ip}"), 60, || net::info_for_peer(&ip)),
            );
        }
        st
    }

    /// Verify an HMAC-signed request from a paired Deck. Returns the device id.
    pub fn verify(&self, r: &Request) -> Result<String, Response> {
        let deny = |m: &str| Err(Response::error(401, m));
        let (Some(dev), Some(ts), Some(nonce), Some(sig)) = (
            r.header("x-ds-device"),
            r.header("x-ds-time"),
            r.header("x-ds-nonce"),
            r.header("x-ds-sig"),
        ) else {
            return deny("não pareado");
        };
        let Some(key) = self.cfg.lock().unwrap().device_key(dev) else {
            return deny("não pareado");
        };
        let Ok(t) = ts.parse::<i64>() else {
            return deny("assinatura inválida");
        };
        if (t - util::now_secs() as i64).abs() > MAX_SKEW {
            return deny("relógio do Deck e do PC muito diferentes");
        }
        if nonce.len() < 16 || nonce.len() > 64 || !nonce.chars().all(|c| c.is_ascii_hexdigit()) {
            return deny("assinatura inválida");
        }
        let msg = format!(
            "{}\n{}\n{}\n{}\n{}",
            r.method,
            r.target,
            ts,
            nonce,
            crypto::hex(&crypto::sha256(&r.body))
        );
        let expected = crypto::hex(&crypto::hmac_sha256(&key, msg.as_bytes()));
        if !crypto::ct_eq(expected.as_bytes(), sig.to_ascii_lowercase().as_bytes()) {
            return deny("assinatura inválida");
        }
        let mut nonces = self.nonces.lock().unwrap();
        let now = util::now_secs();
        nonces.retain(|_, t| now.saturating_sub(*t) < (MAX_SKEW as u64) * 2);
        let k = format!("{dev}:{nonce}");
        if nonces.contains_key(&k) {
            return deny("requisição repetida");
        }
        nonces.insert(k, now);
        Ok(dev.to_string())
    }

    fn start_screen(&self, b: &Json) -> (bool, Json) {
        let req = StartReq {
            width: b.i64_at("width", 1280).clamp(320, 7680) as u32,
            height: b.i64_at("height", 800).clamp(240, 4320) as u32,
            fps: b.i64_at("fps", 60).clamp(24, 240) as u32,
            client: b.str_at("client"),
        };
        let cfg = self.cfg.lock().unwrap();
        let mut sc = self.screen.lock().unwrap();
        let ok = sc.start(&req, &cfg);
        (ok, sc.status())
    }

    fn stop_screen(&self) -> Json {
        let close = self.cfg.lock().unwrap().settings().close_app;
        let status = {
            let mut sc = self.screen.lock().unwrap();
            sc.stop(close);
            sc.status()
        };
        self.apply_pending_sunshine();
        status
    }

    /// Settings changed. On GNOME, switching to/from "mirror" changes how Sunshine
    /// captures (virtual monitor vs. real screen), which needs a Sunshine restart.
    pub fn settings_changed(&self, old: &crate::config::Settings, new: &crate::config::Settings) {
        let was = old.mode == "mirror";
        let now = new.mode == "mirror";
        if self.desktop != Desktop::Gnome || !display::gnome_virtual() || was == now {
            return;
        }
        self.pending_sunshine
            .store(if now { 1 } else { 2 }, std::sync::atomic::Ordering::SeqCst);
        if !self.screen.lock().unwrap().active() {
            self.apply_pending_sunshine();
        } else {
            crate::info!("GNOME: o Sunshine será reconfigurado quando a sessão atual terminar");
        }
    }

    fn apply_pending_sunshine(&self) {
        let p = self.pending_sunshine.swap(0, std::sync::atomic::Ordering::SeqCst);
        if p == 0 {
            return;
        }
        let desktop = self.desktop;
        std::thread::spawn(move || {
            let exe = std::env::current_exe()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            match sunshine::configure(desktop, &exe, p == 1) {
                Ok(notes) => notes.iter().for_each(|n| crate::info!("{n}")),
                Err(e) => crate::warn!("não reconfigurei o Sunshine: {e}"),
            }
            if let Some(s) = sunshine::service_name() {
                util::run("systemctl", &["--user", "restart", &s]);
                crate::info!(
                    "Sunshine reiniciado ({})",
                    if p == 1 { "espelhar" } else { "monitor virtual" }
                );
            }
        });
    }

    fn pending_json(&self) -> Json {
        let p: Vec<Json> = self
            .pairing
            .pending()
            .into_iter()
            .map(|r| obj! {"id" => r.id, "device_name" => r.device_name, "ip" => r.ip, "pin" => r.pin})
            .collect();
        obj! {"pending" => p}
    }
}

// ============================================================================ routes

pub fn lan_handler(st: Arc<State>) -> Handler {
    Arc::new(move |r: &Request| {
        let m = r.method.as_str();
        let p = r.path.as_str();
        match (m, p) {
            ("GET", "/api/hello") => {
                let paired = st.verify(r).is_ok();
                Response::json(
                    200,
                    &obj! {
                        "app" => "deckscreen", "version" => VERSION, "name" => util::hostname(),
                        "auth" => "hmac-v1", "paired" => paired,
                        "desktop" => st.desktop.id(), "capability" => display::capability(st.desktop),
                    },
                )
            }
            ("POST", "/api/pair/request") => {
                let b = match r.json() {
                    Ok(b) => b,
                    Err(e) => return Response::error(400, &e),
                };
                match st.pairing.create(
                    &b.str_at("device_id"),
                    &b.str_at("device_name"),
                    &b.str_at("pin"),
                    &r.peer_ip(),
                ) {
                    Ok(req) => Response::json(200, &obj! {"request_id" => req.id}),
                    Err((c, e)) => Response::error(c, &e),
                }
            }
            ("GET", "/api/pair/status") => match st.pairing.status(r.query("id").unwrap_or(""), &r.peer_ip()) {
                Some(j) => Response::json(200, &j),
                None => Response::error(404, "pedido não encontrado"),
            },
            _ if p.starts_with("/api/") => {
                let dev = match st.verify(r) {
                    Ok(d) => d,
                    Err(resp) => return resp,
                };
                match (m, p) {
                    ("GET", "/api/ping") => Response::json(200, &obj! {"pong" => util::now_secs()}),
                    ("GET", "/api/status") => Response::json(200, &st.status_json(Some(&r.peer_ip()))),
                    ("GET", "/api/settings") => Response::json(200, &st.cfg.lock().unwrap().settings().to_json()),
                    ("PUT", "/api/settings") | ("POST", "/api/settings") => match r.json() {
                        Ok(b) => {
                            let (old, new) = {
                                let mut c = st.cfg.lock().unwrap();
                                let old = c.settings();
                                (old, c.update_settings(&b))
                            };
                            st.settings_changed(&old, &new);
                            Response::json(200, &new.to_json())
                        }
                        Err(e) => Response::error(400, &e),
                    },
                    ("POST", "/api/stop") => {
                        sunshine::close_app(st.api.as_ref());
                        Response::json(200, &st.stop_screen())
                    }
                    ("POST", "/api/unpair") => {
                        st.cfg.lock().unwrap().forget(&dev);
                        Response::json(200, &obj! {"ok" => true})
                    }
                    ("GET", "/api/doctor") => Response::json(200, &crate::doctor::checks_json(st.desktop)),
                    _ => Response::error(404, "não encontrado"),
                }
            }
            _ => Response::error(404, "não encontrado"),
        }
    })
}

pub fn local_handler(st: Arc<State>) -> Handler {
    Arc::new(move |r: &Request| {
        // Browsers always send Origin on cross-site requests: never accept them here.
        if r.header("origin").is_some() {
            return Response::error(403, "proibido");
        }
        let m = r.method.as_str();
        let p = r.path.as_str();
        if let Some(rest) = p.strip_prefix("/local/") {
            let secret = st.cfg.lock().unwrap().local_secret();
            if !crypto::ct_eq(
                r.header("x-deckscreen-local").unwrap_or("").as_bytes(),
                secret.as_bytes(),
            ) {
                return Response::error(403, "proibido");
            }
            return match (m, rest) {
                ("POST", "start") => {
                    let b = r.json().unwrap_or_default();
                    let (ok, s) = st.start_screen(&b);
                    Response::json(if ok { 200 } else { 500 }, &s)
                }
                ("POST", "stop") => Response::json(200, &st.stop_screen()),
                ("GET", "pending") => Response::json(200, &st.pending_json()),
                ("POST", "decide") => {
                    let b = r.json().unwrap_or_default();
                    let ok = st.pairing.decide(&b.str_at("id"), b.bool_at("accept", false));
                    Response::json(if ok { 200 } else { 404 }, &obj! {"ok" => ok})
                }
                ("GET", "status") => {
                    let mut s = st.status_json(None);
                    let devs: Vec<Json> = st
                        .cfg
                        .lock()
                        .unwrap()
                        .devices()
                        .into_iter()
                        .map(|(id, n, _)| obj! {"id" => id, "name" => n})
                        .collect();
                    s.set("devices", devs);
                    Response::json(200, &s)
                }
                _ => Response::error(404, "não encontrado"),
            };
        }
        if let Some(rest) = p.strip_prefix("/ui/") {
            // KDE widget. A custom header forces a CORS preflight that we never answer.
            if r.header("x-deckscreen-ui") != Some("1") {
                return Response::error(403, "proibido");
            }
            return match (m, rest) {
                ("GET", "status") => {
                    let mut s = st.status_json(None);
                    s.set("pending", st.pending_json().get("pending").cloned().unwrap_or_default());
                    Response::json(200, &s)
                }
                ("POST", "stop") => {
                    sunshine::close_app(st.api.as_ref());
                    Response::json(200, &st.stop_screen())
                }
                ("POST", "decide") => {
                    let b = r.json().unwrap_or_default();
                    let ok = st.pairing.decide(&b.str_at("id"), b.bool_at("accept", false));
                    Response::json(if ok { 200 } else { 404 }, &obj! {"ok" => ok})
                }
                _ => Response::error(404, "não encontrado"),
            };
        }
        Response::error(404, "não encontrado")
    })
}

pub fn discovery_loop(sock: UdpSocket, desktop: Desktop) {
    let reply = obj! {
        "app" => "deckscreen", "name" => util::hostname(), "port" => API_PORT, "version" => VERSION,
        "auth" => "hmac-v1", "capability" => display::capability(desktop),
    }
    .to_compact();
    let mut buf = [0u8; 256];
    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, addr)) => {
                if buf[..n].trim_ascii() == DISCOVERY_MAGIC {
                    let _ = sock.send_to(reply.as_bytes(), addr);
                }
            }
            Err(_) => util::sleep_ms(200),
        }
    }
}

pub fn run(verbose: bool) -> i32 {
    util::init_log_file();
    let desktop = display::detect_desktop();
    let cfg = Arc::new(Mutex::new(Config::load_default()));
    let api: Arc<dyn SunshineApi> = Arc::new(sunshine::api_from_config(&cfg.lock().unwrap()));
    let mut backend = display::backend_for(desktop);
    backend.recover();
    let st = State::new(cfg, backend, api, desktop, true);

    if let Err(e) = http::serve(&format!("0.0.0.0:{API_PORT}"), lan_handler(st.clone())) {
        crate::error!("não consegui abrir a porta {API_PORT}: {e}");
        return 1;
    }
    if let Err(e) = http::serve(&format!("127.0.0.1:{LOCAL_PORT}"), local_handler(st.clone())) {
        crate::error!("não consegui abrir a porta local {LOCAL_PORT}: {e}");
        return 1;
    }
    match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
        Ok(sock) => {
            std::thread::spawn(move || discovery_loop(sock, desktop));
        }
        Err(e) => crate::warn!("busca automática desativada (udp/{DISCOVERY_PORT}): {e}"),
    }
    crate::info!(
        "DeckScreen {VERSION} no ar: API :{API_PORT}, local 127.0.0.1:{LOCAL_PORT}, busca udp/{DISCOVERY_PORT} · {} ({})",
        desktop.label(),
        display::capability(desktop)
    );
    if verbose {
        crate::info!("modo detalhado");
    }
    loop {
        util::sleep_ms(3000);
        st.screen.lock().unwrap().watchdog();
    }
}

// ============================================================================ local client (CLI / hooks)

pub fn local_call(method: &str, path: &str, body: Option<&Json>, timeout_s: u64) -> Result<(u16, Json), String> {
    let secret = Config::load_default().local_secret();
    let b = body.map(|b| b.to_compact());
    let (code, text) = http::request(
        method,
        &format!("127.0.0.1:{LOCAL_PORT}"),
        path,
        &[("X-DeckScreen-Local", &secret)],
        b.as_deref(),
        std::time::Duration::from_secs(timeout_s),
    )?;
    Ok((code, Json::parse(&text).unwrap_or_default()))
}

// ============================================================================ tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::tests::FakeKs;
    use crate::display::KdeBackend;
    use std::time::Duration;

    struct OkApi;
    impl SunshineApi for OkApi {
        fn call(&self, method: &str, _p: &str, _b: Option<&Json>, _t: u32) -> Result<(u16, Option<Json>), String> {
            Ok(if method == "GET" {
                (404, None)
            } else {
                (200, Some(obj! {"status" => true}))
            })
        }
    }

    fn sign(
        key_hex: &str,
        dev: &str,
        method: &str,
        target: &str,
        body: &str,
        ts: u64,
        nonce: &str,
    ) -> Vec<(String, String)> {
        let key = crypto::unhex(key_hex).unwrap();
        let msg = format!(
            "{method}\n{target}\n{ts}\n{nonce}\n{}",
            crypto::hex(&crypto::sha256(body.as_bytes()))
        );
        vec![
            ("X-DS-Device".into(), dev.into()),
            ("X-DS-Time".into(), ts.to_string()),
            ("X-DS-Nonce".into(), nonce.into()),
            (
                "X-DS-Sig".into(),
                crypto::hex(&crypto::hmac_sha256(&key, msg.as_bytes())),
            ),
        ]
    }

    fn call(addr: &str, method: &str, target: &str, headers: &[(String, String)], body: Option<&str>) -> (u16, Json) {
        let h: Vec<(&str, &str)> = headers.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let (c, t) = http::request(method, addr, target, &h, body, Duration::from_secs(5)).unwrap();
        (c, Json::parse(&t).unwrap_or_default())
    }

    #[test]
    fn full_api_flow() {
        util::test_env();
        let path = std::env::temp_dir().join(format!("ds-api-{}.json", crypto::token(4)));
        let cfg = Arc::new(Mutex::new(Config::load(path)));
        let ks = FakeKs::new();
        let mut be = KdeBackend::new(Box::new(ks.clone()));
        be.poll_ms = 1;
        let st = State::new(cfg.clone(), Box::new(be), Arc::new(OkApi), Desktop::Kde, false);
        let lan = http::serve("127.0.0.1:0", lan_handler(st.clone())).unwrap().to_string();
        let local = http::serve("127.0.0.1:0", local_handler(st.clone()))
            .unwrap()
            .to_string();

        let (c, hello) = call(&lan, "GET", "/api/hello", &[], None);
        assert_eq!((c, hello.bool_at("paired", true)), (200, false));
        assert_eq!(call(&lan, "GET", "/api/status", &[], None).0, 401);

        let (_, r) = call(
            &lan,
            "POST",
            "/api/pair/request",
            &[],
            Some(r#"{"device_id":"deck1","device_name":"Steam Deck","pin":"4321"}"#),
        );
        let rid = r.str_at("request_id");
        assert_eq!(
            call(&lan, "GET", &format!("/api/pair/status?id={rid}"), &[], None)
                .1
                .str_at("state"),
            "pending"
        );

        // local control needs the secret; browsers (Origin) are always refused
        assert_eq!(call(&local, "GET", "/local/pending", &[], None).0, 403);
        let secret = vec![("X-DeckScreen-Local".to_string(), cfg.lock().unwrap().local_secret())];
        let mut with_origin = secret.clone();
        with_origin.push(("Origin".into(), "http://evil.example".into()));
        assert_eq!(call(&local, "GET", "/local/pending", &with_origin, None).0, 403);
        let (_, pend) = call(&local, "GET", "/local/pending", &secret, None);
        assert_eq!(pend.get("pending").unwrap().as_arr().unwrap()[0].str_at("pin"), "4321");
        assert_eq!(
            call(
                &local,
                "POST",
                "/local/decide",
                &secret,
                Some(&format!(r#"{{"id":"{rid}","accept":true}}"#))
            )
            .0,
            200
        );

        let (_, s) = call(&lan, "GET", &format!("/api/pair/status?id={rid}"), &[], None);
        assert_eq!(s.str_at("state"), "approved");
        let key = s.str_at("key");
        assert_eq!(key.len(), 64);

        let now = util::now_secs();
        let h = sign(
            &key,
            "deck1",
            "GET",
            "/api/status",
            "",
            now,
            "00112233445566778899aabbccddeeff",
        );
        let (c, status) = call(&lan, "GET", "/api/status", &h, None);
        assert_eq!(c, 200, "{status:?}");
        assert_eq!(
            status.str_at("capability"),
            if display::is_wayland() { "virtual" } else { "mirror" }
        );
        assert!(status.get("network").is_some());
        // replay is refused
        assert_eq!(call(&lan, "GET", "/api/status", &h, None).0, 401);
        // tampered body is refused
        let body = r#"{"mode":"deck_only","profile":"cemu"}"#;
        let h = sign(
            &key,
            "deck1",
            "PUT",
            "/api/settings",
            "{}",
            now,
            "aa112233445566778899aabbccddeeff",
        );
        assert_eq!(call(&lan, "PUT", "/api/settings", &h, Some(body)).0, 401);
        let h = sign(
            &key,
            "deck1",
            "PUT",
            "/api/settings",
            body,
            now,
            "bb112233445566778899aabbccddeeff",
        );
        let (c, s) = call(&lan, "PUT", "/api/settings", &h, Some(body));
        assert_eq!(
            (c, s.str_at("mode"), s.str_at("profile")),
            (200, "deck_only".to_string(), "cemu".to_string())
        );
        // stale timestamp is refused
        let h = sign(
            &key,
            "deck1",
            "GET",
            "/api/ping",
            "",
            now - 3600,
            "cc112233445566778899aabbccddeeff",
        );
        assert_eq!(call(&lan, "GET", "/api/ping", &h, None).0, 401);

        // Sunshine hook starts/stops the screen
        let (c, res) = call(
            &local,
            "POST",
            "/local/start",
            &secret,
            Some(r#"{"width":1280,"height":800,"fps":60,"client":"Deck"}"#),
        );
        assert_eq!((c, res.bool_at("active", false)), (200, true));
        assert!(
            ks.outs.lock().unwrap().iter().all(|o| !o.bool_at("enabled", true)),
            "deck_only disabled monitors"
        );
        // KDE widget
        assert_eq!(call(&local, "GET", "/ui/status", &[], None).0, 403);
        let ui = vec![("X-DeckScreen-UI".to_string(), "1".to_string())];
        let (c, u) = call(&local, "GET", "/ui/status", &ui, None);
        assert_eq!((c, u.get("display").unwrap().bool_at("active", false)), (200, true));
        *ks.vm_alive.lock().unwrap() = false;
        let h = sign(
            &key,
            "deck1",
            "POST",
            "/api/stop",
            "{}",
            now,
            "dd112233445566778899aabbccddeeff",
        );
        let (c, res) = call(&lan, "POST", "/api/stop", &h, Some("{}"));
        assert_eq!((c, res.bool_at("active", true)), (200, false));
        assert!(
            ks.outs.lock().unwrap().iter().all(|o| o.bool_at("enabled", false)),
            "monitors restored"
        );

        // unpair
        let h = sign(
            &key,
            "deck1",
            "POST",
            "/api/unpair",
            "{}",
            now,
            "ee112233445566778899aabbccddeeff",
        );
        assert_eq!(call(&lan, "POST", "/api/unpair", &h, Some("{}")).0, 200);
        let h = sign(
            &key,
            "deck1",
            "GET",
            "/api/ping",
            "",
            now,
            "ff112233445566778899aabbccddeeff",
        );
        assert_eq!(call(&lan, "GET", "/api/ping", &h, None).0, 401);
    }

    #[test]
    fn discovery() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        std::thread::spawn(move || discovery_loop(sock, Desktop::Kde));
        let c = UdpSocket::bind("127.0.0.1:0").unwrap();
        c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        c.send_to(DISCOVERY_MAGIC, addr).unwrap();
        let mut buf = [0u8; 512];
        let (n, _) = c.recv_from(&mut buf).unwrap();
        let j = Json::parse(std::str::from_utf8(&buf[..n]).unwrap()).unwrap();
        assert_eq!(
            (j.str_at("app").as_str(), j.i64_at("port", 0)),
            ("deckscreen", API_PORT as i64)
        );
    }
}
