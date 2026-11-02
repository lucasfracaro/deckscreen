//! ~/.config/deckscreen/config.json (mode 600) and the user-facing settings.

use crate::crypto;
use crate::json::Json;
use crate::obj;
use crate::util;
use std::path::PathBuf;
use std::time::SystemTime;

pub const MODES: [&str; 3] = ["extend", "deck_only", "mirror"];
pub const POSITIONS: [&str; 4] = ["right", "left", "above", "below"];
pub const ORIENTATIONS: [&str; 3] = ["landscape", "portrait", "portrait_flipped"];
pub const PROFILES: [&str; 3] = ["desktop", "cemu", "custom"];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// extend = extra screen · deck_only = PC monitors off · mirror = Deck shows the main screen
    pub mode: String,
    pub position: String,
    pub scale: f64,
    pub orientation: String,
    /// desktop = nothing special · cemu = Wii U GamePad window · custom = user rules
    pub profile: String,
    /// open the profile's app when the Deck connects
    pub open_app: bool,
    /// close it again when the Deck disconnects
    pub close_app: bool,
    pub custom_rules: Vec<String>,
    pub custom_fullscreen: bool,
    pub custom_command: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: "extend".into(),
            position: "right".into(),
            scale: 1.0,
            orientation: "landscape".into(),
            profile: "desktop".into(),
            open_app: true,
            close_app: false,
            custom_rules: vec![],
            custom_fullscreen: true,
            custom_command: String::new(),
        }
    }
}

fn pick(v: Option<&Json>, allowed: &[&str], current: &str) -> String {
    match v.and_then(|x| x.as_str()) {
        Some(s) if allowed.contains(&s) => s.to_string(),
        _ => current.to_string(),
    }
}

fn clean_rules(v: &Json) -> Option<Vec<String>> {
    v.as_arr().map(|a| {
        a.iter()
            .filter_map(|x| x.as_str())
            .map(|s| s.trim().chars().take(120).collect::<String>())
            .filter(|s| !s.is_empty())
            .take(20)
            .collect()
    })
}

impl Settings {
    /// Apply a (partial) JSON patch, validating every field.
    pub fn apply(&mut self, p: &Json) {
        self.mode = pick(p.get("mode"), &MODES, &self.mode);
        self.position = pick(p.get("position"), &POSITIONS, &self.position);
        self.orientation = pick(p.get("orientation"), &ORIENTATIONS, &self.orientation);
        self.profile = pick(p.get("profile"), &PROFILES, &self.profile);
        if let Some(s) = p.get("scale").and_then(|v| v.as_f64()) {
            if (0.5..=3.0).contains(&s) {
                self.scale = (s * 100.0).round() / 100.0;
            }
        }
        if let Some(b) = p.get("open_app").and_then(|v| v.as_bool()) {
            self.open_app = b;
        }
        if let Some(b) = p.get("close_app").and_then(|v| v.as_bool()) {
            self.close_app = b;
        }
        if let Some(c) = p.get("custom") {
            if let Some(r) = c.get("rules").and_then(clean_rules) {
                self.custom_rules = r;
            }
            if let Some(b) = c.get("fullscreen").and_then(|v| v.as_bool()) {
                self.custom_fullscreen = b;
            }
            if let Some(cmd) = c.get("command").and_then(|v| v.as_str()) {
                self.custom_command = cmd.trim().chars().take(500).collect();
            }
        }
        // v1 → v2 migration: "wiiu": {enabled, fullscreen, rules}
        if let Some(w) = p.get("wiiu") {
            if w.bool_at("enabled", false) {
                self.profile = "custom".into();
                if let Some(r) = w.get("rules").and_then(clean_rules) {
                    self.custom_rules = r;
                }
                self.custom_fullscreen = w.bool_at("fullscreen", true);
            }
        }
    }

    pub fn from_json(v: &Json) -> Settings {
        let mut s = Settings::default();
        s.apply(v);
        s
    }

    pub fn to_json(&self) -> Json {
        obj! {
            "mode" => self.mode.clone(),
            "position" => self.position.clone(),
            "scale" => self.scale,
            "orientation" => self.orientation.clone(),
            "profile" => self.profile.clone(),
            "open_app" => self.open_app,
            "close_app" => self.close_app,
            "custom" => obj! {
                "rules" => self.custom_rules.clone(),
                "fullscreen" => self.custom_fullscreen,
                "command" => self.custom_command.clone(),
            },
        }
    }
}

/// What a profile does once the Deck screen exists.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Profile {
    pub rules: Vec<String>,
    pub fullscreen: bool,
    pub command: Option<Vec<String>>,
}

