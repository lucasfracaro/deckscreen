"""Tests for host/assets/gnome-display.py with a fake PyGObject (no GNOME needed)."""
import importlib.util
import io
import json
import os
import sys
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
HELPER = os.path.join(HERE, "..", "host", "assets", "gnome-display.py")

STATE = (
    7,
    [  # monitors: ((connector, vendor, product, serial), modes, props)
        (("DP-1", "DEL", "U2720Q", "123"),
         [("2560x1440@59.951", 2560, 1440, 59.951, 1.25, [1.0, 1.25, 1.5], {"is-current": True, "is-preferred": True}),
          ("1920x1080@60.000", 1920, 1080, 60.0, 1.0, [1.0, 2.0], {})],
         {"display-name": "Dell 27\""}),
        (("Meta-0", "MetaVendor", "Virtual remote monitor", "0x000002"),
         [("1280x800@60.000", 1280, 800, 60.0, 1.0, [1.0, 1.25], {"is-current": True})],
         {}),
    ],
    [(0, 0, 1.25, 0, True, [("DP-1", "DEL", "U2720Q", "123")], {}),
     (2048, 0, 1.0, 0, False, [("Meta-0", "MetaVendor", "Virtual remote monitor", "0x000002")], {})],
    {"layout-mode": 1},
)


class FakeVariant:
    def __init__(self, sig, value):
        self.sig, self.value = sig, value

    def unpack(self):
        return self.value


class FakeBus:
    def __init__(self):
        self.calls = []

    def call_sync(self, name, path, iface, method, params, *_):
        self.calls.append((method, params))
        if method == "GetCurrentState":
            return FakeVariant("", STATE)
        return FakeVariant("", ())


def load_helper(bus):
    gi = types.ModuleType("gi")
    repo = types.ModuleType("gi.repository")

    class GLibError(Exception):
        message = "boom"

    repo.Gio = types.SimpleNamespace(bus_get_sync=lambda *a: bus, BusType=types.SimpleNamespace(SESSION=0),
                                     DBusCallFlags=types.SimpleNamespace(NONE=0))
    repo.GLib = types.SimpleNamespace(Variant=FakeVariant, Error=GLibError)
    gi.repository = repo
    sys.modules["gi"], sys.modules["gi.repository"] = gi, repo
    spec = importlib.util.spec_from_file_location("gnome_display", HELPER)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class HelperTests(unittest.TestCase):
    def test_state_json(self):
        bus = FakeBus()
        mod = load_helper(bus)
        st = mod.state(bus)
        self.assertEqual(st["serial"], 7)
        self.assertEqual(st["layout_mode"], 1)
        dp = st["monitors"][0]
        self.assertEqual((dp["connector"], dp["current_mode"], dp["preferred_mode"]), ("DP-1", "2560x1440@59.951", "2560x1440@59.951"))
        self.assertEqual(dp["modes"][0]["scales"], [1.0, 1.25, 1.5])
        self.assertEqual(st["monitors"][1]["vendor"], "MetaVendor")
        self.assertEqual(st["logical"][1], {"x": 2048, "y": 0, "scale": 1.0, "transform": 0, "primary": False, "monitors": ["Meta-0"]})
        json.dumps(st)  # serializable

    def test_apply_builds_variant(self):
        bus = FakeBus()
        mod = load_helper(bus)
        mod.apply(bus, {"logical": [{"x": 0, "y": 0, "scale": 1.25, "transform": 0, "primary": True,
                                     "monitors": [{"connector": "DP-1", "mode": "2560x1440@59.951"}]},
                                    {"x": 2048, "y": 0, "scale": 1, "transform": 1, "primary": False,
                                     "monitors": [{"connector": "Meta-0", "mode": "1280x800@60.000"}]}]})
        method, params = bus.calls[-1]
        self.assertEqual(method, "ApplyMonitorsConfig")
        self.assertEqual(params.sig, "(uua(iiduba(ssa{sv}))a{sv})")
        serial, how, lms, props = params.value
        self.assertEqual((serial, how, props), (7, 1, {}))
        self.assertEqual(lms[1], (2048, 0, 1.0, 1, False, [("Meta-0", "1280x800@60.000", {})]))

    def test_cli(self):
        bus = FakeBus()
        mod = load_helper(bus)
        out = io.StringIO()
        sys.argv, old = ["gnome-display.py", "state"], sys.stdout
        sys.stdout = out
        try:
            self.assertEqual(mod.main(), 0)
        finally:
            sys.stdout = old
        self.assertEqual(json.loads(out.getvalue())["serial"], 7)


if __name__ == "__main__":
    unittest.main()
