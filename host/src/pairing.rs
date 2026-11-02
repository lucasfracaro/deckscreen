//! Pairing: the Deck asks, the PC user approves (desktop notification or CLI),
//! the Deck receives a 256-bit key used to sign every later request, and the
//! same PIN is entered in Sunshine so Moonlight gets paired too.

use crate::config::{valid_device_id, Config};
use crate::crypto;
use crate::json::Json;
use crate::sunshine::{self, SunshineApi};
use crate::util;
use std::sync::{Arc, Mutex};

const TTL_SECS: u64 = 300;
const MAX_PENDING: usize = 3;

#[derive(Clone, Debug)]
pub struct PairRequest {
    pub id: String,
    pub device_id: String,
    pub device_name: String,
    pub pin: String,
    pub ip: String,
    pub state: String, // pending | approved | denied
    pub key: Option<String>,
    pub sunshine: String, // waiting | ok | failed
    pub created: u64,
}

pub struct Pairing {
    pub cfg: Arc<Mutex<Config>>,
    pub api: Arc<dyn SunshineApi>,
    pub notify: bool,
    reqs: Mutex<Vec<PairRequest>>,
}

impl Pairing {
    pub fn new(cfg: Arc<Mutex<Config>>, api: Arc<dyn SunshineApi>, notify: bool) -> Arc<Pairing> {
        Arc::new(Pairing {
            cfg,
            api,
            notify,
            reqs: Mutex::new(vec![]),
        })
    }

    fn gc(reqs: &mut Vec<PairRequest>) {
        let now = util::now_secs();
        reqs.retain(|r| now.saturating_sub(r.created) < TTL_SECS);
    }

    pub fn create(
        self: &Arc<Self>,
        device_id: &str,
        device_name: &str,
        pin: &str,
        ip: &str,
    ) -> Result<PairRequest, (u16, String)> {
        if pin.len() != 4 || !pin.chars().all(|c| c.is_ascii_digit()) {
            return Err((400, "PIN deve ter 4 dígitos".into()));
        }
        let device_id = if valid_device_id(device_id) {
            device_id.to_string()
        } else {
            crypto::token(8)
        };
        let name: String = device_name
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        let name = if name.is_empty() {
            "Steam Deck".to_string()
        } else {
            name
        };
        let req = {
            let mut reqs = self.reqs.lock().unwrap();
            Self::gc(&mut reqs);
            if reqs.iter().filter(|r| r.state == "pending").count() >= MAX_PENDING {
                return Err((429, "muitos pedidos pendentes".into()));
            }
            let r = PairRequest {
                id: crypto::token(8),
                device_id,
                device_name: name,
                pin: pin.to_string(),
                ip: ip.to_string(),
                state: "pending".into(),
                key: None,
                sunshine: "waiting".into(),
                created: util::now_secs(),
            };
            reqs.push(r.clone());
            r
        };
        crate::info!(
            "pedido de pareamento {} de {} ({}) PIN {}",
            req.id,
            req.device_name,
            req.ip,
            req.pin
        );
        if self.notify {
            let me = self.clone();
            let id = req.id.clone();
            std::thread::spawn(move || me.notify(&id));
        }
        Ok(req)
    }

    /// Status for the Deck. The key is only revealed to the IP that asked.
    pub fn status(&self, id: &str, requester_ip: &str) -> Option<Json> {
        let reqs = self.reqs.lock().unwrap();
        let r = reqs.iter().find(|r| r.id == id)?;
        let key = if r.ip == requester_ip { r.key.clone() } else { None };
        Some(
            obj! {"id" => r.id.clone(), "state" => r.state.clone(), "sunshine" => r.sunshine.clone(),
            "device_id" => r.device_id.clone(), "key" => key},
        )
    }

    pub fn pending(&self) -> Vec<PairRequest> {
        let mut reqs = self.reqs.lock().unwrap();
        Self::gc(&mut reqs);
        reqs.iter().filter(|r| r.state == "pending").cloned().collect()
    }

    pub fn decide(self: &Arc<Self>, id: &str, accept: bool) -> bool {
        let req = {
            let mut reqs = self.reqs.lock().unwrap();
            let Some(r) = reqs.iter_mut().find(|r| r.id == id && r.state == "pending") else {
                return false;
            };
            if !accept {
                r.state = "denied".into();
                crate::info!("pareamento {id} recusado");
                return true;
            }
            let key = self.cfg.lock().unwrap().add_device(&r.device_id, &r.device_name);
            r.key = Some(key);
            r.state = "approved".into();
            crate::info!("pareamento {id} aceito ({})", r.device_name);
            r.clone()
        };
        let me = self.clone();
        std::thread::spawn(move || {
            let ok = sunshine::submit_pin(me.api.as_ref(), &req.pin, &req.device_name, &req.ip, 90, 1500);
            if let Some(r) = me.reqs.lock().unwrap().iter_mut().find(|r| r.id == req.id) {
                r.sunshine = if ok { "ok" } else { "failed" }.into();
            }
        });
        true
    }

    fn notify(self: &Arc<Self>, id: &str) {
        let Some(r) = self.reqs.lock().unwrap().iter().find(|r| r.id == id).cloned() else {
            return;
        };
        if util::which("notify-send").is_none() {
            crate::info!("sem notify-send: aprove com 'deckscreen-host accept'");
            return;
        }
        let title = format!("{} quer se conectar", r.device_name);
        let body = format!("PIN {} — confira se é o mesmo número mostrado no Deck.", r.pin);
        let out = util::run(
            "notify-send",
            &[
                "--app-name=DeckScreen",
                "--icon=video-display",
                "--urgency=critical",
                "--expire-time=120000",
                "--wait",
                "--action=accept=Aceitar",
                "--action=deny=Recusar",
                &title,
                &body,
            ],
        );
        let action = out
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("")
                    .to_string()
            })
            .unwrap_or_default();
        if action == "accept" || action == "deny" {
            self.decide(id, action == "accept");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn flow() {
        let p = std::env::temp_dir().join(format!("ds-pair-{}.json", crypto::token(4)));
        let cfg = Arc::new(Mutex::new(Config::load(p)));
        let pm = Pairing::new(cfg.clone(), Arc::new(OkApi), false);
        assert_eq!(pm.create("d1", "Deck", "12a4", "1.2.3.4").unwrap_err().0, 400);
        let r = pm.create("d1", "Deck\u{7}", "1234", "1.2.3.4").unwrap();
        assert_eq!(r.device_name, "Deck");
        assert_eq!(pm.pending().len(), 1);
        assert!(pm.decide(&r.id, true));
        assert!(!pm.decide(&r.id, true), "only once");
        let st = pm.status(&r.id, "1.2.3.4").unwrap();
        assert_eq!(st.str_at("state"), "approved");
        assert_eq!(st.str_at("key").len(), 64);
        assert!(
            pm.status(&r.id, "9.9.9.9").unwrap().get("key") == Some(&Json::Null),
            "key only to requester"
        );
        for _ in 0..50 {
            if pm.status(&r.id, "1.2.3.4").unwrap().str_at("sunshine") != "waiting" {
                break;
            }
            util::sleep_ms(20);
        }
        assert_eq!(pm.status(&r.id, "1.2.3.4").unwrap().str_at("sunshine"), "ok");
        assert!(cfg.lock().unwrap().device_key("d1").is_some());
        for i in 0..3 {
            pm.create(&format!("x{i}"), "D", "1111", "1.1.1.1").unwrap();
        }
        assert_eq!(pm.create("x9", "D", "1111", "1.1.1.1").unwrap_err().0, 429);
    }
}
