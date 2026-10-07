// Behaves like Claude Cowork's MCP client: discovery, dynamic client
// registration, authorization code + PKCE (S256) through the real edge,
// refresh tokens, then JSON-RPC over Streamable HTTP.
import { request as https } from "node:https";
import { request as http } from "node:http";
import { createHash, randomBytes } from "node:crypto";

const EDGE = "127.0.0.1"; // Caddy; host names are sent as SNI + Host

export function call(method, url, { headers = {}, body } = {}) {
  const u = new URL(url);
  const lib = u.protocol === "https:" ? https : http;
  return new Promise((resolve, reject) => {
    const req = lib(
      {
        host: u.protocol === "https:" ? EDGE : u.hostname,
        port: u.port || (u.protocol === "https:" ? 443 : 80),
        servername: u.hostname,
        path: u.pathname + u.search,
        method,
        rejectUnauthorized: false,
        headers: { host: u.host, ...headers, ...(body ? { "content-length": Buffer.byteLength(body) } : {}) },
      },
      (res) => {
        let data = "";
        res.on("data", (c) => (data += c));
        res.on("end", () => resolve({ status: res.statusCode, headers: res.headers, text: data, json: () => JSON.parse(data) }));
      },
    );
    req.on("error", reject);
    if (body) req.write(body);
    req.end();
  });
}

export class McpClient {
  constructor(base, user) {
    this.base = base; // https://mcp.apps.isp-insoft.de
    this.user = user; // mock Google account
    this.redirect = "http://127.0.0.1:39999/callback";
  }

  async connect() {
    // 1. unauthenticated call -> 401 pointing at resource metadata
    const probe = await call("POST", `${this.base}/mcp`, { headers: { "content-type": "application/json" }, body: "{}" });
    const meta = /resource_metadata="([^"]+)"/.exec(probe.headers["www-authenticate"] ?? "")?.[1];
    if (probe.status !== 401 || !meta) throw new Error(`no auth challenge: ${probe.status}`);
    const prm = (await call("GET", meta)).json();
    const as = (await call("GET", `${prm.authorization_servers[0]}/.well-known/oauth-authorization-server`)).json();
    this.as = as;
    this.resource = prm.resource;
    // 2. dynamic client registration
    const reg = await call("POST", as.registration_endpoint, {
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ client_name: "e2e Cowork", redirect_uris: [this.redirect], token_endpoint_auth_method: "none" }),
    });
    if (reg.status !== 201) throw new Error(`registration: ${reg.status} ${reg.text}`);
    this.clientId = reg.json().client_id;
    // 3. authorize with PKCE, following redirects through mock Google
    const verifier = randomBytes(32).toString("base64url");
    const state = randomBytes(8).toString("hex");
    const authz = new URL(as.authorization_endpoint);
    for (const [k, v] of Object.entries({
      response_type: "code", client_id: this.clientId, redirect_uri: this.redirect, state, resource: this.resource,
      code_challenge: createHash("sha256").update(verifier).digest("base64url"), code_challenge_method: "S256",
    })) authz.searchParams.set(k, v);
    let next = authz.toString();
    for (let hop = 0; hop < 6 && !next.startsWith(this.redirect); hop++) {
      const r = await call("GET", next, { headers: { cookie: `mock_user=${encodeURIComponent(this.user)}` } });
      if (![301, 302, 303, 307].includes(r.status)) throw new Error(`authorize hop ${hop}: ${r.status} ${r.text.slice(0, 200)}`);
      next = r.headers.location;
    }
    const back = new URL(next);
    if (back.searchParams.get("state") !== state) throw new Error("state mismatch");
    if (!back.searchParams.get("code")) throw new Error(`login refused: ${back.searchParams.get("error")} ${back.searchParams.get("error_description") ?? ""}`);
    // 4. token
    const tok = await this.token({ grant_type: "authorization_code", code: back.searchParams.get("code"), redirect_uri: this.redirect, code_verifier: verifier, client_id: this.clientId, resource: this.resource });
    if (tok.status !== 200) throw new Error(`token: ${tok.status} ${tok.text}`);
    this.access = tok.json().access_token;
    this.refreshToken = tok.json().refresh_token;
    return this;
  }

  token(params) {
    return call("POST", this.as.token_endpoint, { headers: { "content-type": "application/x-www-form-urlencoded" }, body: new URLSearchParams(params).toString() });
  }

  async refresh() {
    const r = await this.token({ grant_type: "refresh_token", refresh_token: this.refreshToken, client_id: this.clientId });
    if (r.status === 200) {
      this.access = r.json().access_token;
      this.previousRefresh = this.refreshToken;
      this.refreshToken = r.json().refresh_token;
    }
    return r;
  }

  async rpc(method, params = {}) {
    this.id = (this.id ?? 0) + 1;
    const r = await call("POST", `${this.base}/mcp`, {
      headers: { "content-type": "application/json", accept: "application/json, text/event-stream", authorization: `Bearer ${this.access}`, "mcp-protocol-version": "2025-06-18" },
      body: JSON.stringify({ jsonrpc: "2.0", id: this.id, method, params }),
    });
    if (r.status !== 200) throw new Error(`${method}: HTTP ${r.status} ${r.text.slice(0, 200)}`);
    const msg = r.json();
    if (msg.error) throw new Error(`${method}: ${msg.error.message}`);
    return msg.result;
  }

  async tool(name, args) {
    const res = await this.rpc("tools/call", { name, arguments: args });
    return { ok: !res.isError, text: res.content.map((c) => c.text).join("\n") };
  }

  /** deploy, then follow status() while the build outlives the tool call (as Cowork would). */
  async deploy(args, timeoutMs = 15 * 60_000) {
    const r = await this.tool("deploy", args);
    if (!(r.ok && r.text.startsWith("Still building"))) return r;
    this.stillBuilding = (this.stillBuilding ?? 0) + 1;
    const until = Date.now() + timeoutMs;
    while (Date.now() < until) {
      await new Promise((ok) => setTimeout(ok, 2000));
      const job = JSON.parse((await this.tool("status", { app: args.app })).text).last_deploy;
      if (job && job.state !== "building") return { ok: job.state === "done", text: job.message };
    }
    throw new Error(`deploy of ${args.app} still building after ${timeoutMs} ms`);
  }
}
