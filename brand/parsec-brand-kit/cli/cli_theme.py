"""parsec — CLI theme (Python, truecolor)

Zero-dependency ANSI helpers matching the brand.
Respects NO_COLOR and non-TTY output.

    from cli_theme import p
    print(p.banner())
    print(p.ok("installed"), p.dim("(2.1s)"))
    print(p.err("failed"), p.link("https://getparsec.ai"))
"""
import os
import sys

HEX = {
    "phosphor": "4AF626", "mint": "7CFFB2", "dim": "2E7D46",
    "text": "D7FBE4", "muted": "8CA897", "faint": "556D60",
    "success": "4AF626", "warning": "FFB84D", "error": "FF5C57",
    "info": "4AD0E0", "magenta": "FF5FD2", "bg": "0A0E0C",
}

_ENABLED = (
    not os.environ.get("NO_COLOR")
    and os.environ.get("TERM") != "dumb"
    and sys.stdout.isatty()
)


def _rgb(h):
    return int(h[0:2], 16), int(h[2:4], 16), int(h[4:6], 16)


def _fg(h, s):
    if not _ENABLED:
        return str(s)
    r, g, b = _rgb(h)
    return f"\x1b[38;2;{r};{g};{b}m{s}\x1b[0m"


def _style(codes, s):
    return f"\x1b[{codes}m{s}\x1b[0m" if _ENABLED else str(s)


class _Color:
    def __getattr__(self, name):
        if name in HEX:
            return lambda s: _fg(HEX[name], s)
        raise AttributeError(name)


class Parsec:
    color = _Color()
    SPINNER = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"

    def bold(self, s):       return _style("1", s)
    def underline(self, s):  return _style("4", s)

    def prompt(self, s=""):  return f"{self.color.phosphor(self.bold('❯'))} {s}"
    def ok(self, s):         return f"{self.color.success('✓')} {self.color.text(s)}"
    def warn(self, s):       return f"{self.color.warning('⚠')} {self.color.text(s)}"
    def err(self, s):        return f"{self.color.error('✗')} {self.color.text(s)}"
    def info(self, s):       return f"{self.color.info('ℹ')} {self.color.text(s)}"
    def step(self, s):       return f"{self.color.dim('•')} {self.color.muted(s)}"
    def accent(self, s):     return self.color.phosphor(self.bold(s))
    def dim(self, s):        return self.color.faint(s)
    def link(self, s):       return self.underline(self.color.info(s))

    def spinner(self, label, i):
        return f"{self.color.phosphor(self.SPINNER[i % 10])} {self.color.muted(label)}"

    def bar(self, frac, width=24):
        n = round(max(0.0, min(1.0, frac)) * width)
        return self.color.phosphor("█" * n) + self.color.faint("─" * (width - n))

    def mark(self):
        return self.color.phosphor(self.bold("⟩✦"))

    def banner(self):
        g = self.color.phosphor
        lines = [
            g("  ╲"),
            g("   ╲"),
            g("    ") + g(self.bold("✦")) + "  " + self.bold(self.color.text("parsec")),
            g("   ╱"),
            g("  ╱"),
        ]
        return "\n".join(lines) + "\n  " + self.color.muted("2× context · ½ cost") + "\n"


p = Parsec()

if __name__ == "__main__":
    print(p.banner())
    print(p.ok("context doubled"), p.dim("(cache warm)"))
    print(p.warn("token budget at 80%"))
    print(p.err("provider timeout"), p.link("https://getparsec.ai/docs"))
    print(p.prompt("run 'parsec optimize'"))
    print(p.bar(0.62), "62%")
