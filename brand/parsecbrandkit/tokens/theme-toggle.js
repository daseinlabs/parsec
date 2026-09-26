/* Parsec — explicit dark/light selection. The brand defaults to black.
 * No OS preference override. Storage is owned by the consuming application.
 *
 *   import { initTheme, toggleTheme } from './theme-toggle.js';
 *   initTheme(saved);  // 'dark' | 'light' | undefined; default is dark
 *   button.onclick = () => onChange(toggleTheme());
 */
export function setTheme(mode) {
  const resolved = mode === 'light' ? 'light' : 'dark';
  document.documentElement.setAttribute('data-theme', resolved);
  return resolved;
}

export function currentTheme() {
  return document.documentElement.getAttribute('data-theme') === 'light' ? 'light' : 'dark';
}

export function toggleTheme() {
  return setTheme(currentTheme() === 'dark' ? 'light' : 'dark');
}

export function initTheme(saved) {
  return setTheme(saved);
}
