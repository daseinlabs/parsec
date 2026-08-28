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
