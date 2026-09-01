import type { MouseEvent, FormEvent, SyntheticEvent } from "react";
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
 * Fires when a visitor copies the install one-liner.
 */
export function trackInstallCopy(opts: {
  os: "unix" | "windows";
  method: "button" | "keyboard";
  tool?: string;
}) {
  sendGTMEvent({ event: "install_command_copy", ...opts });
}

/**
 * Sets a temporary cookie for lead generation.
 */
export function setPendingLeadCookie() {
  if (typeof document !== "undefined") {
    const isProduction = window.location.hostname.endsWith("getparsec.ai");
    const domainAttribute = isProduction ? "; domain=.getparsec.ai" : "";
    document.cookie = `pending_lead=true${domainAttribute}; path=/; max-age=3600; SameSite=Lax`;
  }
}

/**
 * Fires the Reddit Pixel Lead and GTM events on click with extensive debug logs.
 * No navigation or other side actions are performed.
 */
export function handleSignInClick(
  e: SyntheticEvent<HTMLElement>,
  href: string
) {
  // Prevent any browser navigation
  e.preventDefault();

  console.log("%c=======================================================", "color: #2FA317; font-weight: bold;");
  console.log("%c[ANALYTICS DEBUG] 🟢 Sign In Button Click Triggered", "color: #2FA317; font-size: 16px; font-weight: bold;");
  console.log("%c=======================================================", "color: #2FA317; font-weight: bold;");

  console.log("[1] Target Destination:", href);
  console.log("[2] Window Location:", typeof window !== "undefined" ? window.location.href : "N/A");

  // Check GTM availability
  const hasGTM = typeof window !== "undefined" && typeof window.google_tag_manager !== "undefined";
  console.log("[3] Google Tag Manager Status:", {
    loaded: hasGTM,
    containers: hasGTM ? Object.keys(window.google_tag_manager || {}) : "None",
    google_tag_manager_object: typeof window !== "undefined" ? window.google_tag_manager : undefined,
  });

  // Check dataLayer availability
  console.log("[4] DataLayer Status before push:", {
    exists: typeof window !== "undefined" && Array.isArray(window.dataLayer),
    length: typeof window !== "undefined" && Array.isArray(window.dataLayer) ? window.dataLayer.length : 0,
    currentDataLayer: typeof window !== "undefined" && Array.isArray(window.dataLayer) ? [...window.dataLayer] : undefined,
  });

  // Check Reddit Pixel (rdt) availability
  const hasRdt = typeof window !== "undefined" && typeof window.rdt === "function";
  console.log("[5] Reddit Pixel (rdt) Status before firing:", {
    rdtFunctionExists: hasRdt,
    callQueue: typeof window !== "undefined" && window.rdt?.callQueue ? [...window.rdt.callQueue] : "No queue",
    rdtObject: typeof window !== "undefined" ? window.rdt : undefined,
  });

  // 1. Direct Reddit Pixel Track
  try {
    if (typeof window !== "undefined") {
      if (!window.rdt) {
        console.warn("[Analytics] window.rdt not found. Initializing stub callQueue...");
        const p: any = (window.rdt = function (...args: any[]) {
          p.sendEvent ? p.sendEvent.apply(p, args) : p.callQueue.push(args);
        });
        p.callQueue = [];
      }
      console.log("[6] 🎯 Executing window.rdt('track', 'Lead')...");
      window.rdt("track", "Lead");
      console.log("[6] ✅ window.rdt('track', 'Lead') executed successfully. Queue state:", window.rdt.callQueue);
    }
  } catch (rdtErr) {
    console.error("[6] ❌ Error executing window.rdt:", rdtErr);
  }

  // 2. DataLayer Pushes
  if (typeof window !== "undefined") {
    window.dataLayer = window.dataLayer || [];

    const eventsToPush: Record<string, any>[] = [
      // Trigger matching Reddit Lead Trigger (gtm.formSubmit with trigger ID 259767097_15)
      {
        event: "gtm.formSubmit",
        "gtm.triggers": "259767097_15",
        "gtm.elementUrl": href,
        "gtm.elementText": "Sign in",
      },
      // Standard lead events
      {
        event: "Lead",
      },
      {
        event: "lead_click",
      },
      {
        event: "sign_in_click",
      },
      {
        event: "github_login_success",
      },
    ];

    console.log("[7] 📦 Pushing events to dataLayer...");
    eventsToPush.forEach((item, index) => {
      console.log(`    👉 Push [${index + 1}/${eventsToPush.length}]:`, item.event, item);
      window.dataLayer?.push(item);
    });

    console.log("[8] 📊 DataLayer Status after push:", {
      length: window.dataLayer.length,
      dataLayerContent: [...window.dataLayer],
    });
  }

  console.log("%c=======================================================", "color: #2FA317; font-weight: bold;");
  console.log("%c[ANALYTICS DEBUG] 🛑 Finished firing events (No redirect performed)", "color: #FFA500; font-weight: bold;");
  console.log("%c=======================================================", "color: #2FA317; font-weight: bold;");
}
