"""Parsec terminal helpers using ../tokens/tokens.json; no third-party dependencies.

Monochrome text, green progress graphic. Respects NO_COLOR, TERM=dumb and non-TTY.
    from cli_theme import p
    print(p.banner())
    print(p.ok("Ready"), p.dim("(cache warm)"))
"""

import json
import math
import os
import sys
from pathlib import Path

_TOKENS = json.loads((Path(__file__).resolve().parent.parent / "tokens/tokens.json").read_text())
_TERMINAL = _TOKENS["terminal"]
HEX = {
    "text": _TERMINAL["foreground"], "muted": _TERMINAL["muted"],
    "faint": _TERMINAL["muted"], "dim": _TERMINAL["muted"],
    "success": _TERMINAL["foreground"], "warning": _TERMINAL["foreground"],
    "error": _TERMINAL["foreground"], "info": _TERMINAL["foreground"],
    "bg": _TERMINAL["background"],
}
_ENABLED = not os.environ.get("NO_COLOR") and os.environ.get("TERM") != "dumb" and sys.stdout.isatty()


def _fg(hex_value, value):
    if not _ENABLED:
        return str(value)
    h = hex_value.removeprefix("#")
    r, g, b = (int(h[i:i + 2], 16) for i in (0, 2, 4))
    return f"\x1b[38;2;{r};{g};{b}m{value}\x1b[0m"


def _style(codes, value):
    return f"\x1b[{codes}m{value}\x1b[0m" if _ENABLED else str(value)


class _Color:
    def __getattr__(self, name):
        if name in HEX:
            return lambda s: _fg(HEX[name], s)
        raise AttributeError(name)


class Parsec:
    color = _Color()
    SPINNER = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"

    def bold(self, s):
        return _style("1", s)

    def underline(self, s):
        return _style("4", s)

    def prompt(self, s=""):
        return f"{self.color.text('❯')} {s}"

    def ok(self, s):
        return f"{self.color.success('✓')} {self.color.text(s)}"

    def warn(self, s):
        return f"{self.color.warning('⚠')} {self.color.text(s)}"

    def err(self, s):
        return f"{self.color.error('✗')} {self.color.text(s)}"

    def info(self, s):
        return f"{self.color.info('ℹ')} {self.color.text(s)}"

    def step(self, s):
        return f"{self.color.dim('•')} {self.color.muted(s)}"

    def accent(self, s):
        return self.color.text(self.bold(s))

    def dim(self, s):
        return self.color.faint(s)

    def link(self, s):
        return self.underline(self.color.text(s))

    def kbd(self, s):
        return _style("7", f" {s} ")

    def spinner(self, label, i):
        return f"{self.color.text(self.SPINNER[i % len(self.SPINNER)])} {self.color.muted(label)}"

    def bar(self, frac, width=24):
        n = math.floor(max(0.0, min(1.0, frac)) * width + 0.5)
        return _fg(_TERMINAL["progress"], "█" * n) + _fg(_TERMINAL["progressTrack"], "─" * (width - n))

    def mark(self):
        # Use the name in text; use logo/svg/logo.svg on graphical surfaces.
        return self.color.text(_TOKENS["content"]["name"])

    def banner(self):
        return self.mark() + "\n" + self.color.muted(_TOKENS["content"]["tagline"]) + "\n"


p = Parsec()

if __name__ == "__main__":
    print(p.banner())
    print(p.ok("Ready"), p.dim("(cache warm)"))
    print(p.warn("Token budget at 80%"))
    print(p.err("Provider timeout"), p.link("https://getparsec.ai/docs"))
    print(p.prompt("parsec --help"))
    print(p.bar(0.62), "62%")
