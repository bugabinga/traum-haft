// The platform MCP as Claude Cowork uses it, through the real edge.
import { McpClient, call } from "./mcp-client.mjs";

const MCP = "https://mcp.apps.isp-insoft.de";
const APPS = process.env.APPS_DIR;
import { readFileSync, existsSync } from "node:fs";

let pass = 0, fail = 0;
async function check(name, fn) {
  try { await fn(); console.log(`PASS  ${name}`); pass++; }
  catch (e) { console.log(`FAIL  ${name}: ${String(e.message ?? e).split("\n").slice(0, 3).join(" | ")}`); fail++; }
}
function assert(c, m) { if (!c) throw new Error(m); }

let alice, bob, triage;
await check("alice connects: discovery, DCR, PKCE, Google login, token", async () => {
  alice = await new McpClient(MCP, "alice@isp-insoft.de").connect();
  const init = await alice.rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "e2e", version: "1" } });
  assert(init.protocolVersion === "2025-06-18", JSON.stringify(init));
  const tools = (await alice.rpc("tools/list")).tools.map((t) => t.name);
  assert(["create_app", "write_files", "deploy", "rollback", "list_issues"].every((t) => tools.includes(t)), tools.join(","));
});
for (const who of ["eve@gmail.com", "mallory@isp-insoft.de"]) {
  await check(`refused at login: ${who}`, async () => {
    let refused = false;
    try { await new McpClient(MCP, who).connect(); } catch (e) { refused = /login refused|access_denied/.test(String(e)); }
    assert(refused, "connected");
  });
}
await check("forged or missing token refused", async () => {
  const r = await call("POST", `${MCP}/mcp`, { headers: { authorization: "Bearer forged", "content-type": "application/json" }, body: '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' });
  assert(r.status === 401, `status ${r.status}`);
});

await check("create_app validates names and refuses duplicates", async () => {
  assert(!(await alice.tool("create_app", { name: "Bad_Name", title: "x" })).ok, "bad name accepted");
  assert(!(await alice.tool("create_app", { name: "connect", title: "x" })).ok, "reserved name accepted");
  const r = await alice.tool("create_app", { name: "board", title: "Board" });
  assert(r.ok, r.text);
  assert(!(await alice.tool("create_app", { name: "board", title: "Board" })).ok, "duplicate accepted");
});

await check("get_source returns the scaffolded template", async () => {
  const r = await alice.tool("get_source", { app: "board" });
  const files = JSON.parse(r.text).files;
  const html = files.find((f) => f.path === "web/index.html");
  assert(html && html.content.includes("<title>Board</title>"), "title not filled in");
  assert(files.some((f) => f.path === "module/src/lib.rs"), "module missing");
  assert(!files.some((f) => f.path.includes("node_modules")), "node_modules leaked");
});

await check("write_files refuses .github, .git and traversal; nothing partial", async () => {
  for (const path of [".github/workflows/x.yml", ".git/config", "../escape.txt", "/etc/passwd", "web/../../x"]) {
    const r = await alice.tool("write_files", { app: "board", files: [{ path: "ok.txt", content: "x" }, { path, content: "x" }] });
    assert(!r.ok, `${path} accepted`);
  }
  const src = JSON.parse((await alice.tool("get_source", { app: "board" })).text).files;
  assert(!src.some((f) => f.path === "ok.txt"), "partial write happened");
});

await check("alice edits and deploys version 1", async () => {
  const src = JSON.parse((await alice.tool("get_source", { app: "board" })).text).files;
  const html = src.find((f) => f.path === "web/index.html").content.replaceAll("Board", "Team-Board");
  assert((await alice.tool("write_files", { app: "board", files: [{ path: "web/index.html", content: html }] })).ok);
  const d = await alice.tool("deploy", { app: "board", message: "Titel geändert" });
  assert(d.ok && d.text.includes("version 1") && d.text.includes("new database"), d.text);
  assert(readFileSync(`${APPS}/board/current/index.html`, "utf8").includes("Team-Board"), "release not live");
});

await check("bob cannot see or change alice's app", async () => {
  bob = await new McpClient(MCP, "bob@isp-insoft.de").connect();
  assert(!(await bob.tool("list_apps", {})).text.includes("board"), "listed");
  for (const [t, a] of [["get_source", { app: "board" }], ["write_files", { app: "board", files: [{ path: "x", content: "y" }] }], ["deploy", { app: "board", message: "x" }], ["rollback", { app: "board", version: 1 }]]) {
    const r = await bob.tool(t, a);
    assert(!r.ok && r.text.includes("belongs to someone else"), `${t}: ${r.text}`);
  }
});

await check("schema change that would delete data is refused; live version unchanged", async () => {
  const src = JSON.parse((await alice.tool("get_source", { app: "board" })).text).files;
  const lib = src.find((f) => f.path === "module/src/lib.rs").content;
  // A column type change the frontend does not use: compiles, but SpacetimeDB
  // cannot migrate it without deleting the data.
  const changed = lib.replace("pub created_at: Timestamp,", "pub created_at: u64,").replace("created_at: ctx.timestamp,", "created_at: 0,");
  assert(changed !== lib, "test edit did not apply");
  try {
    await alice.tool("write_files", { app: "board", files: [{ path: "module/src/lib.rs", content: changed }] });
    const d = await alice.tool("deploy", { app: "board", message: "Spaltentyp geändert" });
    assert(!d.ok && d.text.includes("delete the app's data"), d.text.slice(0, 400));
    const s = JSON.parse((await alice.tool("status", { app: "board" })).text);
    assert(s.live_version === 1, `live ${s.live_version}`);
  } finally {
    await alice.tool("write_files", { app: "board", files: [{ path: "module/src/lib.rs", content: lib }] });
  }
});

await check("platform agent: may fix any app, may not create apps or delete data", async () => {
  triage = await new McpClient(MCP, "triage@isp-insoft.de").connect();
  assert((await triage.tool("list_apps", {})).text.includes("board"), "agent cannot see board");
  assert(!(await triage.tool("create_app", { name: "x1", title: "x" })).ok, "agent created an app");
  const loss = await triage.tool("deploy", { app: "board", message: "x", confirm_data_loss: true });
  assert(!loss.ok && loss.text.includes("only the builder"), loss.text);
  const d = await triage.tool("deploy", { app: "board", message: "Automatische Korrektur" });
  assert(d.ok && d.text.includes("version 2"), d.text);
  const h = await alice.tool("history", { app: "board" });
  assert(h.text.includes("traum-haft triage (Claude): Automatische Korrektur"), h.text);
});

await check("rollback to version 1", async () => {
  const r = await alice.tool("rollback", { app: "board", version: 1 });
  assert(r.ok, r.text);
  assert(JSON.parse((await alice.tool("status", { app: "board" })).text).live_version === 1);
});

await check("refresh tokens rotate; reusing an old one revokes the family", async () => {
  const r1 = await alice.refresh();
  assert(r1.status === 200, r1.text);
  assert((await alice.tool("list_apps", {})).ok, "new access token rejected");
  const reuse = await alice.token({ grant_type: "refresh_token", refresh_token: alice.previousRefresh, client_id: alice.clientId });
  assert(reuse.status === 400 && reuse.json().error === "invalid_grant", reuse.text);
  const after = await alice.refresh();
  assert(after.status === 400, "family not revoked");
});

console.log(`== mcp: ${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
