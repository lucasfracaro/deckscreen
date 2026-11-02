"""
Steam Input layouts for the DeckScreen shortcut.

Steam's default for a non-Steam shortcut is "Gamepad with Joystick Trackpad": the
trackpads act as sticks, so on the PC desktop nothing clicks. DeckScreen installs
two layouts as Steam templates (controller_base/templates, offered to every game)
and selects one for the shortcut:

  desktop - like the Deck's own desktop mode: right trackpad = mouse (click = left
            click), left trackpad = scroll wheel, R2 = left click, L2 = right click;
            buttons and sticks stay a gamepad.
  game    - gamepad with mouse trackpad: analog triggers, right trackpad = mouse
            (click = left click, i.e. a tap on Cemu's GamePad screen), left trackpad = D-pad.

The structure mirrors Steam's own neptune templates (gamepad+mouse, webbrowser).
"""

import os

PREFIX = "controller_neptune_deckscreen_"
KINDS = ("desktop", "game")
TITLES = {
    "desktop": ("DeckScreen: Área de trabalho",
                "Trackpad direito = mouse (clique = botão esquerdo), trackpad esquerdo = rolagem, "
                "R2 = clique esquerdo, L2 = clique direito. Botões e analógicos continuam como controle."),
    "game": ("DeckScreen: Jogo",
             "Controle completo com gatilhos analógicos. Trackpad direito = mouse "
             "(clique = botão esquerdo, toque na tela do GamePad no Cemu)."),
}


def file_name(kind: str) -> str:
    return f"{PREFIX}{kind}.vdf"


def url(kind: str) -> str:
    return f"template://{file_name(kind)}"


def kind_for_profile(profile: str) -> str:
    return "desktop" if profile in ("", "desktop") else "game"


# ------------------------------------------------------------------ KeyValues writer

def _q(s) -> str:
    return '"' + str(s).replace("\\", "\\\\").replace('"', '\\"') + '"'


def _kv(items, depth=0) -> str:
    """items: list of (key, value) where value is str or a nested list (keys may repeat)."""
    pad = "\t" * depth
    out = []
    for k, v in items:
        if isinstance(v, list):
            out.append(f"{pad}{_q(k)}\n{pad}{{\n{_kv(v, depth + 1)}{pad}}}\n")
        else:
            out.append(f"{pad}{_q(k)}\t\t{_q(v)}\n")
    return "".join(out)


def _press(binding: str, activator: str = "Full_Press"):
    return [("activators", [(activator, [("bindings", [("binding", binding)])])])]


def _group(gid: int, mode: str, inputs, settings=None):
    g = [("id", str(gid)), ("mode", mode), ("inputs", inputs)]
    if settings:
        g.append(("settings", settings))
    return ("group", g)


def render(kind: str) -> str:
    if kind not in KINDS:
        raise ValueError(kind)
    title, desc = TITLES[kind]
    groups = [
        _group(0, "four_buttons", [(f"button_{b.lower()}", _press(f"xinput_button {b}")) for b in "ABXY"]),
        _group(1, "dpad", [(f"dpad_{d}", _press(f"xinput_button dpad_{x}"))
                           for d, x in (("north", "up"), ("south", "down"), ("east", "right"), ("west", "left"))]),
        _group(2, "joystick_move", [("click", _press("xinput_button JOYSTICK_LEFT"))]),
        _group(3, "joystick_move", [("click", _press("xinput_button JOYSTICK_RIGHT"))]),
        _group(6, "absolute_mouse", [("click", _press("mouse_button LEFT"))]),
        _group(7, "switches", [
            ("button_escape", _press("xinput_button start")),
            ("button_menu", _press("xinput_button select")),
            ("left_bumper", _press("xinput_button shoulder_left")),
            ("right_bumper", _press("xinput_button shoulder_right")),
        ]),
    ]
    if kind == "desktop":
        groups += [
            _group(4, "trigger", [("edge", _press("mouse_button RIGHT"))]),
            _group(5, "trigger", [("edge", _press("mouse_button LEFT"))]),
            _group(8, "scrollwheel", [
                ("scroll_clockwise", _press("mouse_wheel SCROLL_UP")),
                ("scroll_counterclockwise", _press("mouse_wheel SCROLL_DOWN")),
            ], [("scroll_angle", "137"), ("scroll_type", "2"), ("scroll_invert", "1"), ("scroll_friction", "1")]),
        ]
    else:
        groups += [
            _group(4, "trigger", [], [("output_trigger", "1")]),
            _group(5, "trigger", [], [("output_trigger", "2")]),
            _group(8, "dpad", [(f"dpad_{d}", _press(f"xinput_button dpad_{x}"))
                               for d, x in (("north", "up"), ("south", "down"), ("east", "right"), ("west", "left"))]),
        ]
    preset = ("preset", [
        ("id", "0"),
        ("name", "Default"),
        ("group_source_bindings", [
            ("0", "button_diamond active"),
            ("1", "dpad active"),
            ("2", "joystick active"),
            ("3", "right_joystick active"),
            ("4", "left_trigger active"),
            ("5", "right_trigger active"),
            ("6", "right_trackpad active"),
            ("7", "switch active"),
            ("8", "left_trackpad active"),
        ]),
    ])
    doc = [("controller_mappings", [
        ("version", "3"),
        ("title", title),
        ("description", desc),
        ("controller_type", "controller_neptune"),
        *groups,
        preset,
        ("settings", [("left_trackpad_mode", "0"), ("right_trackpad_mode", "0")]),
    ])]
    return _kv(doc)


def steam_roots(home: str):
    for rel in (".local/share/Steam", ".steam/steam", ".steam/root"):
        p = os.path.join(home, rel)
        if os.path.isdir(os.path.join(p, "controller_base")):
            yield os.path.realpath(p)


def install(home: str) -> list:
    """Write both templates into every Steam install found; returns the files written/kept."""
    done = []
    for root in dict.fromkeys(steam_roots(home)):
        d = os.path.join(root, "controller_base", "templates")
        os.makedirs(d, exist_ok=True)
        for kind in KINDS:
            path = os.path.join(d, file_name(kind))
            text = render(kind)
            try:
                with open(path, encoding="utf-8") as f:
                    if f.read() == text:
                        done.append(path)
                        continue
            except OSError:
                pass
            tmp = path + ".tmp"
            with open(tmp, "w", encoding="utf-8") as f:
                f.write(text)
            os.replace(tmp, path)
            done.append(path)
    return done
