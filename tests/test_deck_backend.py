"""Unit tests for deck/main.py (the Decky backend). Run: python3 -m unittest discover -s tests"""
import hashlib
import hmac
import json
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "fakedecky"))
sys.path.insert(0, os.path.join(HERE, "..", "deck"))
import main  # noqa: E402


class SigningTests(unittest.TestCase):
    def test_signature_matches_spec(self):
        key = "00" * 32
        h = main.sign_headers(key, "deck1", "PUT", "/api/settings", b'{"a":1}', ts=1700000000, nonce="ab" * 16)
        msg = f"PUT\n/api/settings\n1700000000\n{'ab' * 16}\n{hashlib.sha256(b'{\"a\":1}').hexdigest()}".encode()
        self.assertEqual(h["X-DS-Sig"], hmac.new(bytes(32), msg, hashlib.sha256).hexdigest())
        self.assertEqual((h["X-DS-Device"], h["X-DS-Time"]), ("deck1", "1700000000"))


class HelperTests(unittest.TestCase):
    def test_magic_packet(self):
        p = main.magic_packet("AA:bb-cc:dd:ee:ff")
        self.assertEqual(len(p), 102)
        self.assertEqual(p[:6], b"\xff" * 6)
        self.assertEqual(p[6:12], bytes.fromhex("aabbccddeeff"))
        with self.assertRaises(ValueError):
            main.magic_packet("zz")

    def test_nmcli_split(self):
        self.assertEqual(main._split_nm(r"yes:My\:Wifi:5180 MHz:80:866 Mbit/s"), ["yes", "My:Wifi", "5180 MHz", "80", "866 Mbit/s"])

    def test_verdict(self):
        good = {"ok": True, "avg": 2.0, "jitter": 0.5, "lost": 0, "count": 20}
        self.assertIn("ótima", main.verdict(good, {"type": "wifi", "band": "5 GHz", "signal": 90}, True)[0])
        tips = " ".join(main.verdict({**good, "avg": 15}, {"type": "wifi", "band": "2.4 GHz", "signal": 30}, False))
        for word in ("2,4 GHz", "fraco", "cabo", "Baixa latência"):
            self.assertIn(word, tips)
        self.assertIn("não respondeu", main.verdict({"ok": False, "lost": 20, "count": 20}, {}, None)[0])


class SettingsTests(unittest.TestCase):
    def setUp(self):
        if os.path.exists(main.SETTINGS_FILE):
            os.remove(main.SETTINGS_FILE)

    def test_v1_migration(self):
        os.makedirs(os.path.dirname(main.SETTINGS_FILE), exist_ok=True)
        with open(main.SETTINGS_FILE, "w") as f:
            json.dump({"host": {"ip": "10.0.0.5", "port": 47555, "name": "pc"}, "token": "old"}, f)
        s = main._load()
        self.assertEqual(s["active"], "pc")
        self.assertEqual(s["hosts"]["pc"]["ip"], "10.0.0.5")
        self.assertNotIn("token", s)
        self.assertEqual(len(s["device_id"]), 32)

    def test_launch_options(self):
        import asyncio
        p = main.Plugin()
        s = main._load()
        s["hosts"]["pc"] = {"ip": "10.0.0.5", "port": 47555, "name": "pc"}
        s["active"] = "pc"
        main._save(s)
        asyncio.run(p.save_settings({"preset": "balanced", "codec": "HEVC", "overlay": True, "fps": 90, "bitrate": 1}))
        opts = asyncio.run(p.stream_launch_options())
        for part in ("stream 10.0.0.5 DeckScreen", "--fps 90", "--bitrate 2000", "--video-codec HEVC",
                     "--performance-overlay", "--vsync --no-frame-pacing", "--absolute-mouse"):
            self.assertIn(part, opts)


if __name__ == "__main__":
    unittest.main()


class MoonlightPairedTests(unittest.TestCase):
    CONF = (
        "[General]\nlatestsupportedversion-v1=1\n\n[hosts]\n"
        "1\\hostname=nitro\n1\\localaddress=192.168.5.105\n1\\srvcert=@ByteArray(-----BEGIN CERTIFICATE-----x)\n"
        "2\\hostname=old\n2\\manualaddress=10.0.0.2:47989\n2\\srvcert=@ByteArray()\nsize=2\n"
    )

    def test_parse_and_match(self):
        import tempfile
        hosts = main.moonlight_hosts(self.CONF)
        self.assertEqual([h["paired"] for h in hosts], [True, False])
        with tempfile.TemporaryDirectory() as home:
            self.assertFalse(main.moonlight_paired("192.168.5.105", "nitro", home))   # no Moonlight config yet
            path = os.path.join(home, main.MOONLIGHT_CONF)
            os.makedirs(os.path.dirname(path))
            with open(path, "w") as f:
                f.write(self.CONF)
            self.assertTrue(main.moonlight_paired("192.168.5.105", "", home))
            self.assertTrue(main.moonlight_paired("192.168.5.200", "NITRO", home))    # new IP, same name
            self.assertFalse(main.moonlight_paired("10.0.0.2", "old", home))          # known but not paired
