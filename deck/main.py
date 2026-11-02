"""
DeckScreen — Decky Loader backend (runs on the Steam Deck).

Talks to `deckscreen-host` on the PC: HTTP on port 47555 with every request
signed (HMAC-SHA256, key created at pairing), UDP discovery on 47556,
Wake-on-LAN, connection test, and builds the Moonlight command line.
"""

import asyncio
import hashlib
import hmac
import json
import os
import random
import re
import secrets
import shlex
import socket
import statistics
import subprocess
import time
import urllib.error
import urllib.request
import uuid

import decky

API_PORT = 47555
DISCOVERY_PORT = 47556
DISCOVERY_MAGIC = b"DECKSCREEN_DISCOVER_V1"
MOONLIGHT_ID = "com.moonlight_stream.Moonlight"
SUNSHINE_APP = "DeckScreen"
FLATHUB = "https://dl.flathub.org/repo/flathub.flatpakrepo"

SETTINGS_FILE = os.path.join(decky.DECKY_PLUGIN_SETTINGS_DIR, "settings.json")

RESOLUTIONS = ("1280x800", "1920x1080", "1920x1200", "2560x1600")
PRESETS = ("low_latency", "balanced", "quality")
CODECS = ("auto", "H.264", "HEVC")

DEFAULTS = {
    "hosts": {},             # id -> {ip, port, name, key, mac, wired, capability}
    "active": "",
    "device_id": "",
    "resolution": "1280x800",
    "fps": 60,
    "bitrate": 20000,        # kbps
    "preset": "low_latency",
    "codec": "auto",
    "overlay": False,        # Moonlight performance overlay (shows the latency)
    "audio_on_host": False,
    "absolute_touch": True,  # touchscreen = direct clicks on the Deck screen
    "gyro": False,           # turn Steam Input off for the shortcut so motion reaches the PC
    "shortcut_app_id": 0,
}


# ------------------------------------------------------------------ settings

def _load() -> dict:
    s = json.loads(json.dumps(DEFAULTS))
    try:
        with open(SETTINGS_FILE, encoding="utf-8") as f:
            s.update(json.load(f))
    except (FileNotFoundError, json.JSONDecodeError):
        pass
    changed = False
    # v1 -> v2: single host + token
    if isinstance(s.get("host"), dict):
        h = s.pop("host")
        s["hosts"][h.get("name") or h["ip"]] = {"ip": h["ip"], "port": h.get("port", API_PORT), "name": h.get("name", h["ip"])}
        s["active"] = h.get("name") or h["ip"]
        s.pop("token", None)
        changed = True
    if not s.get("device_id"):
        s["device_id"] = uuid.uuid4().hex
        changed = True
    if s["active"] not in s["hosts"]:
        s["active"] = next(iter(s["hosts"]), "")
    if changed:
        _save(s)
    return s


def _save(s: dict) -> None:
    os.makedirs(os.path.dirname(SETTINGS_FILE), exist_ok=True)
    tmp = SETTINGS_FILE + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(s, f, indent=2)
    os.chmod(tmp, 0o600)
    os.replace(tmp, SETTINGS_FILE)


def _active(s: dict):
    return s["hosts"].get(s.get("active") or "")


def _env() -> dict:
    """Clean environment for system binaries (Decky's bundled Python sets LD_LIBRARY_PATH)."""
    env = dict(os.environ)
    orig = env.pop("LD_LIBRARY_PATH_ORIG", None)
    env.pop("LD_LIBRARY_PATH", None)
    if orig:
        env["LD_LIBRARY_PATH"] = orig
    uid = os.getuid()
    env["HOME"] = decky.DECKY_USER_HOME
    env.setdefault("XDG_RUNTIME_DIR", f"/run/user/{uid}")
    env.setdefault("DBUS_SESSION_BUS_ADDRESS", f"unix:path=/run/user/{uid}/bus")
    return env


def _run(cmd: list, timeout: int = 60) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, env=_env())


# ------------------------------------------------------------------ signed HTTP

