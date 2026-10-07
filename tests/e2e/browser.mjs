// Drives a real Chromium through the local traum-haft stack (see run.sh).
import { createRequire } from "node:module";

// Playwright comes from the global install (PLAYWRIGHT_ROOT = `npm root -g`).
const { chromium } = createRequire(`${process.env.PLAYWRIGHT_ROOT}/`)("playwright");

const NOTES = "https://notes.apps.isp-insoft.de/";
let pass = 0, fail = 0;
async function check(name, fn) {
  try { await fn(); console.log(`PASS  ${name}`); pass++; }
  catch (e) { console.log(`FAIL  ${name}: ${String(e.message ?? e).split("\n")[0]}`); fail++; }
}
function assert(cond, msg) { if (!cond) throw new Error(msg); }

const browser = await chromium.launch({
  args: [
    "--no-proxy-server",
    "--host-resolver-rules=MAP *.isp-insoft.de 127.0.0.1, MAP isp-insoft.de 127.0.0.1",
    "--ignore-certificate-errors",
  ],
});
async function as(email) {
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  await ctx.addCookies([{ name: "mock_user", value: email, domain: "127.0.0.1", path: "/" }]);
  return ctx;
}

const alice = await (await as("alice@isp-insoft.de")).newPage();
const bob = await (await as("bob@isp-insoft.de")).newPage();

await check("alice: Google login, app loads, SpacetimeDB connects", async () => {
  await alice.goto(NOTES);
  await alice.waitForFunction(() => document.querySelector("#status")?.textContent === "Verbunden", null, { timeout: 20000 });
  assert(new URL(alice.url()).host === "notes.apps.isp-insoft.de", `ended at ${alice.url()}`);
});

await check("alice adds a note; module records her email", async () => {
  await alice.fill("#note-text", "Hallo von Alice");
  await alice.click("#new-note button");
  await alice.waitForSelector("#notes li >> text=Hallo von Alice", { timeout: 10000 });
  const by = await alice.textContent("#notes li small");
  assert(by === "alice@isp-insoft.de", `author shown as ${by}`);
});

await check("bob sees alice's note live", async () => {
  await bob.goto(NOTES);
  await bob.waitForSelector("#notes li >> text=Hallo von Alice", { timeout: 20000 });
});

await check("alice and bob have different identities", async () => {
  const a = await alice.getAttribute("#status", "data-identity");
  const b = await bob.getAttribute("#status", "data-identity");
  assert(a && b && a !== b, `${a} vs ${b}`);
});

for (const [who, why] of [["mallory@isp-insoft.de", "private account on company domain (no hd)"], ["eve@gmail.com", "foreign domain"]]) {
  await check(`refused: ${who} (${why})`, async () => {
    const page = await (await as(who)).newPage();
    const res = await page.goto(NOTES);
    const connected = await page.locator("#status", { hasText: "Verbunden" }).count();
    assert(connected === 0, "reached the app");
    assert(res.status() === 403 || /forbidden|unauthorized|denied/i.test(await page.content()), `status ${res.status()}`);
  });
}

// What a malicious app could try from its own page (here: the notes page).
async function wsOutcome(page, url) {
  return page.evaluate((u) => new Promise((resolve) => {
    const ws = new WebSocket(u, ["v2.bsatn.spacetimedb"]);
    ws.onopen = () => { ws.close(); resolve("open"); };
    ws.onerror = () => resolve("error");
    setTimeout(() => resolve("timeout"), 5000);
  }), url);
}
await check("cross-app: notes page cannot open a socket to other's origin", async () => {
  const r = await wsOutcome(alice, "wss://other.apps.isp-insoft.de/_stdb/v1/database/other/subscribe");
  assert(r === "error", r);
});
await check("cross-app: notes origin cannot reach database 'other'", async () => {
  const r = await wsOutcome(alice, "wss://notes.apps.isp-insoft.de/_stdb/v1/database/other/subscribe");
  assert(r === "error", r);
});
await check("token exchange refuses a forged bearer", async () => {
  const status = await alice.evaluate(async () =>
    (await fetch("/_stdb/v1/identity/websocket-token", { method: "POST", headers: { authorization: "Bearer forged" } })).status);
  assert(status === 401, `status ${status}`);
});

await check("connect host needs its own login (app cookie not enough)", async () => {
  const before = (await alice.context().cookies("https://connect.apps.isp-insoft.de")).map((c) => c.name);
  assert(!before.includes("__Host-traum-haft-connect"), "connect cookie existed before");
  await alice.goto("https://connect.apps.isp-insoft.de/");
  const after = (await alice.context().cookies("https://connect.apps.isp-insoft.de")).map((c) => c.name);
  assert(after.includes("__Host-traum-haft-connect"), `cookies: ${after}`);
});

// ---- integrations: connect once, then the app calls as the visitor
const api = (page, path) => page.evaluate(async (p) => {
  const r = await fetch(p);
  return { status: r.status, body: await r.json().catch(() => null) };
}, path);
const seen = async () => (await fetch("http://127.0.0.1:39400/_seen")).json();

await check("jira: unconnected visitor gets a connect URL", async () => {
  await alice.goto(NOTES);
  const r = await api(alice, "/_api/jira/rest/api/3/myself");
  assert(r.status === 401 && r.body.connect_url.startsWith("https://connect.apps.isp-insoft.de/c/jira?app=notes"), JSON.stringify(r));
  await alice.goto(r.body.connect_url);
  const text = await alice.textContent("body");
  assert(text.includes("notes") && text.includes("Jira") && text.includes("lesen"), text.slice(0, 200));
});

