#!/usr/bin/env python3
"""DeckScreen helper for GNOME (Mutter): read and apply the monitor layout.

Embedded in deckscreen-host and run with the system python3 (needs PyGObject,
which every GNOME desktop ships). It only talks D-Bus; the layout logic lives in
the Rust code so it can be tested.

  gnome-display.py state            -> prints the current state as JSON
  gnome-display.py apply < cfg.json -> applies {"logical": [...]} temporarily

cfg.json:
  {"logical": [{"x": 0, "y": 0, "scale": 1.0, "transform": 0, "primary": true,
                "monitors": [{"connector": "DP-1", "mode": "1920x1080@60.000"}]}]}
"""
import json
import sys

from gi.repository import Gio, GLib

BUS_NAME = "org.gnome.Mutter.DisplayConfig"
OBJ_PATH = "/org/gnome/Mutter/DisplayConfig"
METHOD_TEMPORARY = 1


def call(bus, method, params=None):
    return bus.call_sync(BUS_NAME, OBJ_PATH, BUS_NAME, method, params, None, Gio.DBusCallFlags.NONE, 10000, None)


def state(bus):
    serial, monitors, logical, props = call(bus, "GetCurrentState").unpack()
    out_monitors = []
    for spec, modes, mprops in monitors:
        connector, vendor, product, mserial = spec
        out_modes = []
        current = preferred = None
        for mid, w, h, refresh, pref_scale, scales, mode_props in modes:
            if mode_props.get("is-current"):
                current = mid
            if mode_props.get("is-preferred"):
                preferred = mid
            out_modes.append({"id": mid, "width": w, "height": h, "refresh": refresh,
                              "preferred_scale": pref_scale, "scales": list(scales)})
        out_monitors.append({"connector": connector, "vendor": vendor, "product": product, "serial": mserial,
                             "display_name": mprops.get("display-name", connector),
                             "current_mode": current, "preferred_mode": preferred, "modes": out_modes})
    out_logical = []
    for x, y, scale, transform, primary, specs, _lprops in logical:
        out_logical.append({"x": x, "y": y, "scale": scale, "transform": transform, "primary": bool(primary),
                            "monitors": [s[0] for s in specs]})
    return {"serial": serial, "layout_mode": int(props.get("layout-mode", 1)),
            "monitors": out_monitors, "logical": out_logical}


def apply(bus, cfg):
    serial = call(bus, "GetCurrentState").unpack()[0]
    lms = []
    for lm in cfg["logical"]:
        mons = [(m["connector"], m["mode"], {}) for m in lm["monitors"]]
        lms.append((int(lm["x"]), int(lm["y"]), float(lm["scale"]), int(lm["transform"]), bool(lm["primary"]), mons))
    params = GLib.Variant("(uua(iiduba(ssa{sv}))a{sv})", (serial, METHOD_TEMPORARY, lms, {}))
    call(bus, "ApplyMonitorsConfig", params)


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("state", "apply"):
        print(__doc__, file=sys.stderr)
        return 2
    try:
        bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        if sys.argv[1] == "state":
            print(json.dumps(state(bus)))
        else:
            apply(bus, json.load(sys.stdin))
            print('{"ok": true}')
        return 0
    except GLib.Error as e:
        print(json.dumps({"error": e.message}))
        return 1


if __name__ == "__main__":
    sys.exit(main())
