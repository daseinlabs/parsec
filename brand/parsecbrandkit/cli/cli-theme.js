/* Parsec — truecolor terminal helpers, using ../tokens/tokens.json.
 * Monochrome text; green only for the progress graphic. Zero third-party dependencies.
 * Respects NO_COLOR, TERM=dumb, and non-TTY output. */
'use strict';

const tokens = require('../tokens/tokens.json');
const terminal = tokens.terminal;
const enabled = !process.env.NO_COLOR && process.env.TERM !== 'dumb' && Boolean(process.stdout?.isTTY);
const HEX = {
  text: terminal.foreground, muted: terminal.muted, faint: terminal.muted,
  dim: terminal.muted, success: terminal.foreground, warning: terminal.foreground,
  error: terminal.foreground, info: terminal.foreground, bg: terminal.background
};

function fg(hex, value) {
  if (!enabled) return String(value);
  const clean = hex.replace(/^#/, '');
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(clean.slice(i, i + 2), 16));
  return `\x1b[38;2;${r};${g};${b}m${value}\x1b[0m`;
}
function style(codes, value) {
  return enabled ? `\x1b[${codes}m${value}\x1b[0m` : String(value);
}

const color = Object.fromEntries(Object.entries(HEX).map(([name, hex]) => [name, (s) => fg(hex, s)]));
const bold = (s) => style('1', s);
const underline = (s) => style('4', s);

const theme = {
  color, bold, underline,
  prompt: (s = '') => `${color.text('❯')} ${s}`,
  ok: (s) => `${color.success('✓')} ${color.text(s)}`,
  warn: (s) => `${color.warning('⚠')} ${color.text(s)}`,
  err: (s) => `${color.error('✗')} ${color.text(s)}`,
  info: (s) => `${color.info('ℹ')} ${color.text(s)}`,
  step: (s) => `${color.dim('•')} ${color.muted(s)}`,
  accent: (s) => color.text(bold(s)),
  dim: (s) => color.faint(s),
  link: (s) => underline(color.text(s)),
  kbd: (s) => style('7', ` ${s} `),
  spinnerFrames: ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'],
  spinner(label, i) {
    const frame = ((i % this.spinnerFrames.length) + this.spinnerFrames.length) % this.spinnerFrames.length;
    return `${color.text(this.spinnerFrames[frame])} ${color.muted(label)}`;
  },
  bar(frac, width = 24) {
    const n = Math.round(Math.max(0, Math.min(1, frac)) * width);
    return fg(terminal.progress, '█'.repeat(n)) + fg(terminal.progressTrack, '─'.repeat(width - n));
  },
  // Terminals use the brand name; graphical surfaces must use logo/svg/logo.svg.
  mark: () => color.text(tokens.content.name),
  banner() {
    return `${this.mark()}\n${color.muted(tokens.content.tagline)}\n`;
  }
};

module.exports = theme;
