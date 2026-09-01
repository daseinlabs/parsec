import { sendGTMEvent } from "@next/third-parties/google";

/**
 * Fires when a visitor copies the install one-liner — the highest-intent
 * action on the site (the actual install happens in their terminal, outside
 * anything we can observe). Pushed to the GTM dataLayer; a GTM trigger on
 * event name `install_command_copy` forwards it to GA4, where it can be
 * imported as an ad-platform conversion.
 *
 * `method` distinguishes the copy button from a manual select-and-copy;
 * `tool` is set on the per-agent landing pages (/claude-code, /codex,
 * /opencode) where the command is pinned to one agent.
 */
export function trackInstallCopy(opts: {
  os: "unix" | "windows";
  method: "button" | "keyboard";
  tool?: string;
}) {
  sendGTMEvent({ event: "install_command_copy", ...opts });
}

/**
 * Sets a temporary cookie to indicate that a user has initiated the sign-in/lead generation flow.
 * Since the app (app.getparsec.ai) and the marketing site (getparsec.ai) share the same root domain,
 * this cookie is set with the `.getparsec.ai` domain in production so that the app can read it
 * upon landing and fire the Lead event.
 */
export function setPendingLeadCookie() {
  if (typeof document !== "undefined") {
    const isProduction = window.location.hostname.endsWith("getparsec.ai");
    const domainAttribute = isProduction ? "; domain=.getparsec.ai" : "";
    document.cookie = `pending_lead=true${domainAttribute}; path=/; max-age=3600; SameSite=Lax`;
  }
}
