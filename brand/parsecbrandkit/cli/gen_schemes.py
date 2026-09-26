"""Generate terminal presets from ../tokens/tokens.json, from any directory."""

import json
import plistlib
from pathlib import Path

HERE = Path(__file__).resolve().parent
ANSI_NAMES = ["Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "White"]


def color_dict(h):
    h = h.removeprefix("#")
    r, g, b = (int(h[i:i + 2], 16) / 255 for i in (0, 2, 4))
    return {"Color Space": "sRGB", "Red Component": r, "Green Component": g,
            "Blue Component": b, "Alpha Component": 1.0}


def write_json(name, data):
    (HERE / name).write_text(json.dumps(data, indent=2) + "\n")


def generate():
    palette = json.loads((HERE.parent / "tokens/tokens.json").read_text())["terminal"]
    ansi = palette["ansi"]
    write_json("terminal-palette.json", palette)

    iterm = {f"Ansi {i} Color": color_dict(ansi[str(i)]) for i in range(16)}
    for key, role in {
        "Background Color": "background", "Foreground Color": "foreground",
        "Bold Color": "foreground", "Cursor Color": "cursor", "Cursor Text Color": "cursorText",
        "Selection Color": "selection", "Selected Text Color": "selectionText", "Link Color": "foreground",
    }.items():
        iterm[key] = color_dict(palette[role])
    (HERE / "parsec.itermcolors").write_bytes(plistlib.dumps(iterm, sort_keys=False))

    windows = {
        "name": palette["name"], "background": palette["background"],
        "foreground": palette["foreground"], "cursorColor": palette["cursor"],
        "selectionBackground": palette["selection"],
    }
    vscode = {
        "terminal.background": palette["background"], "terminal.foreground": palette["foreground"],
        "terminalCursor.foreground": palette["cursor"], "terminalCursor.background": palette["cursorText"],
        "terminal.selectionBackground": palette["selection"], "terminal.selectionForeground": palette["selectionText"],
    }
    for i in range(16):
        name = ANSI_NAMES[i % 8]
        win_name = "Purple" if name == "Magenta" else name
        win_key = "bright" + win_name if i >= 8 else win_name.lower()
        windows[win_key] = ansi[str(i)]
        vscode["terminal.ansi" + ("Bright" if i >= 8 else "") + name] = ansi[str(i)]
    write_json("windows-terminal.json", windows)
    write_json("vscode-terminal.json", {"workbench.colorCustomizations": vscode})


if __name__ == "__main__":
    generate()
    print("Generated terminal-palette.json, parsec.itermcolors, windows-terminal.json, vscode-terminal.json")
