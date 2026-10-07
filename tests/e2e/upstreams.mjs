// Mock upstreams for e2e: Atlassian OAuth + Jira API, CRM Plus webservice,
// GitHub App API, the triage routine, and an SMTP sink. GET /_seen returns
// everything they received.
import { createServer } from "node:http";
import { createServer as tcp } from "node:net";
import { createHash, randomBytes } from "node:crypto";
import { execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const port = Number(process.argv[2] ?? 39400);
const smtpPort = Number(process.argv[3] ?? 39425);
const base = `http://127.0.0.1:${port}`;
const blobBase = `http://127.0.0.8:${port}`;
const seen = { jira: [], crm: [], issues: [], fires: [], mails: [], comments: [], repos: [], dispatches: [] };
// GitHub repositories are bare repos here; Actions runs live in runs/<id>/.
const ghDir = process.env.GITHUB_DIR;
const runner = join(dirname(fileURLToPath(import.meta.url)), "fake-actions.py");
const runDir = (id) => join(ghDir, "runs", String(id));
const runOf = (id) => existsSync(join(runDir(id), "run.json")) ? JSON.parse(readFileSync(join(runDir(id), "run.json"), "utf8")) : null;
let nextRun = 1;
const codes = new Map();

const send = (res, status, obj, type = "application/json") => {
  res.writeHead(status, { "content-type": type });
  res.end(typeof obj === "string" ? obj : JSON.stringify(obj));
};
const body = async (req) => { let s = ""; for await (const c of req) s += c; return s; };

const handler = async (req, res) => {
  const url = new URL(req.url, base);
  const p = url.pathname;
  if (p === "/_seen") return send(res, 200, seen);
  // Atlassian
  if (p === "/atlassian/authorize") {
    const code = randomBytes(8).toString("hex");
    codes.set(code, url.searchParams.get("code_challenge"));
    const back = new URL(url.searchParams.get("redirect_uri"));
    back.searchParams.set("code", code);
    back.searchParams.set("state", url.searchParams.get("state"));
    res.writeHead(302, { location: back.toString() });
    return res.end();
  }
  if (p === "/atlassian/token") {
    const f = Object.fromEntries(new URLSearchParams(await body(req)));
    if (f.grant_type === "authorization_code") {
      const want = codes.get(f.code);
      const got = createHash("sha256").update(f.code_verifier ?? "").digest("base64url");
      if (!want || want !== got) return send(res, 400, { error: "invalid_grant" });
    }
    return send(res, 200, { access_token: "jira-at", refresh_token: "jira-rt", expires_in: 3600 });
  }
  if (p === "/atlassian/resources") return send(res, 200, [{ id: "cloud-1", url: "https://isp.atlassian.net" }]);
  if (p.startsWith("/atlassian/ex/jira/")) {
    seen.jira.push({ method: req.method, path: p + url.search, auth: req.headers.authorization });
    return send(res, 200, { displayName: "Alice (Jira)", path: p });
  }
  // CRM Plus
  if (p === "/crm/webservice.php") {
    const q = req.method === "POST" ? Object.fromEntries(new URLSearchParams(await body(req))) : Object.fromEntries(url.searchParams);
    seen.crm.push(q);
    if (q.operation === "getchallenge") return send(res, 200, { success: true, result: { token: "chal" } });
    if (q.operation === "login") {
      const ok = q.username === "alice" && q.accessKey === createHash("md5").update("chalsecret-key").digest("hex");
      return send(res, 200, ok ? { success: true, result: { sessionName: "s1", userId: "19x1" } } : { success: false, error: { code: "INVALID_AUTH", message: "no" } });
    }
    if (q.sessionName === "s1") return send(res, 200, { success: true, result: [{ accountname: "ACME GmbH" }] });
    return send(res, 200, { success: false, error: { code: "INVALID_SESSIONID", message: "no" } });
  }
  // GitHub App API
  if (p.match(/^\/github\/orgs\/[^/]+\/installation$/)) return send(res, 200, { id: 1 });
  if (p.match(/^\/github\/app\/installations\/\d+\/access_tokens$/)) return send(res, 201, { token: "ghs_e2e" });
  const auth = req.headers.authorization ?? "";
  if (p.startsWith("/github/repos/") && auth !== "Bearer ghs_e2e" && !p.includes("/_blob/")) return send(res, 401, { message: "Bad credentials" });
  const newRepo = p.match(/^\/github\/orgs\/([^/]+)\/repos$/);
  if (newRepo && req.method === "POST") {
    const b = JSON.parse(await body(req));
    const bare = join(ghDir, `${b.name}.git`);
    if (existsSync(bare)) return send(res, 422, { message: "Repository creation failed.", errors: [{ message: "name already exists on this account" }] });
    execFileSync("git", ["init", "-q", "--bare", "-b", "main", bare]);
    seen.repos.push({ org: newRepo[1], ...b });
    return send(res, 201, { name: b.name, private: b.private });
  }
  const gh = p.match(/^\/github\/repos\/([^/]+)\/([^/]+)\/actions\/(.+)$/);
  if (gh) {
    const [, , repo, rest] = gh;
    const bare = join(ghDir, `${repo}.git`);
    if (!existsSync(bare)) return send(res, 404, { message: "Not Found" });
    let m;
    if ((m = rest.match(/^workflows\/([\w.-]+)\/dispatches$/)) && req.method === "POST") {
      const b = JSON.parse(await body(req));
      seen.dispatches.push({ repo, workflow: m[1], ...b });
      if (m[1] !== "build.yml" || b.ref !== "main") return send(res, 404, { message: "Not Found" });
      try { execFileSync("python3", [runner, "check", bare, JSON.stringify(b.inputs ?? {})], { stdio: "pipe" }); }
      catch (e) { return send(res, 422, { message: String(e.stderr).trim() }); }
      const id = nextRun++;
      const head_sha = execFileSync("git", ["-C", bare, "rev-parse", "main"]).toString().trim();
      mkdirSync(runDir(id), { recursive: true });
      writeFileSync(join(runDir(id), "run.json"), JSON.stringify({ id, repo, head_sha, event: "workflow_dispatch", status: "queued", conclusion: null, html_url: `https://github.example/${repo}/actions/runs/${id}` }));
      spawn("python3", [runner, "run", bare, JSON.stringify(b.inputs), runDir(id), join(ghDir, "cache")], { stdio: "ignore" });
      res.writeHead(204); return res.end();
    }
    if (rest === "runs") {
      const runs = existsSync(join(ghDir, "runs")) ? readdirSync(join(ghDir, "runs")).map(runOf).filter(Boolean) : [];
      const sha = url.searchParams.get("head_sha");
      return send(res, 200, { workflow_runs: runs.filter((r) => r.repo === repo && (!sha || r.head_sha === sha)).sort((a, b) => b.id - a.id) });
    }
    if ((m = rest.match(/^runs\/(\d+)(\/artifacts|\/jobs)?$/))) {
      const r = runOf(m[1]);
      if (!r || r.repo !== repo) return send(res, 404, { message: "Not Found" });
      if (!m[2]) return send(res, 200, r);
      if (m[2] === "/jobs") return send(res, 200, { jobs: [{ id: r.id, conclusion: r.conclusion }] });
      const zips = readdirSync(runDir(r.id)).filter((f) => f.endsWith(".zip"));
      return send(res, 200, { artifacts: zips.map((f) => ({ name: f.slice(0, -4), archive_download_url: `${base}${p}/${f.slice(0, -4)}/zip` })) });
    }
    if ((m = rest.match(/^runs\/(\d+)\/artifacts\/([\w.-]+)\/zip$/))) {
      // Like GitHub: a redirect to blob storage that needs no token.
      res.writeHead(302, { location: `${blobBase}/github/_blob/${m[1]}/${m[2]}.zip` }); return res.end();
    }
    if ((m = rest.match(/^jobs\/(\d+)\/logs$/))) {
      res.writeHead(302, { location: `${blobBase}/github/_blob/${m[1]}/log.txt` }); return res.end();
    }
    return send(res, 404, { message: "Not Found", path: p });
  }
  const blob = p.match(/^\/github\/_blob\/(\d+)\/([\w.-]+)$/);
  if (blob) {
    if (auth) return send(res, 400, { message: "blob storage refuses Authorization headers" });
    const f = join(runDir(blob[1]), blob[2]);
    if (!existsSync(f)) return send(res, 404, {});
    res.writeHead(200); return res.end(readFileSync(f));
  }
  const issue = p.match(/^\/github\/repos\/([^/]+)\/([^/]+)\/issues$/);
  if (issue && req.method === "POST") {
    const b = JSON.parse(await body(req));
    seen.issues.push({ repo: issue[2], state: "open", ...b });
    return send(res, 201, { number: seen.issues.length, html_url: `https://github.example/${issue[2]}/issues/${seen.issues.length}` });
  }
  const asGh = (i, n) => ({ number: n, title: i.title, body: i.body, state: i.state, labels: (i.labels ?? []).map((name) => ({ name })) });
  if (issue && req.method === "GET") {
    return send(res, 200, seen.issues.map((i, n) => [i, n + 1]).filter(([i]) => i.repo === issue[2] && i.state === "open").map(([i, n]) => asGh(i, n)));
  }
  const one = p.match(/^\/github\/repos\/([^/]+)\/([^/]+)\/issues\/(\d+)(\/comments)?$/);
  if (one) {
    const n = Number(one[3]), i = seen.issues[n - 1];
    if (!i || i.repo !== one[2]) return send(res, 404, { message: "Not Found" });
    if (one[4] && req.method === "POST") { seen.comments.push({ repo: one[2], number: n, ...JSON.parse(await body(req)) }); return send(res, 201, {}); }
    if (req.method === "PATCH") { Object.assign(i, JSON.parse(await body(req))); return send(res, 200, asGh(i, n)); }
    return send(res, 200, asGh(i, n));
  }
  // Routine
  if (p === "/routine/fire") {
    seen.fires.push({ auth: req.headers.authorization, ...JSON.parse(await body(req)) });
    return send(res, 200, {});
  }
  send(res, 404, { error: "not mocked", path: p });
};
createServer(handler).listen(port, "127.0.0.1", () => console.log(`upstreams on ${base}`));
// Second origin, standing in for GitHub's blob storage.
createServer(handler).listen(port, "127.0.0.8");

tcp((sock) => {
  let data = null, buf = "";
  sock.write("220 sink\r\n");
  sock.on("data", (chunk) => {
    buf += chunk.toString();
    let i;
    while ((i = buf.indexOf("\r\n")) >= 0) {
      const line = buf.slice(0, i); buf = buf.slice(i + 2);
      if (data !== null) {
        if (line === ".") { seen.mails.push(data); data = null; sock.write("250 ok\r\n"); } else data += line + "\n";
        continue;
      }
      const cmd = line.toUpperCase();
      if (cmd.startsWith("DATA")) { data = ""; sock.write("354 go\r\n"); }
      else if (cmd.startsWith("QUIT")) { sock.end("221 bye\r\n"); }
      else sock.write("250 ok\r\n");
    }
  });
}).listen(smtpPort, "127.0.0.1");
