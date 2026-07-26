# parsec — brand & build kit

Everything needed to build the app and theme the CLI. Retro-terminal identity:
phosphor green on a near-black void, monospace everywhere, one glowing star.

```
  ╲
   ╲
    ✦  parsec        2× context · ½ cost
   ╱
  ╱
```

---

## 1. Logo

The mark is the parsec **parallax angle**: two sightlines converging on a star.
Symmetric about the horizontal axis. No wordmark baked into the mark.

Files in `logo/`:

| File | Use |
|---|---|
| `svg/parsec-mark-flat.svg` | Master mark, one flat color, transparent |
| `svg/parsec-mark-glow.svg` | Mark with phosphor glow (screen/hero) |
| `svg/parsec-icon-512.svg` | Rounded app-icon tile (dark) |
| `svg/parsec-favicon-32.svg`, `-16.svg` | Thicker strokes for tiny sizes |
| `png/parsec-icon-1024.png`, `-512.png` | App icon / marketplace raster |
| `png/parsec-favicon-64/32/16.png` | Browser tab / favicon raster |
| `png/parsec-mark-glow-dark.png` | Hero look on the void |
| `png/parsec-mark-glow.png`, `mark-flat.png` | Transparent marks |

**Rules:** prefer SVG. Keep the star centered on the axis (symmetric top↔bottom).
Clear space ≥ one sightline gap. Never rotate, stretch, or recolor per-campaign.
On light backgrounds use the green `#2FA317`, not the neon phosphor.

---

## 2. Color

Dark is the default theme. Full machine-readable set in `tokens/tokens.json`,
`tokens/theme.css`, and `tokens/tailwind.preset.js`.

### Brand
| Token | Hex | Role |
|---|---|---|
| phosphor | `#4AF626` | Primary / accent (on dark) |
| phosphor-hover | `#64F846` | Hover |
| phosphor-press | `#38C41B` | Active |
| on-phosphor | `#06210B` | Text on a green fill |
| mint | `#7CFFB2` | Bright highlight |
| dim | `#2E7D46` | Muted green / strong border |
| green-light | `#2FA317` | Brand green **on light backgrounds** |

### Surfaces & text
| Token | Hex |
|---|---|
| bg / void | `#0A0E0C` |
| surface | `#0F1512` |
| elevated | `#131B17` |
| overlay | `#16201B` |
| border | `#1D2A22` |
| border-strong | `#2E7D46` |
| text | `#D7FBE4` |
| text-muted | `#8CA897` |
| text-faint | `#556D60` |

### Status
| Token | Hex |
|---|---|
| success | `#4AF626` |
| warning | `#FFB84D` |
| error | `#FF5C57` |
| info / link | `#4AD0E0` |
| magenta (special) | `#FF5FD2` |

---

## 3. Typography

- **Display / UI / code:** JetBrains Mono (400/500/700/800). The identity is monospace.
- **Long-form body (optional):** IBM Plex Sans, or stay all-mono for max terminal feel.
- **Numerals / metrics:** lean on `2×`, `÷2`, `1 pc` in phosphor — the stat is the story.
- Fonts are free (OFL). Fallback stack is in `theme.css`.

Scale (px): 12 · 13 · 15(base) · 16 · 20 · 28 · 40 · 56.

---

## 4. Voice

Terse, benefit-first, engineer-to-engineer. Lead with the number; let the metric
do the selling. Primary line: **"2× the context. ½ the cost."**
Alternates: "Go further per token." · "Same model. Longer reach."

---

## 5. CLI theming

Files in `cli/`:

| File | Use |
|---|---|
| `cli-theme.js` | Node, zero-dep truecolor helpers + semantic roles, spinner, progress bar, banner. Respects `NO_COLOR`. |
| `cli_theme.py` | Same API for Python CLIs. |
| `terminal-palette.json` | The 16-color ANSI palette (reference). |
| `parsec.itermcolors` | iTerm2 color preset (import in Profiles → Colors). |
| `windows-terminal.json` | Windows Terminal scheme fragment (paste into `schemes`). |
| `vscode-terminal.json` | VS Code `workbench.colorCustomizations` snippet. |

### Semantic roles (use these, not raw colors)
- **prompt** `❯` phosphor · **ok** `✓` success · **warn** `⚠` warning ·
  **err** `✗` error · **info** `ℹ` info · **step** `•` muted
- **spinner:** braille frames in phosphor · **progress bar:** phosphor fill on `#1D2A22` track
- **links:** underlined cyan `#4AD0E0` · **dim/secondary:** `#556D60`

### 16-color ANSI mapping
| slot | hex | | slot | hex |
|---|---|---|---|---|
| black | `#0A0E0C` | | br-black | `#3A4A40` |
| red | `#FF5C57` | | br-red | `#FF8079` |
| green | `#4AF626` | | br-green | `#7CFFB2` |
| yellow | `#FFB84D` | | br-yellow | `#FFD08A` |
| blue | `#37AEE2` | | br-blue | `#6FD0F0` |
| magenta | `#FF5FD2` | | br-magenta | `#FF93E2` |
| cyan | `#4AD0E0` | | br-cyan | `#86E9F5` |
| white | `#C8D8CE` | | br-white | `#EAFBF0` |

fg `#D7FBE4` · bg `#0A0E0C` · cursor `#4AF626` · selection `#163A22`

### Node example
```js
const p = require('./cli/cli-theme');
console.log(p.banner());
console.log(p.ok('context doubled'), p.dim('(cache warm)'));
console.log(p.warn('token budget at 80%'));
console.log(p.err('provider timeout'), p.link('https://getparsec.ai/docs'));
process.stdout.write(p.bar(0.62, 24) + ' 62%\n');
```

---

## 6. File map

```
parsec-brand-kit/
├─ BRANDING.md            ← this file
├─ brand-guide.html       ← visual reference (open in a browser)
├─ logo/  svg/ png/
├─ tokens/
│  ├─ tokens.json         ← source of truth
│  ├─ theme.css           ← CSS variables (dark + light) + helpers
│  └─ tailwind.preset.js  ← Tailwind preset
└─ cli/
   ├─ cli-theme.js  cli_theme.py
   ├─ terminal-palette.json
   ├─ parsec.itermcolors  windows-terminal.json  vscode-terminal.json
   └─ gen_schemes.py      ← regenerates the scheme files from the palette
```

Font: JetBrains Mono (OFL) — https://www.jetbrains.com/lp/mono/
