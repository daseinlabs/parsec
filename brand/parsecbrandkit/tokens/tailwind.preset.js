/* Parsec — Tailwind preset. Import theme.css and load Roboto Serif 400 + Inter 300/500.
 * Usage: presets: [require('./tokens/tailwind.preset.js')]
 * Titles: font-display font-normal. Body: font-sans font-light.
 * Accent button: bg-primary text-on-primary. Never use green for text.
 * Theme-dependent colors follow the root data-theme attribute. */
'use strict';
const tokens = require('./tokens.json');
const variable = (name) => `var(--parsec-${name})`;
const values = (group) => Object.fromEntries(
  Object.entries(group).map(([name, token]) => [name, token.value])
);

module.exports = {
  theme: {
    extend: {
      colors: {
        ...Object.fromEntries(Object.keys(tokens.color.themes.dark).map((name) => [name, variable(name)])),
        black: tokens.color.brand.black.value,
        white: tokens.color.brand.white.value,
        gray: tokens.color.brand.gray.value,
        // Readable aliases for existing generic utilities.
        void: variable('bg'), ink: variable('text'), muted: variable('text-muted'),
        faint: variable('text-faint'), line: variable('border'), 'line-strong': variable('border-strong')
      },
      fontFamily: {
        display: tokens.font.display.value,
        serif: tokens.font.display.value,
        sans: tokens.font.sans.value,
        mono: tokens.font.mono.value
      },
      fontWeight: { light: tokens.font.weight.light, normal: tokens.font.weight.regular, medium: tokens.font.weight.medium },
      fontSize: values(tokens.fontSize),
      lineHeight: tokens.lineHeight,
      letterSpacing: tokens.letterSpacing,
      // Prefix the kit's spacing scale to avoid changing Tailwind's standard spacing utilities.
      spacing: Object.fromEntries(Object.entries(tokens.space).map(([name, value]) => [`parsec-${name}`, value])),
      borderRadius: tokens.radius,
      boxShadow: { none: tokens.shadow.none.value },
      transitionDuration: { fast: tokens.motion.fast, base: tokens.motion.base, slow: tokens.motion.slow },
      transitionTimingFunction: { parsec: tokens.motion.ease }
    }
  }
};
