/* parsec — CLI theme (Node, truecolor)
 * Zero-dependency ANSI helpers so your CLI matches the brand.
 * Works in any 24-bit-color terminal. Falls back gracefully if
 * NO_COLOR is set or output is not a TTY.
 *
 *   const p = require('./cli-theme');
 *   console.log(p.banner());
 *   console.log(p.ok('installed'), p.dim('(2.1s)'));
 *   console.log(p.err('failed'), p.link('https://getparsec.ai'));
 */
'use strict';

const enabled =
  !process.env.NO_COLOR &&
  process.env.TERM !== 'dumb' &&
  (process.stdout && process.stdout.isTTY !== false);

const HEX = {
  phosphor: '4AF626', mint: '7CFFB2', dim: '2E7D46',
  text: 'D7FBE4', muted: '8CA897', faint: '556D60',
  success: '4AF626', warning: 'FFB84D', error: 'FF5C57',
  info: '4AD0E0', magenta: 'FF5FD2', bg: '0A0E0C'
};

const rgb = (hex) => [
  parseInt(hex.slice(0, 2), 16),
  parseInt(hex.slice(2, 4), 16),
  parseInt(hex.slice(4, 6), 16)
];

function fg(hex, s) {
  if (!enabled) return String(s);
  const [r, g, b] = rgb(hex);
  return `\x1b[38;2;${r};${g};${b}m${s}\x1b[0m`;
}
function style(codes, s) {
  return enabled ? `\x1b[${codes}m${s}\x1b[0m` : String(s);
}

// palette colorizers
const color = {};
for (const [name, hex] of Object.entries(HEX)) color[name] = (s) => fg(hex, s);

const bold = (s) => style('1', s);
const underline = (s) => style('4', s);

// semantic roles — use these in your CLI, not raw colors
const theme = {
  color,
  bold,
  underline,

  prompt: (s = '') => `${color.phosphor(bold('❯'))} ${s}`,       // interactive prompt
  ok:     (s) => `${color.success('✓')} ${color.text(s)}`,        // success line
  warn:   (s) => `${color.warning('⚠')} ${color.text(s)}`,        // warning line
  err:    (s) => `${color.error('✗')} ${color.text(s)}`,          // error line
  info:   (s) => `${color.info('ℹ')} ${color.text(s)}`,           // info line
  step:   (s) => `${color.dim('•')} ${color.muted(s)}`,           // sub-step
  accent: (s) => color.phosphor(bold(s)),
  dim:    (s) => color.faint(s),
  link:   (s) => underline(color.info(s)),
  kbd:    (s) => style('7', ` ${s} `),                            // inverse "key"

  // spinner frames (braille) + a helper
  spinnerFrames: ['⠋','⠙','⠹','⠸','⠼','⠴','⠦','⠧','⠇','⠏'],
  spinner(label, i) { return `${color.phosphor(this.spinnerFrames[i % 10])} ${color.muted(label)}`; },

  // progress bar: parsec.bar(0.62, 24)
  bar(frac, width = 24) {
    const n = Math.round(Math.max(0, Math.min(1, frac)) * width);
    return `${color.phosphor('█'.repeat(n))}${color.faint('─'.repeat(width - n))}`;
  },

  // compact inline mark + one-line banner
  mark: () => color.phosphor(bold('⟩✦')),
  banner() {
    const g = color.phosphor;
    const lines = [
      g('  ╲'),
      g('   ╲'),
      g('    ') + g(bold('✦')) + '  ' + bold(color.text('parsec')),
      g('   ╱'),
      g('  ╱')
    ];
    return lines.join('\n') + '\n  ' + color.muted('2× context · ½ cost') + '\n';
  }
};

module.exports = theme;
