// Mock upstreams for e2e: Atlassian OAuth + Jira API, CRM Plus webservice,
// GitHub App API, the triage routine, and an SMTP sink. GET /_seen returns
// everything they received.
import { createServer } from "node:http";
import { createServer as tcp } from "node:net";
import { createHash, randomBytes } from "node:crypto";

const port = Number(process.argv[2] ?? 39400);
const smtpPort = Number(process.argv[3] ?? 39425);
const base = `http://127.0.0.1:${port}`;
const seen = { jira: [], crm: [], issues: [], fires: [], mails: [] };
const codes = new Map();

const send = (res, status, obj, type = "application/json") => {
  res.writeHead(status, { "content-type": type });
  res.end(typeof obj === "string" ? obj : JSON.stringify(obj));
};
const body = async (req) => { let s = ""; for await (const c of req) s += c; return s; };

createServer(async (req, res) => {
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
  const issue = p.match(/^\/github\/repos\/([^/]+)\/([^/]+)\/issues$/);
  if (issue && req.method === "POST") {
    const b = JSON.parse(await body(req));
    seen.issues.push({ repo: issue[2], ...b });
    return send(res, 201, { number: seen.issues.length, html_url: `https://github.example/${issue[2]}/issues/${seen.issues.length}` });
  }
  // Routine
  if (p === "/routine/fire") {
    seen.fires.push({ auth: req.headers.authorization, ...JSON.parse(await body(req)) });
    return send(res, 200, {});
  }
  send(res, 404, { error: "not mocked", path: p });
}).listen(port, "127.0.0.1", () => console.log(`upstreams on ${base}`));

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
