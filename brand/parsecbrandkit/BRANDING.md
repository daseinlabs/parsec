# Parsec — brand system

Parsec is minimal, precise, and technical: classic typography, simple geometric
linework, and generous empty space. Black and white lead; gray supports; green
marks one important moment.

This kit implements the supplied **parsec brand system.pdf**: logo suite on
pages 1–5, typography on pages 7–10, color on pages 12–14, and illustrations on
page 16. The supplied root **logo.svg** is the master logo artwork, copied
unchanged to [`logo/svg/logo.svg`](logo/svg/logo.svg). The accompanying
`design_spec.md` supplies the more specific guidance for visual density and charts.

The PDF defines the visual identity. UI spacing, font sizes, interaction states,
light-mode behavior, and terminal mappings below are implementation choices for
this kit, not additional specifications claimed by the PDF.

## Logo

The logo combines a circular, double-ring symbol crossed by a horizontal line
with the **Parsec** serif wordmark. Use the supplied outlines; do not typeset a
replacement wordmark or redraw the symbol. The standalone symbol is extracted
from PDF page 4, which supplies the complete right-hand line absent from the
joined symbol/wordmark export.

| Asset in `logo/` | Use |
| --- | --- |
| `svg/logo.svg` | Canonical full logo, byte-for-byte copy of the supplied SVG |
| `svg/parsec-logo-black.svg` | Full logo in black for white or occasional green backgrounds |
| `svg/parsec-symbol.svg`, `svg/parsec-symbol-black.svg` | Standalone symbol, light or black |
| `svg/parsec-mark-flat.svg` | Standalone light symbol at the existing integration path |
| `svg/parsec-icon-512.svg` | Light symbol on a flat black square with rounded corners |
| `svg/parsec-avatar.svg` | Light symbol on a black circle |
| `svg/parsec-favicon-16.svg`, `-32.svg`, `-64.svg` | Square black favicon tiles |
| `png/parsec-logo.png`, `png/parsec-logo-black.png` | Full logo raster exports |
| `png/parsec-icon-512.png`, `-1024.png` | App and marketplace icons |
| `png/parsec-avatar.png` | Circular profile image |
| `png/parsec-favicon-16.png`, `-32.png`, `-64.png` | Favicon raster exports |
| `png/parsec-mark-flat.png`, `png/parsec-symbol-black.png` | Transparent standalone symbols |

Existing `parsec-mark-glow.svg`, `parsec-mark-glow.png`, and
`parsec-mark-glow-dark.png` paths remain compatibility exports of the **new**
symbol, with no glow. The `-dark.png` export has a black background. They keep
existing consumers working; use the canonical names for new integrations.

Prefer SVG. Preserve aspect ratio, proportions, and the horizontal line. Leave
generous space around the logo; do not crop the line, rotate, stretch, add effects,
or color the logo green. Use the black version on light backgrounds. The original
SVG's `#FDFEFE` artwork fill is preserved; interface white is `#FFFFFF`.

All PNGs and compatibility SVGs can be regenerated with:

```sh
python3 -m pip install PyMuPDF
python3 logo/gen_assets.py
```

The generator reads the two canonical SVGs (`logo.svg` and `parsec-symbol.svg`)
from this kit. It works from any current directory and requires no original PDF.

## Color

| Token | Value | Role |
| --- | --- | --- |
| Black | `#000000` | Primary backgrounds; text on white or green |
| White | `#FFFFFF` | Primary text and technical linework on black |
| Gray | `#828282` | Supporting tones, rules, secondary elements |
| Green | `#7EFB94` | Small button fills, illustration highlights, one selected datapoint |

Black and white are primary, gray secondary, green tertiary. Use green in only
**one or two moments per design**. **Never use green for text.** Green can fill a
button with black text or highlight the one datapoint that matters. The PDF also
permits an occasional green background with black type; this is an exception,
not the default surface. Do not introduce additional hues.

Use white/black text, symbols, and explicit labels for success, warning, error,
and information. Status must remain understandable without a hue change. Links
use the foreground color and an underline. No colored text, glows, gradients,
shadows, or decorative textures.

### UI themes

Dark is the default, regardless of OS preference. Set `data-theme="light"` on
`<html>` for an explicit neutral inversion; the PDF's white guide layouts inform
this optional mode. `tokens/theme-toggle.js` provides storage-agnostic controls.

| Role | Dark | Light |
| --- | --- | --- |
| Background / surface / elevated / overlay | `#000000` | `#FFFFFF` |
| Main text, status text, links, focus outline | `#FFFFFF` | `#000000` |
| Secondary text | `#828282` | `#000000` |
| Disabled text / supporting rules | `#828282` | `#828282` |
| Primary button fill | `#7EFB94` | `#7EFB94` |
| Text on primary button | `#000000` | `#000000` |

Gray is reserved for supporting graphics or disabled content on white; use black
for readable secondary text there. Separate panels with thin rules and spacing.
Use monochrome outlines for hover and focus rather than inventing accent shades.

`tokens/tokens.json` is the machine-readable source. Regenerate the CSS with
`python3 tokens/gen_theme.py`. The Tailwind preset reads the same tokens and uses
CSS variables for theme-dependent colors; import `tokens/theme.css` alongside it.
Use `bg-primary text-on-primary` for an accent button, never `text-primary`.

## Typography

| Role | Typeface | Weight | Case |
| --- | --- | --- | --- |
| Major headings / titles | Roboto Serif | Regular, 400 | Natural title or sentence case |
| Subheadings / eyebrows | Inter | Medium, 500 | ALL CAPS |
| Body / labels / supporting text | Inter | Light, 300 | Sentence case |
| Code / terminal output | User's system monospace | Terminal default | Preserve code and command case |

