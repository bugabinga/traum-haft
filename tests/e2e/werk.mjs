// Developer apps (werk), end to end: a developer pushes to a repository in
// the org; the platform finds it, builds it on (fake) Actions, the worker
// runs it, and a real Chromium reaches it through the real edge with the
// werk login. Argument: the test app's directory (Containerfile + app.py).
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { McpClient } from "./mcp-client.mjs";

const { chromium } = createRequire(`${process.env.PLAYWRIGHT_ROOT}/`)("playwright");
const GH = process.env.GITHUB_DIR, APPS = process.env.APPS_DIR, APP_SRC = process.argv[2];
const URL_ = "https://werk-demo.werk.isp-insoft.de/";
const seen = async () => (await fetch("http://127.0.0.1:39400/_seen")).json();
const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));

let pass = 0, fail = 0;
async function check(name, fn) {
  try { await fn(); console.log(`PASS  ${name}`); pass++; }
  catch (e) { console.log(`FAIL  ${name}: ${String(e.message ?? e).split("\n").slice(0, 3).join(" | ")}`); fail++; }
}
function assert(c, m) { if (!c) throw new Error(m); }

const WORKFLOW = `name: traum-haft
on:
  workflow_dispatch:
    inputs:
      sha: { required: true }
      version: { required: true }
      recipient: { required: true }
jobs:
  build:
    uses: isp-insoft-gmbh/traum-haft/.github/workflows/build-container.yml@main
    with:
      sha: \${{ inputs.sha }}
      version: \${{ inputs.version }}
      recipient: \${{ inputs.recipient }}
    secrets: inherit
`;
const TOML = (name, extra = "") => `name = "${name}"
port = 8000
health = "/healthz"
owners = ["alice@isp-insoft.de"]
memory = "128m"
secrets = ["DEMO_SECRET"]
integrations = ["jira:read"]
${extra}`;

// A developer's working copy of an org repository.
function repo(name) {
  const bare = join(GH, `${name}.git`);
  if (!existsSync(bare)) execFileSync("git", ["init", "-q", "--bare", "-b", "main", bare]);
  const dir = mkdtempSync(join(tmpdir(), `${name}-`));
  execFileSync("git", ["clone", "-q", bare, dir], { stdio: "ignore" });
  const git = (...a) => execFileSync("git", ["-C", dir, "-c", "user.name=Dev", "-c", "user.email=dev@isp-insoft.de", ...a]).toString().trim();
  return {
    dir,
    write(files) { for (const [p, c] of Object.entries(files)) { mkdirSync(join(dir, p, ".."), { recursive: true }); writeFileSync(join(dir, p), c); } },
    remove(p) { git("rm", "-q", p); },
    push(msg) { git("add", "-A"); git("commit", "-q", "--allow-empty", "-m", msg); git("push", "-q", "origin", "HEAD:main"); return git("rev-parse", "HEAD"); },
  };
}
function appFiles(version, broken = false) {
  const containerfile = readFileSync(join(APP_SRC, "Containerfile"), "utf8")
    .replace("ARG APP_VERSION=1", `ARG APP_VERSION=${version}`)
    .replace("ARG BROKEN=", `ARG BROKEN=${broken ? "1" : ""}`);
  return { "Containerfile": containerfile, "app.py": readFileSync(join(APP_SRC, "app.py"), "utf8") };
}
async function statusFor(repoName, sha, final = true, timeoutMs = 600_000) {
  const until = Date.now() + timeoutMs;
  while (Date.now() < until) {
    const all = (await seen()).statuses.filter((s) => s.repo === repoName && s.sha === sha);
    const last = all.at(-1);
    if (last && (!final || last.state !== "pending")) return { last, all };
    await sleep(1000);
  }
  throw new Error(`no ${final ? "final " : ""}status for ${repoName}@${sha.slice(0, 7)}`);
}
const fetchIn = (page, path, init) => page.evaluate(async ([p, i]) => {
  const r = await fetch(p, i);
  return { status: r.status, text: await r.text() };
}, [path, init ?? {}]);

const browser = await chromium.launch({
  args: ["--no-proxy-server", "--host-resolver-rules=MAP *.isp-insoft.de 127.0.0.1, MAP isp-insoft.de 127.0.0.1", "--ignore-certificate-errors"],
});
async function as(email) {
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  await ctx.addCookies([{ name: "mock_user", value: email, domain: "127.0.0.1", path: "/" }]);
  return ctx;
}
const json = async (page) => JSON.parse(await page.textContent("body"));

mkdirSync(join(GH, "secrets"), { recursive: true });
writeFileSync(join(GH, "secrets", "werk-demo.json"), JSON.stringify({ DEMO_SECRET: "aus-github", OTHER_SECRET: "nicht-gelistet" }));

const demo = repo("werk-demo");
let sha1;
await check("developer pushes 3 files: platform builds, status pending → success with the app URL", async () => {
  demo.write({ "traum-haft.toml": TOML("werk-demo"), ".github/workflows/traum-haft.yml": WORKFLOW, ...appFiles(1) });
  sha1 = demo.push("first");
  const { last, all } = await statusFor("werk-demo", sha1);
  assert(all.some((s) => s.state === "pending"), "no pending status");
  assert(last.state === "success" && last.target_url === URL_, JSON.stringify(last));
});

