"""End-to-end: real Rust daemon + the Decky backend (signed requests, pairing, settings).
Run: python3 tests/integration.py path/to/deckscreen-host"""
import asyncio
import os
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "host", "target", "release", "deckscreen-host"))
tmp = tempfile.mkdtemp(prefix="ds-it-")
env = dict(os.environ, XDG_CONFIG_HOME=f"{tmp}/config", XDG_DATA_HOME=f"{tmp}/data", HOME=f"{tmp}/home")
os.makedirs(env["HOME"])
os.environ["DECKY_TEST_SETTINGS"] = f"{tmp}/decky"
sys.path.insert(0, os.path.join(HERE, "fakedecky"))
sys.path.insert(0, os.path.join(HERE, "..", "deck"))
import main  # noqa: E402


def cli(*args):
    return subprocess.run([BIN, *args], capture_output=True, text=True, env=env)


async def run():
    p = main.Plugin()
    assert (await p.add_host("127.0.0.1"))["ok"]
    r = await p.pair_start()
    assert r["ok"] and len(r["pin"]) == 4, r
    assert r["pin"] in cli("pending").stdout
    assert cli("accept").returncode == 0
    await asyncio.sleep(0.3)
    st = await p.pair_status(r["request_id"])
    assert st["state"] == "approved", st
    assert (await p.get_settings())["host"]["paired"]
    s = await p.host_status()
    assert s["ok"] and s["data"]["version"], s
    u = await p.host_update_settings({"mode": "deck_only", "profile": "cemu", "scale": 1.25})
    assert u["data"]["mode"] == "deck_only" and u["data"]["profile"] == "cemu" and u["data"]["scale"] == 1.25, u
    assert (await p.host_doctor())["ok"]
    t = await p.test_connection()
    assert t["rtt"]["ok"], t
    assert (await p.host_stop())["ok"]
    assert await p.forget_host()
    assert "Nenhum Deck pareado" in cli("devices").stdout
    print("integration: OK")


daemon = subprocess.Popen([BIN, "daemon"], env=env, stderr=subprocess.DEVNULL)
try:
    for _ in range(50):
        if cli("status").returncode == 0:
            break
        time.sleep(0.1)
    asyncio.run(run())
finally:
    daemon.terminate()
    daemon.wait()