Place short uppercase subheadings above major headings or use them for secondary
points of information. Keep paragraphs brief. Use serif titles for the classic
editorial hierarchy and sans-serif text for information. Monospace is functional
for code and terminals, not the visual identity. Font files are not bundled;
applications must load Roboto Serif 400 and Inter 300/500. Fallbacks are in the
tokens. Do not rebuild the outlined logo using a font.

The kit's UI scale is 12, 14, 16 (base), 18, 24, 32, 48, 64, and 80 px. Choose
sizes for context and readability; the PDF does not prescribe a fixed scale.

## Content

Write **Parsec** in prose. Keep the executable and command examples lowercase:
`parsec`. Copy should be brief, concrete, and easy to scan.

This kit uses **“Agents Without Limits.”**, shown on PDF page 14, as its brand
line. Supporting examples include
**“Agent Context”**, **“Input Tokens Delivered To The Model”**, **“Get Started For
Free”**, and **“Read the Benchmark”**. Its campaign example reads **“78% fewer
tokens and double your Claude Code limit”**. Treat that percentage and the other
example metrics as campaign copy requiring current supporting evidence, not as
evergreen defaults or CLI claims. The CLI banner uses the brand line without a
numeric savings promise.

## Illustrations, charts, and layout

- Use thin, consistent white outlines on black, inspired by technical CAD
  drawings. Prefer simple geometry, clear connectors, and a clear reading direction.
- Explain one idea per visual. Use 2–3 datapoints, nodes, or stages by default,
  with a maximum of 4; split more complex explanations into separate visuals.
- Leave generous empty space. Avoid crossing arrows, dense networks, elaborate
  3D effects, and decoration that does not explain the idea.
- Choose the simplest chart, label values directly, and minimize grids and legends.
  Preserve scales and proportions; identify illustrative data.
- Keep most linework white and supporting elements gray. Green highlights at most
  one or two small elements, never their text labels.

## CLI

The terminal adaptation uses black backgrounds, white output, gray secondary
output, and a green cursor or progress graphic. Terminal fonts remain under the
user's control. Text-only output identifies the brand as `Parsec`; it does not
attempt to redraw the supplied SVG as ASCII art.

| File in `cli/` | Purpose |
| --- | --- |
| `cli-theme.js`, `cli_theme.py` | Node/Python truecolor helpers; symbols, spinner, progress, and banner |
| `terminal-palette.json` | Generated 16-slot ANSI mapping and terminal roles |
| `parsec.itermcolors` | iTerm2 color preset |
| `windows-terminal.json` | Windows Terminal scheme fragment |
| `vscode-terminal.json` | VS Code terminal color customizations |
| `gen_schemes.py` | Regenerates all four presets from `tokens/tokens.json` |

Helpers respect `NO_COLOR`, `TERM=dumb`, and non-TTY output. Prompt, success,
warning, error, and information symbols use white. Secondary lines and spinner
labels use gray. `accent()` emphasizes text in white; `bar()` uses green only for
its graphical fill. `mark()` returns the plain-text brand name. Both language
helpers read the adjacent `tokens/tokens.json`; distribute them with that file.

The 16 ANSI slots deliberately map to black, gray, or white. Traditional slot
names such as red/green/blue remain terminal protocol names, not extra brand
colors. Green is reserved for the cursor and explicit truecolor graphics. This
palette removes hue distinctions in other applications; use labels and symbols
when communicating status.

| Slots | Color |
| --- | --- |
| 0 (black) | `#000000` |
| 1–7 (normal foreground slots) | `#FFFFFF` |
| 8 (bright black / secondary) | `#828282` |
| 9–15 (bright foreground slots) | `#FFFFFF` |

Foreground `#FFFFFF`; background `#000000`; cursor `#7EFB94`; cursor text
`#000000`; selection `#828282` with black selected text where supported.

```js
const p = require('./cli/cli-theme');
console.log(p.banner());
console.log(p.ok('Ready'), p.dim('(cache warm)'));
console.log(p.warn('Token budget at 80%'));
console.log(p.err('Provider timeout'), p.link('https://getparsec.ai/docs'));
process.stdout.write(p.bar(0.62, 24) + ' 62%\n');
```

Regenerate terminal presets from any directory:

```sh
python3 cli/gen_schemes.py
```

## File map and migration

```text
parsecbrandkit/
├── BRANDING.md
├── logo/
│   ├── gen_assets.py
│   ├── svg/                # Canonical sources and derived SVGs
│   └── png/                # Generated raster exports
├── tokens/
│   ├── tokens.json         # Design and terminal token source
│   ├── gen_theme.py        # CSS generator
│   ├── theme.css           # Generated variables and UI helpers
│   ├── tailwind.preset.js  # Tailwind theme extension
│   └── theme-toggle.js     # Explicit dark/light selection
└── cli/
    ├── cli-theme.js
    ├── cli_theme.py
    ├── gen_schemes.py
    ├── terminal-palette.json
    ├── parsec.itermcolors
    ├── windows-terminal.json
    └── vscode-terminal.json
```

Version 2 replaces the previous color/font token schema. Migrate old accent-text
utilities to foreground text; use `primary` only for fills and `on-primary` for
their black labels. Use `font-display` for titles, `font-sans` for body text, and
the `.parsec-subheading` helper for uppercase Inter labels. Removed glow/shadow
effects must not be recreated. Existing logo filenames retain the new artwork
for consumers that already reference them.

CLI semantic helpers keep their names. Replace removed raw colorizers with
`color.text` or `color.muted`; reserve the green progress color for `bar()`.