const alice = await (await as("alice@isp-insoft.de")).newPage();
await check("alice: werk login, app answers with her identity and the GitHub secret", async () => {
  await alice.goto(URL_);
  const j = await json(alice);
  assert(new URL(alice.url()).host === "werk-demo.werk.isp-insoft.de", alice.url());
  assert(j.email === "alice@isp-insoft.de" && j.sub, JSON.stringify(j));
  assert(j.secret === "aus-github", `secret ${j.secret}`);
  assert(j.url === "https://werk-demo.werk.isp-insoft.de" && j.app === "werk-demo", JSON.stringify(j));
});

await check("the app's server never sees the werk session cookie or the edge secret", async () => {
  const cookies = (await alice.context().cookies()).map((c) => c.name);
  assert(cookies.some((n) => n.startsWith("_th_werk")), `browser has no werk cookie: ${cookies}`);
  const j = await json(alice);
  assert(!String(j.cookie ?? "").includes("_th_werk"), `cookie reached the app: ${j.cookie}`);
  assert(j.edge_header === null, "edge secret reached the app");
});

await check("own cookies still reach the app", async () => {
  await alice.context().addCookies([{ name: "pref", value: "dark", domain: "werk-demo.werk.isp-insoft.de", path: "/" }]);
  await alice.reload();
  const j = await json(alice);
  assert(String(j.cookie).includes("pref=dark"), `cookie ${j.cookie}`);
});

await check("only the listed secret is passed", async () => {
  const containers = execFileSync("podman", ["ps", "--filter", "label=traum-haft.app=werk-demo", "--format", "{{.Names}}"]).toString().trim();
  const env = execFileSync("podman", ["inspect", containers.split("\n")[0], "--format", "{{json .Config.Env}}"]).toString();
  assert(env.includes("DEMO_SECRET=aus-github") && !env.includes("OTHER_SECRET"), env.slice(0, 300));
});

await check("integrations: /_api from the werk app; connect returns to the werk origin", async () => {
  const r = await fetchIn(alice, "/_api/jira/rest/api/3/myself");
  assert(r.status === 401, `${r.status} ${r.text}`);
  const connect = new URL(JSON.parse(r.text).connect_url);
  assert(connect.searchParams.get("return") === URL_, connect.toString());
});

await check("the worker refuses requests that do not come through the edge", async () => {
  const r = await fetch("http://127.0.0.13:8080/", { headers: { host: "werk-demo.werk.isp-insoft.de", "x-user-email": "mallory@isp-insoft.de" } });
  assert(r.status === 404, `status ${r.status}`);
});

for (const who of ["eve@gmail.com", "mallory@isp-insoft.de"]) {
  await check(`werk login refuses ${who}`, async () => {
    const p = await (await as(who)).newPage();
    await p.goto(URL_).catch(() => {});
    const body = await p.textContent("body").catch(() => "");
    assert(!body.includes('"email"'), "app answered");
  });
}

await check("logs page: owner sees requests; others get nothing", async () => {
  await alice.goto("https://werk.isp-insoft.de/logs/werk-demo");
  const text = await alice.textContent("body");
  assert(text.includes("request / from alice@isp-insoft.de") && text.includes("v1 live"), text.slice(0, 300));
  const bob = await (await as("bob@isp-insoft.de")).newPage();
  const res = await bob.goto("https://werk.isp-insoft.de/logs/werk-demo");
  assert(res.status() === 404 && !(await bob.textContent("body")).includes("request /"), `bob got ${res.status()}`);
});

await check("logs page needs the edge (no secret, no access)", async () => {
  const r = await fetch("http://127.0.0.5:8080/werk/logs/werk-demo", { headers: { "x-user-email": "alice@isp-insoft.de" } });
  assert(r.status === 403, `status ${r.status}`);
});

await check("guide and llms-dev.txt are public", async () => {
  const p = await browser.newPage({ ignoreHTTPSErrors: true });
  await p.goto("https://werk.isp-insoft.de/");
  assert((await p.textContent("h1")).includes("Eigene Apps"), "guide heading");
  const llms = await p.goto("https://werk.isp-insoft.de/llms-dev.txt");
  assert((await llms.text()).includes("traum-haft werk: deploy a developer app"), "llms-dev.txt");
});

