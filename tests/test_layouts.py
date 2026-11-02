"""Tests for deck/py_modules/layouts.py (Steam Input templates). Run: python3 -m unittest discover -s tests"""
import os
import re
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "deck", "py_modules"))
import layouts  # noqa: E402

TOKEN = re.compile(r'"((?:[^"\\]|\\.)*)"|([{}])')


def parse(text):
    """Minimal KeyValues parser -> nested list of (key, value)."""
    toks = [m.group(1) if m.group(1) is not None else m.group(2) for m in TOKEN.finditer(text)]
    pos = 0

    def block():
        nonlocal pos
        items = []
        while pos < len(toks) and toks[pos] != "}":
            key = toks[pos]
            pos += 1
            if toks[pos] == "{":
                pos += 1
                items.append((key, block()))
                assert toks[pos] == "}"
                pos += 1
            else:
                items.append((key, toks[pos]))
                pos += 1
        return items

    out = block()
    assert pos == len(toks), "unbalanced braces"
    return out


def groups(doc):
    root = dict(doc)["controller_mappings"]
    return {dict(g)["id"]: dict(g) for k, g in root if k == "group"}, root


def bindings(group, inp):
    acts = dict(dict(group["inputs"])[inp])["activators"]
    return [v for _, a in acts for _, b in a if True for k, v in b if k == "binding"]


class RenderTests(unittest.TestCase):
    def test_desktop(self):
        g, root = groups(parse(layouts.render("desktop")))
        self.assertEqual(dict(root)["controller_type"], "controller_neptune")
        self.assertEqual(bindings(g["5"], "edge"), ["mouse_button LEFT"])   # R2 = left click
        self.assertEqual(bindings(g["4"], "edge"), ["mouse_button RIGHT"])  # L2 = right click
        self.assertEqual(bindings(g["6"], "click"), ["mouse_button LEFT"])  # right pad click
        self.assertEqual(g["8"]["mode"], "scrollwheel")
        self.assertEqual(bindings(g["0"], "button_a"), ["xinput_button A"])
        preset = dict(dict(root)["preset"])["group_source_bindings"]
        self.assertIn(("6", "right_trackpad active"), preset)
        self.assertEqual(sorted(k for k, _ in preset), sorted(g))          # every group is bound

    def test_game(self):
        g, _ = groups(parse(layouts.render("game")))
        self.assertEqual(dict(g["5"]["settings"])["output_trigger"], "2")
        self.assertEqual(dict(g["5"]["inputs"]), {})
        self.assertEqual(g["8"]["mode"], "dpad")
        self.assertEqual(bindings(g["6"], "click"), ["mouse_button LEFT"])

    def test_profile_and_url(self):
        self.assertEqual(layouts.kind_for_profile("desktop"), "desktop")
        self.assertEqual(layouts.kind_for_profile("cemu"), "game")
        self.assertEqual(layouts.url("game"), "template://controller_neptune_deckscreen_game.vdf")
        with self.assertRaises(ValueError):
            layouts.render("x")


class InstallTests(unittest.TestCase):
    def test_install_into_steam_templates(self):
        with tempfile.TemporaryDirectory() as home:
            self.assertEqual(layouts.install(home), [])                     # no Steam: nothing
            os.makedirs(os.path.join(home, ".local/share/Steam/controller_base"))
            os.makedirs(os.path.join(home, ".steam"))
            os.symlink(os.path.join(home, ".local/share/Steam"), os.path.join(home, ".steam/steam"))
            files = layouts.install(home)
            self.assertEqual(len(files), 2)                                  # symlinked root counted once
            for f in files:
                with open(f, encoding="utf-8") as fh:
                    parse(fh.read())
            self.assertEqual(layouts.install(home), files)                   # idempotent


if __name__ == "__main__":
    unittest.main()
