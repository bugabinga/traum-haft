// traum-haft platform client. Keep this file as it is; the platform updates it.
//
// Sign-in, tokens and integration credentials are handled by the platform.
// Apps never see a password, key or OAuth token.

/** App name = first label of the host, e.g. "notes" for notes.apps.isp-insoft.de. */
export const appName = location.hostname.split(".")[0];

/** WebSocket URI for the SpacetimeDB SDK. The trailing slash is required. */
export const spacetimeUri = `${location.origin.replace(/^http/, "ws")}/_stdb/`;

/** A short-lived token for this visitor and this app (cookie-authenticated). */
export async function getToken(): Promise<string> {
  const res = await fetch("/_auth/token", { credentials: "same-origin", cache: "no-store" });
  if (!res.ok) throw new Error(`token request failed: ${res.status}`);
  const body = (await res.json()) as { token: string };
  return body.token;
}

/** Thrown when the visitor has not connected a provider (e.g. Jira) yet. */
export class ConnectRequired extends Error {
  constructor(
    public readonly provider: string,
    public readonly connectUrl: string,
  ) {
    super(`connect ${provider} first`);
  }
}

/**
 * Calls an integration as the visitor, e.g. `api("jira", "rest/api/3/myself")`.
 * The provider must be declared in app.toml.
 */
export async function api(provider: string, path: string, init?: RequestInit): Promise<Response> {
  const res = await fetch(`/_api/${encodeURIComponent(provider)}/${path.replace(/^\//, "")}`, {
    credentials: "same-origin",
    ...init,
  });
  if (res.status === 401) {
    const body = (await res.json().catch(() => ({}))) as { connect_url?: string };
    if (body.connect_url) throw new ConnectRequired(provider, body.connect_url);
  }
  return res;
}

/** Adds the "Problem melden" button. Reports become issues for the app. */
export function mountFeedbackButton(appVersion: string): void {
  const recentErrors: string[] = [];
  addEventListener("error", (e) => recentErrors.push(String(e.message)).toString());
  addEventListener("unhandledrejection", (e) => recentErrors.push(String(e.reason)).toString());

  const button = document.createElement("button");
  button.className = "th-feedback";
  button.textContent = "Problem melden";
  button.addEventListener("click", () => {
    const dialog = document.createElement("dialog");
    dialog.className = "th-dialog";
    dialog.innerHTML = `
      <form method="dialog">
        <p><label>Was ist passiert?<br><textarea name="text" required maxlength="4000"></textarea></label></p>
        <menu><button value="cancel" formnovalidate>Abbrechen</button><button value="send">Senden</button></menu>
      </form>`;
    document.body.append(dialog);
    dialog.addEventListener("close", async () => {
      const text = (dialog.querySelector("textarea") as HTMLTextAreaElement).value.trim();
      dialog.remove();
      if (dialog.returnValue !== "send" || !text) return;
      const res = await fetch("/_api/_platform/feedback", {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          text,
          page: location.pathname + location.search,
          app_version: appVersion,
          errors: recentErrors.slice(-10),
        }),
      });
      alert(res.ok ? "Danke! Die Meldung ist angekommen." : "Senden fehlgeschlagen.");
    });
    dialog.showModal();
  });
  document.body.append(button);
}