await check("MCP: owner sees the developer app (list, status, logs marked untrusted); bob does not", async () => {
  const a = await new McpClient("https://mcp.apps.isp-insoft.de", "alice@isp-insoft.de").connect();
  const list = JSON.parse((await a.tool("list_apps", {})).text);
  assert(list.some((x) => x.app === "werk-demo" && x.kind === "developer"), JSON.stringify(list));
  const st = JSON.parse((await a.tool("status", { app: "werk-demo" })).text);
  assert(st.state === "live" && st.live_version === 1 && st.repo === "werk-demo", JSON.stringify(st));
  const logs = await a.tool("logs", { app: "werk-demo" });
  assert(logs.ok && logs.text.startsWith("Untrusted content") && logs.text.includes("request /"), logs.text.slice(0, 200));
  const b = await new McpClient("https://mcp.apps.isp-insoft.de", "bob@isp-insoft.de").connect();
  const bs = await b.tool("status", { app: "werk-demo" });
  assert(!bs.ok && bs.text.includes("only its owners"), bs.text);
  assert(!(await b.tool("list_apps", {})).text.includes("werk-demo"), "bob lists it");
  const create = await b.tool("create_app", { name: "werk-demo", title: "x" });
  assert(!create.ok && create.text.includes("taken"), "user app could take a werk name");
});

await check("a broken version is refused; the live one keeps running; owners get mail", async () => {
  demo.write(appFiles(9, true));
  const sha = demo.push("broken");
  const { last } = await statusFor("werk-demo", sha);
  assert(last.state === "failure" && last.description.startsWith("Start fehlgeschlagen"), JSON.stringify(last));
  await alice.goto(URL_);
  assert((await json(alice)).version === "1", "live version changed");
  const mails = (await seen()).mails.filter((m) => /^To: alice@isp-insoft.de/m.test(m) && /fehlgeschlagen/.test(m));
  assert(mails.length >= 1, "no mail to the owner");
});

await check("next good push: v2 live, /data kept", async () => {
  const before = (await json(alice)).counter;
  demo.write(appFiles(2));
  const sha = demo.push("v2");
  const { last } = await statusFor("werk-demo", sha);
  assert(last.state === "success", JSON.stringify(last));
  await alice.goto(URL_);
  const j = await json(alice);
  assert(j.version === "2" && j.counter > before, JSON.stringify(j));
});

await check("invalid traum-haft.toml: failure names the field", async () => {
  const r = repo("werk-bad");
  r.write({ "traum-haft.toml": TOML("Bad_Name"), ".github/workflows/traum-haft.yml": WORKFLOW, ...appFiles(1) });
  const { last } = await statusFor("werk-bad", r.push("bad"));
  assert(last.state === "failure" && last.description.includes("traum-haft.toml ungültig: name"), JSON.stringify(last));
});

await check("same name from another repository: Name vergeben", async () => {
  const r = repo("werk-copy");
  r.write({ "traum-haft.toml": TOML("werk-demo"), ".github/workflows/traum-haft.yml": WORKFLOW, ...appFiles(1) });
  const { last } = await statusFor("werk-copy", r.push("copy"));
  assert(last.state === "failure" && last.description.startsWith("Name vergeben"), JSON.stringify(last));
});

await check("name of a user app: Name vergeben", async () => {
  const r = repo("werk-steal");
  r.write({ "traum-haft.toml": TOML("board"), ".github/workflows/traum-haft.yml": WORKFLOW, ...appFiles(1) });
  const { last } = await statusFor("werk-steal", r.push("steal"));
  assert(last.state === "failure" && last.description.startsWith("Name vergeben"), JSON.stringify(last));
});

await check("missing workflow: Workflow fehlt", async () => {
  const r = repo("werk-noflow");
  r.write({ "traum-haft.toml": TOML("noflow"), ...appFiles(1) });
  const { last } = await statusFor("werk-noflow", r.push("no workflow"));
  assert(last.state === "failure" && last.description.startsWith("Workflow fehlt"), JSON.stringify(last));
});

await check("a user-app repository never becomes a developer app, whatever is pushed into it", async () => {
  const r = repo("app-board");
  r.write({ "traum-haft.toml": TOML("board-dev"), ".github/workflows/traum-haft.yml": WORKFLOW, ...appFiles(1) });
  const sha = r.push("try to promote");
  await sleep(12_000);
  const st = (await seen()).statuses.filter((s) => s.repo === "app-board");
  const dispatched = (await seen()).dispatches.filter((d) => d.repo === "app-board" && d.workflow === "traum-haft.yml");
  assert(st.length === 0 && dispatched.length === 0, `statuses ${st.length}, dispatches ${dispatched.length} for ${sha}`);
  assert(!existsSync(join(APPS, "board-dev")), "board-dev exists");
  const res = await fetch("http://127.0.0.2:8080/_internal/tls-ask?domain=board.werk.isp-insoft.de");
  assert(res.status === 404, `tls-ask board.werk ${res.status}`);
});

await check("deleting traum-haft.toml takes the app offline", async () => {
  demo.remove("traum-haft.toml");
  const sha = demo.push("retire");
  const { last } = await statusFor("werk-demo", sha);
  assert(last.state === "success" && last.description.includes("entfernt"), JSON.stringify(last));
  const ask = await fetch("http://127.0.0.2:8080/_internal/tls-ask?domain=werk-demo.werk.isp-insoft.de");
  assert(ask.status === 404, `tls-ask ${ask.status}`);
  const res = await alice.goto(URL_);
  const body = await alice.textContent("body").catch(() => "");
  assert(!body.includes('"email"'), `still answering (${res?.status()})`);
});

await browser.close();
console.log(`== werk: ${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
