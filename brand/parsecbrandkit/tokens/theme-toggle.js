/* parsec — theme toggle
 * Sets data-theme on <html>. Defaults to the OS preference (via the
 * prefers-color-scheme block in theme.css) until the user chooses.
 * Persist however your app persists settings — this uses a simple
 * in-memory + optional callback so it stays storage-agnostic.
 *
 *   import { initTheme, toggleTheme, setTheme } from './theme-toggle.js';
 *   initTheme(saved);                 // saved: 'dark' | 'light' | undefined
 *   button.onclick = () => onChange(toggleTheme());
 */
const root = document.documentElement;

export function setTheme(mode) {
  if (mode === 'dark' || mode === 'light') root.setAttribute('data-theme', mode);
  else root.removeAttribute('data-theme'); // fall back to OS preference
  return mode;
}

export function currentTheme() {
  const explicit = root.getAttribute('data-theme');
  if (explicit) return explicit;
  return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
}

export function toggleTheme() {
  return setTheme(currentTheme() === 'dark' ? 'light' : 'dark');
}

export function initTheme(saved) {
  if (saved) setTheme(saved);
}
