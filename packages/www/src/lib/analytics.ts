import type { MouseEvent } from "react";

declare global {
  interface Window {
    dataLayer?: any[];
    rdt?: ((...args: any[]) => void) & { callQueue?: any[]; sendEvent?: (...args: any[]) => void };
  }
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
 * Helper to fire direct Reddit Pixel track call if available on window
 */
function fireDirectPixel() {
  try {
    if (typeof window !== "undefined" && typeof window.rdt === "function") {
      window.rdt("track", "Lead");
      console.log("[Analytics] Direct rdt('track', 'Lead') executed");
    }
  } catch (err) {
    console.error("[Analytics] Error firing Reddit Pixel:", err);
  }
}

/**
 * Fires the Reddit Pixel Lead event and GTM dataLayer events on click,
 * and waits for GTM tags to finish executing before navigating.
 */
export function handleSignInClick(
  e: MouseEvent<HTMLAnchorElement>,
  href: string
) {
  const isModifier =
    e.metaKey || e.ctrlKey || e.shiftKey || e.altKey || e.button !== 0;

  // Set backup cookie
  setPendingLeadCookie();

  // If opening in new tab/window via modifier, fire tracking and let browser handle navigation
  if (isModifier) {
    fireDirectPixel();
    if (typeof window !== "undefined") {
      window.dataLayer = window.dataLayer || [];
      window.dataLayer.push({ event: "Lead" });
      window.dataLayer.push({ event: "lead_click" });
      window.dataLayer.push({ event: "sign_in_click" });
      window.dataLayer.push({ event: "github_login_success" });
    }
    return;
  }

  // Prevent immediate navigation so the browser does not cancel any pending requests
  e.preventDefault();

  let navigated = false;
  const navigate = () => {
    if (!navigated) {
      navigated = true;
      console.log("[Analytics] Navigating to destination:", href);
      window.location.href = href;
    }
  };

  // Hard safety timeout: if GTM takes longer than 1500ms (or is blocked by an extension), navigate anyway
  const safetyTimeout = setTimeout(() => {
    console.warn("[Analytics] Safety timeout reached, proceeding with navigation");
    navigate();
  }, 1500);

  // 1. Direct Reddit Pixel track call
  fireDirectPixel();

  // 2. GTM dataLayer push with official GTM eventCallback & eventTimeout
  try {
    if (typeof window !== "undefined") {
      window.dataLayer = window.dataLayer || [];

      // Push all common event names so any configured trigger in GTM matches
      window.dataLayer.push({ event: "Lead" });
      window.dataLayer.push({ event: "lead_click" });
      window.dataLayer.push({ event: "sign_in_click" });

      // In case GTM trigger is configured as a Form Submission trigger (gtm.formSubmit)
      window.dataLayer.push({
        event: "gtm.formSubmit",
        "gtm.triggers": "259767097_15",
        "gtm.elementUrl": href,
      });

      const conversionId =
        typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
          ? crypto.randomUUID()
          : String(Date.now());

      console.log("[Analytics] Pushing github_login_success to dataLayer with conversion_id:", conversionId);

      window.dataLayer.push({
        event: "github_login_success",
        conversion_id: conversionId,
        // GTM executes eventCallback after all tags triggered by this event have fired
        eventCallback: function (containerId?: string) {
          console.log("[Analytics] GTM eventCallback received from container:", containerId);
          clearTimeout(safetyTimeout);
          // Small 100ms buffer to ensure beacon HTTP transport completes before navigation
          setTimeout(navigate, 100);
        },
        // GTM internal timeout fallback (in case a tag hangs)
        eventTimeout: 1400,
      });
    } else {
      navigate();
    }
  } catch (err) {
    console.error("[Analytics] Error in dataLayer push:", err);
    navigate();
  }
}