pub fn cemu_command() -> Option<Vec<String>> {
    for bin in ["cemu", "Cemu"] {
        if util::which(bin).is_some() {
            return Some(vec![bin.to_string()]);
        }
    }
    if util::run("flatpak", &["info", "info.cemu.Cemu"])
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some(vec!["flatpak".into(), "run".into(), "info.cemu.Cemu".into()]);
    }
    let apps = util::home().join("Applications");
    if let Ok(rd) = std::fs::read_dir(apps) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if n.starts_with("cemu") && n.ends_with(".appimage") {
                return Some(vec![e.path().to_string_lossy().into_owned()]);
            }
        }
    }
    None
}

/// Split a command line respecting simple quotes ("a b" / 'a b').
pub fn split_command(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"') | (None, '\'') => {
                quote = Some(c);
                has = true;
            }
            (None, c) if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn resolve_profile(s: &Settings) -> Profile {
    match s.profile.as_str() {
        "cemu" => Profile {
            rules: vec!["GamePad View".into()],
            fullscreen: true,
            command: cemu_command(),
        },
        "custom" => Profile {
            rules: s.custom_rules.clone(),
            fullscreen: s.custom_fullscreen,
            command: Some(split_command(&s.custom_command)).filter(|v| !v.is_empty()),
        },
        _ => Profile::default(),
    }
}

// ------------------------------------------------------------------ config file

pub struct Config {
    pub path: PathBuf,
    pub data: Json,
    loaded: Option<SystemTime>,
}

/// Read sunshine.portal_virtual without creating/modifying config.json.
pub fn peek_portal_virtual() -> bool {
    std::fs::read_to_string(Config::default_path())
        .ok()
        .and_then(|s| Json::parse(&s).ok())
        .and_then(|j| j.get("sunshine").map(|s| s.bool_at("portal_virtual", false)))
        .unwrap_or(false)
}

