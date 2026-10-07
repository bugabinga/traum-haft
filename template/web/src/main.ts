import { DbConnection, tables, type ErrorContext } from "./module_bindings";
import { appName, getToken, mountFeedbackButton, spacetimeUri } from "./traum-haft";
import type { Identity } from "spacetimedb";

const status = document.querySelector<HTMLParagraphElement>("#status")!;
const form = document.querySelector<HTMLFormElement>("#new-note")!;
const input = document.querySelector<HTMLInputElement>("#note-text")!;
const list = document.querySelector<HTMLUListElement>("#notes")!;

mountFeedbackButton(import.meta.env.VITE_APP_VERSION ?? "dev");

let conn: DbConnection | undefined;
let retryDelayMs = 1000;

// SDK 2.11 has no automatic reconnect: each attempt fetches a fresh platform
// token and builds a new connection.
async function connect(): Promise<void> {
  let token: string;
  try {
    token = await getToken();
  } catch (e) {
    return retry(`Anmeldung fehlgeschlagen: ${e}`);
  }
  conn = DbConnection.builder()
    .withUri(spacetimeUri)
    .withDatabaseName(appName)
    .withToken(token)
    .onConnect((c: DbConnection, identity: Identity) => {
      retryDelayMs = 1000;
      status.textContent = "Verbunden";
      status.dataset.identity = identity.toHexString();
      form.hidden = false;
      c.subscriptionBuilder().onApplied(render).subscribe([tables.note]);
      c.db.note.onInsert(render);
      c.db.note.onDelete(render);
    })
    .onConnectError((_ctx: ErrorContext, error: Error) => retry(`Verbindung fehlgeschlagen: ${error.message}`))
    .onDisconnect(() => retry("Getrennt, verbinde neu …"))
    .build();
}

function retry(message: string): void {
  status.textContent = message;
  form.hidden = true;
  setTimeout(connect, retryDelayMs);
  retryDelayMs = Math.min(retryDelayMs * 2, 30_000);
}

function render(): void {
  if (!conn) return;
  const notes = [...conn.db.note.iter()].sort((a, b) => Number(b.id - a.id));
  list.replaceChildren(
    ...notes.map((note) => {
      const li = document.createElement("li");
      li.dataset.id = String(note.id);
      const text = document.createElement("span");
      text.textContent = note.text;
      const by = document.createElement("small");
      by.textContent = note.authorEmail;
      li.append(text, by);
      return li;
    }),
  );
}

form.addEventListener("submit", (event) => {
  event.preventDefault();
  const text = input.value;
  input.value = "";
  conn?.reducers.addNote({ text }).catch((e: unknown) => {
    status.textContent = `Fehler: ${e}`;
  });
});

void connect();
