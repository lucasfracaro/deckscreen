//! Sunshine integration: local web API (through curl, since it is HTTPS-only)
//! and wiring of sunshine.conf / apps.json.

use crate::display::{Desktop, HYPR_OUTPUT, KDE_OUTPUT};
use crate::json::Json;
use crate::util;
use std::io::Write;
use std::process::{Command, Stdio};

pub const APP_NAME: &str = "DeckScreen";

/// Abstracts the HTTP call so the pairing logic can be tested.
pub trait SunshineApi: Send + Sync {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Json>,
        timeout_s: u32,
    ) -> Result<(u16, Option<Json>), String>;
}

pub struct CurlApi {
    pub url: String,
    pub user: String,
    pub password: String,
}

impl SunshineApi for CurlApi {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Json>,
        timeout_s: u32,
    ) -> Result<(u16, Option<Json>), String> {
        // Credentials and body go through stdin (curl --config -), never argv,
        // so they don't show up in `ps`. The connection never leaves 127.0.0.1;
        // Sunshine's certificate is self-signed, hence --insecure.
        let mut cfg = String::new();
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        cfg.push_str(&format!(
            "url = \"{}{}\"\n",
            esc(self.url.trim_end_matches('/')),
            esc(path)
        ));
        cfg.push_str(&format!("request = \"{method}\"\n"));
        if !self.user.is_empty() {
            cfg.push_str(&format!("user = \"{}:{}\"\n", esc(&self.user), esc(&self.password)));
        }
        if let Some(b) = body {
            cfg.push_str("header = \"Content-Type: application/json\"\n");
            cfg.push_str(&format!("data-binary = \"{}\"\n", esc(&b.to_compact())));
        }
        let mut child = Command::new("curl")
            .args([
                "-sS",
                "--insecure",
                "--max-time",
                &timeout_s.to_string(),
                "-w",
                "\n%{http_code}",
                "--config",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("curl não encontrado: {e}"))?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(cfg.as_bytes())
            .map_err(|e| e.to_string())?;
        let o = child.wait_with_output().map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&o.stdout).into_owned();
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
        let code: u16 = code.trim().parse().unwrap_or(0);
        if code == 0 {
            return Err(format!(
                "Sunshine não respondeu: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Ok((code, Json::parse(body).ok()))
    }
}

pub fn api_from_config(cfg: &crate::config::Config) -> CurlApi {
    let (url, user, password) = cfg.sunshine();
    CurlApi { url, user, password }
}

fn truthy(v: Option<&Json>) -> bool {
    match v {
        Some(Json::Bool(b)) => *b,
        Some(Json::Str(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Enter Moonlight's PIN in Sunshine. Supports both API generations:
///   new: GET /api/pin -> {"pairings":[{id,name,address}]} then POST {pairing_id,pin,name}
///   old: POST {pin,name}
pub fn submit_pin(api: &dyn SunshineApi, pin: &str, name: &str, client_ip: &str, timeout_s: u64, poll_ms: u64) -> bool {
    let deadline = util::now_secs() + timeout_s;
    let ip = client_ip.trim_start_matches("::ffff:");
    while util::now_secs() <= deadline {
        match api.call("GET", "/api/pin", None, 5) {
            Ok((401, _)) => {
                crate::error!("o Sunshine recusou o usuário/senha salvos (rode 'deckscreen-host install' de novo)");
                return false;
            }
            Ok((200, Some(body))) if body.get("pairings").is_some() => {
                let pend = body
                    .get("pairings")
                    .and_then(|p| p.as_arr())
                    .cloned()
                    .unwrap_or_default();
                let pick = pend
                    .iter()
                    .find(|p| !ip.is_empty() && p.str_at("address").contains(ip))
                    .or_else(|| pend.first());
                if let Some(p) = pick {
                    let req = obj! {"pairing_id" => p.str_at("id"), "pin" => pin, "name" => name};
                    if let Ok((200, Some(r))) = api.call("POST", "/api/pin", Some(&req), 40) {
                        if truthy(r.get("status")) {
                            crate::info!("Sunshine: PIN aceito (API nova)");
                            return true;
                        }
                    }
                }
            }
            Ok(_) => {
                let req = obj! {"pin" => pin, "name" => name};
                match api.call("POST", "/api/pin", Some(&req), 40) {
                    Ok((401, _)) => {
                        crate::error!("o Sunshine recusou o usuário/senha salvos");
                        return false;
                    }
                    Ok((200, Some(r))) if truthy(r.get("status")) => {
                        crate::info!("Sunshine: PIN aceito (API antiga)");
                        return true;
                    }
                    _ => {}
                }
            }
            Err(e) => crate::warn!("Sunshine ainda não respondeu: {e}"),
        }
        util::sleep_ms(poll_ms);
    }
    crate::warn!("o Sunshine não recebeu pedido do Moonlight a tempo (ou ele já estava pareado)");
    false
}

pub fn close_app(api: &dyn SunshineApi) -> bool {
    matches!(api.call("POST", "/api/apps/close", None, 5), Ok((200, _)))
}

pub fn reachable(api: &dyn SunshineApi) -> bool {
    matches!(api.call("GET", "/api/apps", None, 3), Ok((200, _)))
}

// ------------------------------------------------------------------ binary / service discovery

/// The `sunshine` command: native package or an AppImage in ~/Applications.
pub fn find_binary() -> Option<String> {
    if let Some(p) = util::which("sunshine") {
        return Some(p.to_string_lossy().into_owned());
    }
    let apps = util::home().join("Applications");
    let mut found: Vec<_> = std::fs::read_dir(apps)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            n.starts_with("sunshine") && n.ends_with(".appimage")
        })
        .collect();
    found.sort();
    found.pop().map(|p| p.to_string_lossy().into_owned())
}

pub fn service_name() -> Option<String> {
    let (_, out) = util::run_text("systemctl", &["--user", "list-unit-files", "--no-legend"]);
    out.lines()
        .filter_map(|l| l.split_whitespace().next())
        .find(|u| *u == "app-dev.lizardbyte.app.Sunshine.service" || *u == "sunshine.service")
        .map(str::to_string)
}

pub fn is_flatpak_install() -> bool {
    util::which("sunshine").is_none()
        && util::run("flatpak", &["info", "dev.lizardbyte.app.Sunshine"])
            .map(|o| o.status.success())
            .unwrap_or(false)
}

// ------------------------------------------------------------------ sunshine.conf / apps.json

fn conf_key(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    t.split_once('=').map(|(k, _)| k.trim().to_string())
}

/// Set keys (value None = remove) keeping every other line and comment.
pub fn merge_conf(text: &str, values: &[(&str, Option<&str>)]) -> String {
    let mut done: Vec<&str> = vec![];
    let mut out: Vec<String> = vec![];
    for line in text.lines() {
        match conf_key(line) {
            Some(k) => match values.iter().find(|(vk, _)| *vk == k) {
                Some((vk, v)) => {
                    if done.contains(vk) {
                        continue;
                    }
                    done.push(vk);
                    if let Some(v) = v {
                        out.push(format!("{vk} = {v}"));
                    }
                }
                None => out.push(line.to_string()),
            },
            None => out.push(line.to_string()),
        }
    }
    for (k, v) in values {
        if !done.contains(k) {
            if let Some(v) = v {
                out.push(format!("{k} = {v}"));
            }
        }
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

pub fn conf_get(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find(|l| conf_key(l).as_deref() == Some(key))
        .and_then(|l| l.split_once('='))
        .map(|(_, v)| v.trim().to_string())
}

/// Fork of Sunshine with `portal_virtual_display` (needed on GNOME).
pub const FORK_REPO: &str = "lucasfracaro/Sunshine";

/// Capture method / output name per desktop.
/// GNOME + DeckScreen build of Sunshine: portal capture of a virtual monitor
/// (or of the real screen when mirroring).
pub fn conf_values(d: Desktop, gnome_virtual: bool, mirror: bool) -> Vec<(&'static str, Option<&'static str>)> {
    match d {
        Desktop::Kde => vec![("capture", Some("kwin")), ("output_name", Some(KDE_OUTPUT))],
        Desktop::Hyprland => vec![("capture", Some("wlr")), ("output_name", Some(HYPR_OUTPUT))],
        Desktop::Gnome if gnome_virtual => vec![
            ("capture", Some("portal")),
            (
                "portal_virtual_display",
                Some(if mirror { "disabled" } else { "enabled" }),
            ),
            ("output_name", None),
        ],
        // mirror only: don't touch the capture method, just drop our output name
        _ => vec![],
    }
}

pub fn merge_apps(data: Option<Json>, exe: &str) -> Json {
    let mut data = match data {
        Some(j @ Json::Obj(_)) => j,
        _ => Json::obj(),
    };
    data.entry_obj("env");
    let mut apps: Vec<Json> = data
        .get("apps")
        .and_then(|a| a.as_arr())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.str_at("name") != APP_NAME)
        .collect();
    if apps.is_empty() {
        apps.push(obj! {"name" => "Desktop", "image-path" => "desktop.png"});
    }
    apps.push(obj! {
        "name" => APP_NAME,
        "cmd" => "",
        "output" => "",
        "image-path" => "desktop.png",
        "auto-detach" => true,
        "wait-all" => false,
        "exclude-global-prep-cmd" => false,
        "prep-cmd" => vec![obj! {"do" => format!("{exe} hook start"), "undo" => format!("{exe} hook stop"), "elevated" => false}],
    });
    data.set("apps", apps);
    data
}

/// Wire Sunshine for this desktop. Writes backups first.
pub fn configure(d: Desktop, exe: &str, mirror: bool) -> Result<Vec<String>, String> {
    let dir = util::sunshine_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = util::now_secs();
    let mut notes = vec![];

    let conf = dir.join("sunshine.conf");
    let old = std::fs::read_to_string(&conf).unwrap_or_default();
    if !old.is_empty() {
        let _ = std::fs::copy(&conf, dir.join(format!("sunshine.conf.deckscreen-bak.{stamp}")));
    }
    let gnome_virtual = crate::display::gnome_virtual();
    let mut values = conf_values(d, gnome_virtual, mirror);
    // remove an output_name we set for another desktop earlier
    if values.is_empty() {
        if let Some(v) = conf_get(&old, "output_name") {
            if v == KDE_OUTPUT || v == HYPR_OUTPUT {
                values.push(("output_name", None));
            }
        }
    }
    std::fs::write(&conf, merge_conf(&old, &values)).map_err(|e| e.to_string())?;
    notes.push(match d {
        Desktop::Kde => format!("sunshine.conf: capture = kwin, output_name = {KDE_OUTPUT}"),
        Desktop::Hyprland => format!("sunshine.conf: capture = wlr, output_name = {HYPR_OUTPUT}"),
        Desktop::Gnome if gnome_virtual => format!(
            "sunshine.conf: capture = portal, portal_virtual_display = {}",
            if mirror { "disabled" } else { "enabled" }
        ),
        _ => "sunshine.conf: captura mantida (modo espelhar)".to_string(),
    });

    let apps_path = dir.join("apps.json");
    let apps = std::fs::read_to_string(&apps_path).ok().and_then(|s| {
        let _ = std::fs::copy(&apps_path, dir.join(format!("apps.json.deckscreen-bak.{stamp}")));
        Json::parse(&s).ok()
    });
    std::fs::write(&apps_path, merge_apps(apps, exe).pretty() + "\n").map_err(|e| e.to_string())?;
    notes.push(format!("apps.json: app '{APP_NAME}' (cria/remove a tela do Deck)"));
    Ok(notes)
}

pub fn unconfigure() {
    let dir = util::sunshine_dir();
    let conf = dir.join("sunshine.conf");
    if let Ok(old) = std::fs::read_to_string(&conf) {
        let old = merge_conf(&old, &[("portal_virtual_display", None)]);
        let _ = std::fs::write(&conf, &old);
        if matches!(
            conf_get(&old, "output_name").as_deref(),
            Some(KDE_OUTPUT) | Some(HYPR_OUTPUT)
        ) {
            let _ = std::fs::write(&conf, merge_conf(&old, &[("output_name", None)]));
        }
    }
    let apps_path = dir.join("apps.json");
    if let Some(mut data) = std::fs::read_to_string(&apps_path)
        .ok()
        .and_then(|s| Json::parse(&s).ok())
    {
        let apps: Vec<Json> = data
            .get("apps")
            .and_then(|a| a.as_arr())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a.str_at("name") != APP_NAME)
            .collect();
        data.set("apps", apps);
        let _ = std::fs::write(&apps_path, data.pretty() + "\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn conf_merge() {
        let text = "# c\nsunshine_name = PC\ncapture = kms\ncapture = x11\n";
        let new = merge_conf(text, &[("capture", Some("kwin")), ("output_name", Some(KDE_OUTPUT))]);
        assert!(new.contains("# c") && new.contains("sunshine_name = PC"));
        assert_eq!(new.matches("capture =").count(), 1);
        assert_eq!(conf_get(&new, "capture").as_deref(), Some("kwin"));
        assert_eq!(conf_get(&new, "output_name").as_deref(), Some(KDE_OUTPUT));
        let removed = merge_conf(&new, &[("output_name", None)]);
        assert!(conf_get(&removed, "output_name").is_none());
    }

    #[test]
    fn gnome_conf() {
        assert!(conf_values(Desktop::Gnome, false, false).is_empty());
        let v = conf_values(Desktop::Gnome, true, false);
        let new = merge_conf("capture = kms\noutput_name = Virtual-DeckScreen\n", &v);
        assert_eq!(conf_get(&new, "capture").as_deref(), Some("portal"));
        assert_eq!(conf_get(&new, "portal_virtual_display").as_deref(), Some("enabled"));
        assert!(conf_get(&new, "output_name").is_none());
        let m = merge_conf(&new, &conf_values(Desktop::Gnome, true, true));
        assert_eq!(conf_get(&m, "portal_virtual_display").as_deref(), Some("disabled"));
    }

    #[test]
    fn apps_merge() {
        let data = Json::parse(r#"{"env":{"PATH":"x"},"apps":[{"name":"Desktop"},{"name":"Steam"}]}"#).unwrap();
        let once = merge_apps(Some(data), "/bin/ds");
        let twice = merge_apps(Some(once), "/bin/ds");
        let names: Vec<String> = twice
            .get("apps")
            .unwrap()
            .as_arr()
            .unwrap()
            .iter()
            .map(|a| a.str_at("name"))
            .collect();
        assert_eq!(names, vec!["Desktop", "Steam", APP_NAME]);
        assert_eq!(twice.get("env").unwrap().str_at("PATH"), "x");
        let fresh = merge_apps(None, "/bin/ds");
        assert!(fresh.get("env").is_some());
        let app = &fresh.get("apps").unwrap().as_arr().unwrap()[1];
        assert_eq!(
            app.get("prep-cmd").unwrap().as_arr().unwrap()[0].str_at("do"),
            "/bin/ds hook start"
        );
    }

    struct Fake {
        calls: Mutex<Vec<(String, String, Option<Json>)>>,
        mode: &'static str,
    }
    impl SunshineApi for Fake {
        fn call(&self, method: &str, path: &str, body: Option<&Json>, _t: u32) -> Result<(u16, Option<Json>), String> {
            let n = {
                let mut c = self.calls.lock().unwrap();
                c.push((method.into(), path.into(), body.cloned()));
                c.len()
            };
            match (self.mode, method) {
                ("new", "GET") if n < 3 => Ok((200, Some(obj! {"pairings" => Vec::<Json>::new()}))),
                ("new", "GET") => Ok((
                    200,
                    Some(
                        Json::parse(
                            r#"{"pairings":[{"id":"abababababababababababababababab","address":"192.168.0.50"}]}"#,
                        )
                        .unwrap(),
                    ),
                )),
                ("new", _) => Ok((200, Some(obj! {"status" => true}))),
                ("old", "GET") => Ok((404, None)),
                ("old", _) => Ok((200, Some(obj! {"status" => "true"}))),
                _ => Ok((401, None)),
            }
        }
    }

    #[test]
    fn pin_new_and_old_api() {
        let f = Fake {
            calls: Mutex::new(vec![]),
            mode: "new",
        };
        assert!(submit_pin(&f, "1234", "Deck", "::ffff:192.168.0.50", 5, 1));
        let calls = f.calls.lock().unwrap();
        let post = calls.iter().find(|c| c.0 == "POST").unwrap();
        assert_eq!(
            post.2.as_ref().unwrap().str_at("pairing_id"),
            "abababababababababababababababab"
        );

        let f = Fake {
            calls: Mutex::new(vec![]),
            mode: "old",
        };
        assert!(submit_pin(&f, "1234", "Deck", "", 5, 1));
        let f = Fake {
            calls: Mutex::new(vec![]),
            mode: "unauth",
        };
        assert!(!submit_pin(&f, "1234", "Deck", "", 5, 1));
    }
}