pub fn valid_device_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Config {
    pub fn default_path() -> PathBuf {
        util::config_dir().join("config.json")
    }

    pub fn load_default() -> Config {
        Config::load(Config::default_path())
    }

    pub fn load(path: PathBuf) -> Config {
        let mut c = Config {
            path,
            data: Json::obj(),
            loaded: None,
        };
        c.reload();
        c
    }

    fn reload(&mut self) {
        self.loaded = util::mtime(&self.path);
        self.data = match std::fs::read_to_string(&self.path) {
            Ok(s) => match Json::parse(&s) {
                Ok(j @ Json::Obj(_)) => j,
                _ => {
                    crate::error!("config.json corrompido; usando padrões (cópia salva como .bad)");
                    let _ = std::fs::copy(&self.path, self.path.with_extension("bad"));
                    Json::obj()
                }
            },
            Err(_) => Json::obj(),
        };
        let mut changed = false;
        if self.data.str_at("local_secret").len() < 32 {
            self.data.set("local_secret", crypto::token(24));
            changed = true;
        }
        self.data.entry_obj("devices");
        let sun = self.data.entry_obj("sunshine");
        if sun.str_at("url").is_empty() {
            sun.set("url", "https://127.0.0.1:47990");
        }
        let settings = Settings::from_json(self.data.get("settings").unwrap_or(&Json::Null));
        self.data.set("settings", settings.to_json());
        if changed || self.loaded.is_none() {
            self.save();
        }
    }

    /// Pick up edits made by another process (e.g. the CLI) without a restart.
    pub fn maybe_reload(&mut self) {
        if util::mtime(&self.path) != self.loaded {
            self.reload();
        }
    }

    pub fn save(&mut self) {
        if let Err(e) = util::write_private(&self.path, &self.data.pretty()) {
            crate::error!("não consegui salvar {}: {e}", self.path.display());
        }
        self.loaded = util::mtime(&self.path);
    }

    pub fn local_secret(&self) -> String {
        self.data.str_at("local_secret")
    }

    pub fn settings(&self) -> Settings {
        Settings::from_json(self.data.get("settings").unwrap_or(&Json::Null))
    }

    pub fn update_settings(&mut self, patch: &Json) -> Settings {
        let mut s = self.settings();
        s.apply(patch);
        self.data.set("settings", s.to_json());
        self.save();
        s
    }

    pub fn sunshine(&self) -> (String, String, String) {
        let s = self.data.get("sunshine").cloned().unwrap_or_default();
        (s.str_at("url"), s.str_at("user"), s.str_at("password"))
    }

    /// Sunshine is the DeckScreen build (portal_virtual_display), needed on GNOME.
    pub fn portal_virtual(&self) -> bool {
        self.data
            .get("sunshine")
            .map(|s| s.bool_at("portal_virtual", false))
            .unwrap_or(false)
    }

    pub fn set_portal_virtual(&mut self, on: bool) {
        self.data.entry_obj("sunshine").set("portal_virtual", on);
        self.save();
    }

    pub fn set_sunshine_creds(&mut self, user: &str, password: &str) {
        let s = self.data.entry_obj("sunshine");
        s.set("user", user);
        s.set("password", password);
        self.save();
    }

    /// Create (or replace) a paired device. Returns its hex key.
    pub fn add_device(&mut self, id: &str, name: &str) -> String {
        let key = crypto::token(32);
        self.data.entry_obj("devices").set(
            id,
            obj! {"name" => name, "key" => key.clone(), "added" => util::now_secs()},
        );
        self.save();
        key
    }

    pub fn device_key(&mut self, id: &str) -> Option<Vec<u8>> {
        self.maybe_reload();
        let k = self.data.get("devices")?.get(id)?.str_at("key");
        crypto::unhex(&k).filter(|k| k.len() == 32)
    }

    pub fn device_name(&self, id: &str) -> String {
        self.data
            .get("devices")
            .and_then(|d| d.get(id))
            .map(|d| d.str_at("name"))
            .unwrap_or_default()
    }

    pub fn forget(&mut self, id: &str) -> bool {
        let removed = self.data.entry_obj("devices").remove(id).is_some();
        if removed {
            self.save();
        }
        removed
    }

    pub fn devices(&self) -> Vec<(String, String, u64)> {
        self.data
            .get("devices")
            .and_then(|d| d.as_obj())
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.str_at("name"), v.i64_at("added", 0) as u64))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Copy safe to share in bug reports (no keys / passwords).
    pub fn redacted(&self) -> Json {
        let mut d = self.data.clone();
        d.set("local_secret", "***");
        if let Some(s) = d.get_mut("sunshine") {
            if !s.str_at("password").is_empty() {
                s.set("password", "***");
            }
        }
        if let Some(Json::Obj(devs)) = d.get_mut("devices") {
            for (_, v) in devs.iter_mut() {
                v.set("key", "***");
            }
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ds-cfg-{}-{}", name, crypto::token(4)));
        std::fs::create_dir_all(&d).unwrap();
        d.join("config.json")
    }

    #[test]
    fn settings_validate_and_migrate() {
        let s = Settings::from_json(
            &Json::parse(
                r#"{"mode":"bogus","position":"left","scale":9,"orientation":"portrait",
            "wiiu":{"enabled":true,"rules":[" Cemu ",""],"fullscreen":false}}"#,
            )
            .unwrap(),
        );
        assert_eq!(s.mode, "extend");
        assert_eq!(s.position, "left");
        assert_eq!(s.scale, 1.0);
        assert_eq!(s.orientation, "portrait");
        assert_eq!(s.profile, "custom");
        assert_eq!(s.custom_rules, vec!["Cemu"]);
        assert!(!s.custom_fullscreen);
        assert_eq!(Settings::from_json(&s.to_json()), s);
    }

    #[test]
    fn profiles() {
        let mut s = Settings::default();
        assert_eq!(resolve_profile(&s), Profile::default());
        s.apply(
            &Json::parse(r#"{"profile":"custom","custom":{"rules":["Secondary"],"command":"melonDS --x 'a b'"}}"#)
                .unwrap(),
        );
        let p = resolve_profile(&s);
        assert_eq!(p.rules, vec!["Secondary"]);
        assert_eq!(p.command.unwrap(), vec!["melonDS", "--x", "a b"]);
        s.apply(&crate::obj! {"profile" => "cemu"});
        assert_eq!(resolve_profile(&s).rules, vec!["GamePad View"]);
        assert_eq!(split_command(r#"a "" b"#), vec!["a", "", "b"]);
    }

    #[test]
    fn config_devices_and_reload() {
        let p = tmp("dev");
        let mut a = Config::load(p.clone());
        assert_eq!(a.local_secret().len(), 48);
        let key = a.add_device("deck1", "Steam Deck");
        assert_eq!(a.device_key("deck1").unwrap(), crypto::unhex(&key).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut b = Config::load(p.clone());
        assert!(b.forget("deck1"));
        assert!(a.device_key("deck1").is_none(), "a must see b's change");
        assert!(valid_device_id("abc-123_x") && !valid_device_id("a b") && !valid_device_id(""));
        let red = a.redacted();
        assert_eq!(red.str_at("local_secret"), "***");
    }
}