def sign_headers(key_hex: str, device_id: str, method: str, target: str, body: bytes, ts=None, nonce=None) -> dict:
    ts = str(int(time.time()) if ts is None else ts)
    nonce = nonce or secrets.token_hex(16)
    msg = f"{method}\n{target}\n{ts}\n{nonce}\n{hashlib.sha256(body).hexdigest()}".encode()
    sig = hmac.new(bytes.fromhex(key_hex), msg, hashlib.sha256).hexdigest()
    return {"X-DS-Device": device_id, "X-DS-Time": ts, "X-DS-Nonce": nonce, "X-DS-Sig": sig}


def _http(method: str, host: dict, target: str, body=None, key: str = "", device_id: str = "", timeout: float = 5):
    data = json.dumps(body).encode() if body is not None else b""
    req = urllib.request.Request(f"http://{host['ip']}:{host.get('port', API_PORT)}{target}",
                                 data=data if body is not None else None, method=method)
    if body is not None:
        req.add_header("Content-Type", "application/json")
    if key:
        for k, v in sign_headers(key, device_id, method, target, data).items():
            req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return {"ok": True, "code": r.status, "data": json.loads(r.read() or b"null")}
    except urllib.error.HTTPError as e:
        try:
            payload = json.loads(e.read() or b"null")
        except json.JSONDecodeError:
            payload = None
        msg = payload.get("error") if isinstance(payload, dict) else str(e)
        return {"ok": False, "code": e.code, "error": msg}
    except (urllib.error.URLError, OSError, TimeoutError) as e:
        return {"ok": False, "code": 0, "error": f"PC não respondeu ({getattr(e, 'reason', e)})"}


def _discover(timeout: float = 2.0) -> list:
    found = {}
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    s.settimeout(0.3)
    try:
        for _ in range(2):
            try:
                s.sendto(DISCOVERY_MAGIC, ("255.255.255.255", DISCOVERY_PORT))
            except OSError:
                pass
            end = time.monotonic() + timeout / 2
            while time.monotonic() < end:
                try:
                    data, addr = s.recvfrom(1024)
                except socket.timeout:
                    continue
                try:
                    info = json.loads(data)
                except json.JSONDecodeError:
                    continue
                if info.get("app") == "deckscreen":
                    found[addr[0]] = {"ip": addr[0], "port": int(info.get("port", API_PORT)),
                                      "name": str(info.get("name", addr[0])), "version": info.get("version", ""),
                                      "capability": info.get("capability", "")}
    finally:
        s.close()
    return sorted(found.values(), key=lambda h: h["name"])


# ------------------------------------------------------------------ Wake-on-LAN

def magic_packet(mac: str) -> bytes:
    raw = bytes.fromhex(re.sub(r"[^0-9a-fA-F]", "", mac))
    if len(raw) != 6:
        raise ValueError("MAC inválido")
    return b"\xff" * 6 + raw * 16


def _send_wol(mac: str, ip: str) -> None:
    pkt = magic_packet(mac)
    targets = {"255.255.255.255"}
    parts = ip.split(".")
    if len(parts) == 4:
        targets.add(".".join(parts[:3] + ["255"]))   # common /24 home networks
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    try:
        for _ in range(3):
            for t in targets:
                for port in (9, 7):
                    try:
                        s.sendto(pkt, (t, port))
                    except OSError:
                        pass
            time.sleep(0.2)
    finally:
        s.close()


# ------------------------------------------------------------------ connection test

def _split_nm(line: str) -> list:
    return [p.replace("\\:", ":") for p in re.split(r"(?<!\\):", line)]


def deck_network() -> dict:
    info = {"type": "unknown"}
    try:
        p = _run(["nmcli", "-t", "-f", "DEVICE,TYPE,STATE", "device"], timeout=10)
        for line in p.stdout.splitlines():
            dev, typ, state = (_split_nm(line) + ["", "", ""])[:3]
            if typ == "ethernet" and state == "connected":
                return {"type": "ethernet", "device": dev}
        p = _run(["nmcli", "-t", "-f", "ACTIVE,SSID,FREQ,SIGNAL,RATE", "device", "wifi"], timeout=10)
        for line in p.stdout.splitlines():
            f = _split_nm(line)
            if len(f) >= 5 and f[0] in ("yes", "sim"):
                freq = int(re.sub(r"\D", "", f[2]) or 0)
                info = {"type": "wifi", "ssid": f[1], "freq": freq, "signal": int(f[3] or 0), "rate": f[4],
                        "band": "6 GHz" if freq >= 5925 else "5 GHz" if freq >= 4900 else "2.4 GHz"}
                break
    except (OSError, subprocess.TimeoutExpired, ValueError):
        pass
    return info