await check("jira: consent + OAuth (PKCE) + back in the app; call as alice", async () => {
  await alice.click("button:has-text('Erlauben')");
  await alice.waitForURL(NOTES, { timeout: 15000 });
  const r = await api(alice, "/_api/jira/rest/api/3/myself");
  assert(r.status === 200 && r.body.displayName === "Alice (Jira)", JSON.stringify(r));
  const s = await seen();
  const last = s.jira.at(-1);
  assert(last.auth === "Bearer jira-at" && last.path === "/atlassian/ex/jira/cloud-1/rest/api/3/myself", JSON.stringify(last));
});

await check("jira: bob is not alice (no shared connection)", async () => {
  await bob.goto(NOTES);
  const r = await api(bob, "/_api/jira/rest/api/3/myself");
  assert(r.status === 401, JSON.stringify(r));
});

await check("jira: app without the provider in app.toml is refused", async () => {
  const page = await alice.context().newPage();
  await page.goto("https://other.apps.isp-insoft.de/");
  const r = await api(page, "/_api/jira/rest/api/3/myself");
  assert(r.status === 403, JSON.stringify(r));
  await page.close();
});

await check("crm: wrong key is refused and nothing is stored", async () => {
  const r = await api(alice, "/_api/crmplus/query?query=SELECT%20*%20FROM%20Accounts%3B");
  assert(r.status === 401, JSON.stringify(r));
  await alice.goto(r.body.connect_url);
  await alice.click("button:has-text('Erlauben')");
  await alice.fill("input[name=username]", "alice");
  await alice.fill("input[name=access_key]", "wrong");
  await alice.click("button:has-text('Verbinden')");
  assert((await alice.textContent("body")).includes("refused"), "expected a refusal message");
  await alice.goto(NOTES);
  assert((await api(alice, "/_api/crmplus/query?query=x")).status === 401, "still not connected");
});

await check("crm: right key connects; query works; deleteuser never passes", async () => {
  const r = await api(alice, "/_api/crmplus/query?query=x");
  await alice.goto(r.body.connect_url);
  await alice.click("button:has-text('Erlauben')");
  await alice.fill("input[name=username]", "alice");
  await alice.fill("input[name=access_key]", "secret-key");
  await alice.click("button:has-text('Verbinden')");
  await alice.waitForURL(NOTES, { timeout: 15000 });
  const q = await api(alice, "/_api/crmplus/query?query=SELECT%20*%20FROM%20Accounts%3B");
  assert(q.status === 200 && q.body.result[0].accountname === "ACME GmbH", JSON.stringify(q));
  const d = await alice.evaluate(async () => (await fetch("/_api/crmplus/deleteuser", { method: "POST" })).status);
  assert(d === 403, `deleteuser -> ${d}`);
  assert(!(await seen()).crm.some((c) => c.operation === "deleteuser"), "deleteuser reached CRM");
});

await check("Problem melden: issue, routine, mail to reporter and builder", async () => {
  await alice.goto(NOTES);
  await alice.waitForFunction(() => document.querySelector("#status")?.textContent === "Verbunden", null, { timeout: 20000 });
  let alertText = "";
  alice.once("dialog", async (d) => { alertText = d.message(); await d.accept(); });
  await alice.click("button.th-feedback");
  await alice.fill(".th-dialog textarea", "Der Speichern-Knopf reagiert nicht");
  await alice.click(".th-dialog button[value=send]");
  await alice.waitForTimeout(2500);
  assert(alertText.startsWith("Danke"), `alert: ${alertText}`);
  const s = await seen();
  assert(s.issues.length === 1 && s.issues[0].repo === "app-notes" && s.issues[0].labels.includes("visitor-report"), JSON.stringify(s.issues));
  assert(s.issues[0].body.includes("alice@isp-insoft.de"), "reporter in issue");
  assert(s.fires.length === 1 && s.fires[0].auth === "Bearer routine-e2e", JSON.stringify(s.fires));
  const to = s.mails.map((m) => /^To: (.*)$/m.exec(m)?.[1]);
  assert(to.includes("alice@isp-insoft.de") && to.includes("bob@isp-insoft.de"), JSON.stringify(to));
});

await check("app built by the MCP is live and works (board)", async () => {
  await alice.goto("https://board.apps.isp-insoft.de/");
  await alice.waitForFunction(() => document.querySelector("#status")?.textContent === "Verbunden", null, { timeout: 20000 });
  assert((await alice.title()) === "Team-Board", `title ${await alice.title()}`);
  await alice.fill("#note-text", "Board-Notiz");
  await alice.click("#new-note button");
  await alice.waitForSelector("#notes li >> text=Board-Notiz", { timeout: 10000 });
});

await check("visitor reports a problem in board (bob)", async () => {
  await bob.goto("https://board.apps.isp-insoft.de/");
  await bob.waitForFunction(() => document.querySelector("#status")?.textContent === "Verbunden", null, { timeout: 20000 });
  bob.once("dialog", (d) => d.accept());
  await bob.click("button.th-feedback");
  await bob.fill(".th-dialog textarea", "Die Notizen sind nicht sortiert");
  await bob.click(".th-dialog button[value=send]");
  await bob.waitForTimeout(2000);
  const s = await seen();
  assert(s.issues.some((i) => i.repo === "app-board" && i.body.includes("bob@isp-insoft.de")), JSON.stringify(s.issues.map((i) => i.repo)));
});

await browser.close();
console.log(`== ${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
