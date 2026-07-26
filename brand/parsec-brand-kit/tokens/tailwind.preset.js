/* parsec — Tailwind preset
   Usage: presets: [require('./tokens/tailwind.preset.js')]
   Then: bg-void, text-phosphor, border-line, etc. */
module.exports = {
  theme: {
    extend: {
      colors: {
        phosphor: { DEFAULT: '#4AF626', hover: '#64F846', press: '#38C41B' },
        mint: '#7CFFB2',
        dim: '#2E7D46',
        void: '#0A0E0C',
        surface: '#0F1512',
        elevated: '#131B17',
        overlay: '#16201B',
        line: '#1D2A22',
        'line-strong': '#2E7D46',
        ink: '#D7FBE4',
        muted: '#8CA897',
        faint: '#556D60',
        success: '#4AF626',
        warning: '#FFB84D',
        error: '#FF5C57',
        info: '#4AD0E0',
        magenta: '#FF5FD2'
      },
      fontFamily: {
        mono: ['JetBrains Mono', 'ui-monospace', 'IBM Plex Mono', 'monospace'],
        sans: ['IBM Plex Sans', 'ui-sans-serif', 'system-ui', 'sans-serif']
      },
      borderRadius: { sm: '4px', md: '8px', lg: '12px', xl: '20px' },
      boxShadow: {
        glow: '0 0 6px rgba(74,246,38,.55), 0 0 22px rgba(74,246,38,.20)',
        'glow-strong': '0 0 8px rgba(74,246,38,.9), 0 0 26px rgba(74,246,38,.35)'
      },
      transitionTimingFunction: { parsec: 'cubic-bezier(.2,.6,.2,1)' }
    }
  }
};
