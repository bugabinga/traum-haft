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

await browser.close();
console.log(`== ${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
