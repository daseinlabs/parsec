import type { SyntheticEvent } from "react";
import { sendGTMEvent } from "@next/third-parties/google";

declare global {
  interface Window {
    dataLayer?: Object[];
    google_tag_manager?: Record<string, any>;
    rdt?: ((...args: any[]) => void) & {
      callQueue?: any[];
      sendEvent?: (...args: any[]) => void;
    };
  }
}

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

/**
 * Helper to initialize Reddit Pixel stub if not already loaded,
 * and fire the "Lead" track event directly.
 */
export function fireRedditPixelLead() {
  try {
    if (typeof window !== "undefined") {
      if (!window.rdt) {
        const p: any = (window.rdt = function (...args: any[]) {
          p.sendEvent ? p.sendEvent.apply(p, args) : p.callQueue.push(args);
        });
        p.callQueue = [];
      }
      window.rdt("track", "Lead");
    }
  } catch (err) {
    console.error("[Analytics] Error firing Reddit Pixel:", err);
  }
}

/**
 * Fires the Reddit Pixel Lead and GTM dataLayer events on click,
 * and waits for GTM tags to finish executing before navigating.
 */
export function handleSignInClick(
  e: SyntheticEvent<HTMLElement>,
  href: string
) {
  const nativeEvent = e.nativeEvent as MouseEvent | undefined;
  const isModifier =
    nativeEvent &&
    (nativeEvent.metaKey ||
      nativeEvent.ctrlKey ||
      nativeEvent.shiftKey ||
      nativeEvent.altKey ||
      nativeEvent.button !== 0);

  // Set backup cookie
  setPendingLeadCookie();

  // 1. Direct Reddit Pixel Lead event
  fireRedditPixelLead();

  // 2. Push GTM dataLayer events
  if (typeof window !== "undefined") {
    window.dataLayer = window.dataLayer || [];

    // Trigger matching Reddit Lead Trigger (gtm.formSubmit with trigger ID 259767097_15)
    window.dataLayer.push({
      event: "gtm.formSubmit",
      "gtm.triggers": "259767097_15",
      "gtm.elementUrl": href,
      "gtm.elementText": "Sign in",
    });

    window.dataLayer.push({ event: "Lead" });
    window.dataLayer.push({ event: "lead_click" });
    window.dataLayer.push({ event: "sign_in_click" });
  }

  // If opening in new tab/window via modifier, fire tracking and let browser handle navigation
  if (isModifier) {
    if (typeof window !== "undefined") {
      window.dataLayer?.push({ event: "github_login_success" });
    }
    return;
  }

  // Prevent immediate navigation so the browser does not cancel pending analytics beacons
  e.preventDefault();

  let navigated = false;
  const navigate = () => {
    if (!navigated) {
      navigated = true;
      window.location.href = href;
    }
  };

  // Safety timeout: if GTM takes longer than 1200ms (or 150ms in dev/when GTM is absent), navigate anyway
  const isGtmLoaded =
    typeof window !== "undefined" && Boolean(window.google_tag_manager);
  const safetyTimeout = setTimeout(navigate, isGtmLoaded ? 1200 : 150);

  // GTM dataLayer push with official GTM eventCallback & eventTimeout
  try {
    if (typeof window !== "undefined") {
      const conversionId =
        typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
          ? crypto.randomUUID()
          : String(Date.now());

      window.dataLayer?.push({
        event: "github_login_success",
        conversion_id: conversionId,
        // GTM executes eventCallback after all tags triggered by this event have fired
        eventCallback: function () {
          clearTimeout(safetyTimeout);
          // Small 100ms buffer to ensure beacon HTTP transport completes before navigation
          setTimeout(navigate, 100);
        },
        // GTM internal timeout fallback (in case a tag hangs)
        eventTimeout: 1100,
      });
    } else {
      navigate();
    }
  } catch {
    navigate();
  }
}