def tcp_rtt(ip: str, port: int, count: int = 20) -> dict:
    """Round-trip time using TCP handshakes (needs no root, unlike ICMP ping)."""
    times, lost = [], 0
    for _ in range(count):
        t0 = time.perf_counter()
        try:
            with socket.create_connection((ip, port), timeout=1.0):
                times.append((time.perf_counter() - t0) * 1000)
        except OSError:
            lost += 1
        time.sleep(0.05)
    if not times:
        return {"ok": False, "lost": lost, "count": count}
    return {"ok": True, "count": count, "lost": lost, "min": round(min(times), 1), "avg": round(statistics.mean(times), 1),
            "max": round(max(times), 1), "jitter": round(statistics.pstdev(times), 1)}


def verdict(rtt: dict, deck: dict, pc_wired) -> list:
    tips = []
    if not rtt.get("ok"):
        return ["O PC não respondeu. Ele está ligado e na mesma rede?"]
    if deck.get("type") == "wifi":
        if deck.get("band") == "2.4 GHz":
            tips.append("O Deck está no Wi-Fi 2,4 GHz: troque para a rede 5 GHz (ou 6 GHz). É o que mais reduz o atraso.")
        if deck.get("signal", 100) < 50:
            tips.append(f"Sinal do Wi-Fi fraco ({deck['signal']}%): chegue mais perto do roteador.")
    if pc_wired is False:
        tips.append("O PC está no Wi-Fi: ligar o PC no cabo deixa tudo mais estável.")
    if rtt["lost"] > 0:
        tips.append(f"{rtt['lost']} de {rtt['count']} tentativas se perderam: a rede está congestionada ou instável.")
    if rtt["avg"] > 8 or rtt["jitter"] > 4:
        tips.append("Rede lenta ou oscilando. Use o modo \"Baixa latência\" e reduza o bitrate.")
    if not tips:
        tips.append("Rede ótima para streaming. 👍")
    return tips


# ------------------------------------------------------------------ plugin

class Plugin:
    # ---------------- settings
    async def get_settings(self):
        s = _load()
        hosts = [{"id": k, "ip": v["ip"], "port": v.get("port", API_PORT), "name": v.get("name", k),
                  "paired": bool(v.get("key")), "mac": v.get("mac") or "", "wired": v.get("wired"),
                  "capability": v.get("capability", "")} for k, v in s["hosts"].items()]
        out = {k: s[k] for k in ("resolution", "fps", "bitrate", "preset", "codec", "overlay", "audio_on_host",
                                 "absolute_touch", "gyro", "shortcut_app_id", "active")}
        out["hosts"] = hosts
        out["host"] = next((h for h in hosts if h["id"] == s["active"]), None)
        return out

    async def save_settings(self, patch: dict):
        s = _load()
        if patch.get("resolution") in RESOLUTIONS:
            s["resolution"] = patch["resolution"]
        if "fps" in patch and int(patch["fps"]) in (30, 40, 45, 60, 90, 120):
            s["fps"] = int(patch["fps"])
        if "bitrate" in patch:
            s["bitrate"] = max(2000, min(150000, int(patch["bitrate"])))
        if patch.get("preset") in PRESETS:
            s["preset"] = patch["preset"]
        if patch.get("codec") in CODECS:
            s["codec"] = patch["codec"]
        for k in ("audio_on_host", "absolute_touch", "overlay", "gyro"):
            if k in patch:
                s[k] = bool(patch[k])
        if "shortcut_app_id" in patch:
            s["shortcut_app_id"] = int(patch["shortcut_app_id"] or 0)
        _save(s)
        return await self.get_settings()

    # ---------------- PCs
    async def discover(self):
        return await asyncio.to_thread(_discover)

    async def add_host(self, ip: str, port: int = API_PORT):
        ip = (ip or "").strip()
        if not ip:
            return {"ok": False, "error": "Digite o IP do PC"}
        probe = {"ip": ip, "port": int(port)}
        res = await asyncio.to_thread(_http, "GET", probe, "/api/hello")
        d = res.get("data") if res["ok"] else None
        if not isinstance(d, dict) or d.get("app") != "deckscreen":
            return {"ok": False, "error": res.get("error") or "Não é um PC com DeckScreen"}
        if d.get("auth") != "hmac-v1":
            return {"ok": False, "error": "Atualize o DeckScreen no PC (versão antiga)"}
        s = _load()
        hid = d.get("name") or ip
        old = s["hosts"].get(hid, {})
        s["hosts"][hid] = {**old, "ip": ip, "port": int(port), "name": d.get("name", ip),
                           "capability": d.get("capability", "")}
        s["active"] = hid
        _save(s)
        return {"ok": True, "id": hid}

    async def select_host(self, host_id: str):
        s = _load()
        if host_id in s["hosts"]:
            s["active"] = host_id
            _save(s)
            return True
        return False

    async def forget_host(self, host_id: str = ""):
        s = _load()
        hid = host_id or s["active"]
        h = s["hosts"].get(hid)
        if h and h.get("key"):
            await asyncio.to_thread(_http, "POST", h, "/api/unpair", {}, h["key"], s["device_id"])
        s["hosts"].pop(hid, None)
        s["active"] = next(iter(s["hosts"]), "")
        _save(s)
        return True

    async def pair_start(self):
        s = _load()
        h = _active(s)
        if not h:
            return {"ok": False, "error": "Escolha um PC primeiro"}
        pin = f"{random.SystemRandom().randint(0, 9999):04d}"
        res = await asyncio.to_thread(
            _http, "POST", h, "/api/pair/request",
            {"device_id": s["device_id"], "device_name": socket.gethostname() or "Steam Deck", "pin": pin})
        if not res["ok"]:
            return {"ok": False, "error": res["error"]}
        return {"ok": True, "pin": pin, "request_id": res["data"]["request_id"],
                "launch_options": self._moonlight_cmd(["pair", h["ip"], "--pin", pin])}

    async def pair_status(self, request_id: str):
        s = _load()
        h = _active(s)
        if not h:
            return {"ok": False, "error": "Sem PC"}
        res = await asyncio.to_thread(_http, "GET", h, f"/api/pair/status?id={request_id}")
        if not res["ok"]:
            return {"ok": False, "error": res["error"]}
        d = res["data"]
        if d.get("state") == "approved" and d.get("key"):
            h["key"] = d["key"]
            _save(s)
        return {"ok": True, "state": d.get("state"), "sunshine": d.get("sunshine")}

    # ---------------- host API (signed)
    async def _authed(self, method: str, path: str, body=None, timeout: float = 5):
        s = _load()
        h = _active(s)
        if not h or not h.get("key"):
            return {"ok": False, "code": 401, "error": "Não pareado"}
        res = await asyncio.to_thread(_http, method, h, path, body, h["key"], s["device_id"], timeout)
        if res.get("code") == 401 and "relógio" not in (res.get("error") or ""):
            h.pop("key", None)
            _save(s)
            res["error"] = "O PC não reconhece mais este Deck. Pareie de novo."
        return res

    async def host_status(self):
        res = await self._authed("GET", "/api/status")
        if res.get("ok"):
            net = (res["data"] or {}).get("network") or {}
            s = _load()
            h = _active(s)
            if h is not None:
                changed = False
                for k, v in (("mac", net.get("mac")), ("wired", net.get("wired")),
                             ("capability", res["data"].get("capability"))):
                    if v is not None and h.get(k) != v:
                        h[k] = v
                        changed = True
                if changed:
                    _save(s)
        return res

    async def host_update_settings(self, patch: dict):
        return await self._authed("PUT", "/api/settings", patch)

    async def host_stop(self):
        return await self._authed("POST", "/api/stop", {})

    async def host_doctor(self):
        return await self._authed("GET", "/api/doctor", timeout=20)

    # ---------------- Wake-on-LAN / test
    async def wake_host(self, wait_seconds: int = 90):
        s = _load()
        h = _active(s)
        if not h or not h.get("mac"):
            return {"ok": False, "error": "MAC do PC desconhecido: conecte uma vez com o PC ligado (e no cabo)."}
        await asyncio.to_thread(_send_wol, h["mac"], h["ip"])
        deadline = time.monotonic() + wait_seconds
        while time.monotonic() < deadline:
            res = await asyncio.to_thread(_http, "GET", h, "/api/hello", None, "", "", 2)
            if res["ok"]:
                return {"ok": True}
            await asyncio.sleep(2)
            await asyncio.to_thread(_send_wol, h["mac"], h["ip"])
        return {"ok": False, "error": "O PC não acordou. Confira o Wake-on-LAN na BIOS e se ele estava suspenso (não desligado)."}

    async def test_connection(self):
        s = _load()
        h = _active(s)
        if not h:
            return {"ok": False, "error": "Escolha um PC primeiro"}
        rtt, deck = await asyncio.gather(asyncio.to_thread(tcp_rtt, h["ip"], int(h.get("port", API_PORT))),
                                         asyncio.to_thread(deck_network))
        return {"ok": True, "rtt": rtt, "deck": deck, "pc_wired": h.get("wired"),
                "tips": verdict(rtt, deck, h.get("wired"))}

    # ---------------- Moonlight
    def _moonlight_cmd(self, args: list) -> str:
        return " ".join(["run", MOONLIGHT_ID] + [shlex.quote(str(a)) for a in args])

    async def stream_launch_options(self):
        s = _load()
        h = _active(s)
        if not h:
            return ""
        args = ["stream", h["ip"], SUNSHINE_APP,
                "--resolution", s["resolution"], "--fps", s["fps"], "--bitrate", s["bitrate"],
                "--display-mode", "fullscreen", "--quit-after", "--no-game-optimization",
                "--video-codec", s["codec"],
                "--audio-on-host" if s["audio_on_host"] else "--no-audio-on-host",
                "--performance-overlay" if s["overlay"] else "--no-performance-overlay"]
        args += {"low_latency": ["--no-vsync", "--no-frame-pacing"],
                 "balanced": ["--vsync", "--no-frame-pacing"],
                 "quality": ["--vsync", "--frame-pacing"]}[s["preset"]]
        if s["absolute_touch"]:
            args += ["--absolute-mouse", "--no-touchscreen-trackpad"]
        else:
            args += ["--no-absolute-mouse", "--touchscreen-trackpad"]
        return self._moonlight_cmd(args)

    async def moonlight_status(self):
        def check():
            try:
                p = _run(["flatpak", "info", MOONLIGHT_ID], timeout=20)
                return {"installed": p.returncode == 0}
            except (OSError, subprocess.TimeoutExpired) as e:
                return {"installed": False, "error": str(e)}
        return await asyncio.to_thread(check)

    async def install_moonlight(self):
        def install():
            steps = [
                ["flatpak", "remote-add", "--user", "--if-not-exists", "flathub", FLATHUB],
                ["flatpak", "install", "--user", "-y", "--noninteractive", "flathub", MOONLIGHT_ID],
            ]
            for cmd in steps:
                try:
                    p = _run(cmd, timeout=900)
                except (OSError, subprocess.TimeoutExpired) as e:
                    return {"ok": False, "error": str(e)}
                if p.returncode != 0:
                    decky.logger.error("%s -> %s", cmd, p.stderr)
                    return {"ok": False, "error": (p.stderr or p.stdout).strip()[-300:]}
            return {"ok": True}
        return await asyncio.to_thread(install)

    # ---------------- lifecycle
    async def _main(self):
        _load()
        decky.logger.info("DeckScreen loaded")

    async def _unload(self):
        pass

    async def _uninstall(self):
        pass
