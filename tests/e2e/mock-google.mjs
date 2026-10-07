// Minimal stand-in for Google's OpenID Connect provider, for e2e tests only.
// The user is picked by the "mock_user" cookie on this host (default alice).
import { createServer } from "node:http";
import { createHash, generateKeyPairSync, randomBytes, sign } from "node:crypto";

const [host, port] = (process.argv[2] ?? "127.0.0.1:39300").split(":");
const issuer = `http://${host}:${port}`;
const users = {
  "alice@isp-insoft.de": { sub: "google-alice", hd: "isp-insoft.de" },
  "bob@isp-insoft.de": { sub: "google-bob", hd: "isp-insoft.de" },
  // A private Google account on the company domain: no hd claim.
  "mallory@isp-insoft.de": { sub: "google-mallory-private" },
  "eve@gmail.com": { sub: "google-eve" },
};
const { publicKey, privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: "jwk" }), kid: "mock-1", alg: "RS256", use: "sig" };
const codes = new Map();
const refresh = new Map();

const b64u = (b) => Buffer.from(b).toString("base64url");
function idToken(email, clientId, nonce) {
  const now = Math.floor(Date.now() / 1000);
  const u = users[email];
  const claims = { iss: issuer, aud: clientId, azp: clientId, sub: u.sub, email, email_verified: true, iat: now, exp: now + 3600, ...(u.hd ? { hd: u.hd } : {}), ...(nonce ? { nonce } : {}) };
  const head = b64u(JSON.stringify({ alg: "RS256", kid: "mock-1", typ: "JWT" }));
  const body = b64u(JSON.stringify(claims));
  return `${head}.${body}.${b64u(sign("sha256", Buffer.from(`${head}.${body}`), privateKey))}`;
}
function json(res, status, obj) {
  res.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
  res.end(JSON.stringify(obj));
}
async function form(req) {
  let raw = "";
  for await (const chunk of req) raw += chunk;
  return new URLSearchParams(raw);
}

createServer(async (req, res) => {
  const url = new URL(req.url, issuer);
  if (url.pathname === "/.well-known/openid-configuration") {
    return json(res, 200, {
      issuer, authorization_endpoint: `${issuer}/authorize`, token_endpoint: `${issuer}/token`,
      userinfo_endpoint: `${issuer}/userinfo`, jwks_uri: `${issuer}/jwks`,
      response_types_supported: ["code"], subject_types_supported: ["public"],
      id_token_signing_alg_values_supported: ["RS256"], scopes_supported: ["openid", "email", "profile"],
      claims_supported: ["sub", "email", "email_verified", "hd"], code_challenge_methods_supported: ["S256", "plain"],
    });
  }
  if (url.pathname === "/jwks") return json(res, 200, { keys: [jwk] });
  if (url.pathname === "/authorize") {
    const cookie = /(?:^|;\s*)mock_user=([^;]+)/.exec(req.headers.cookie ?? "");
    const email = cookie ? decodeURIComponent(cookie[1]) : "alice@isp-insoft.de";
    if (!users[email]) return json(res, 400, { error: "unknown mock user" });
    const code = b64u(randomBytes(16));
    codes.set(code, { email, clientId: url.searchParams.get("client_id"), redirectUri: url.searchParams.get("redirect_uri"),
      nonce: url.searchParams.get("nonce"), challenge: url.searchParams.get("code_challenge") });
    const back = new URL(url.searchParams.get("redirect_uri"));
    back.searchParams.set("code", code);
    back.searchParams.set("state", url.searchParams.get("state") ?? "");
    res.writeHead(302, { location: back.toString() });
    return res.end();
  }
  if (url.pathname === "/token" && req.method === "POST") {
    const p = await form(req);
    let clientId = p.get("client_id");
    const basic = /^Basic (.+)$/.exec(req.headers.authorization ?? "");
    if (basic) clientId = decodeURIComponent(Buffer.from(basic[1], "base64").toString().split(":")[0]);
    let email;
    if (p.get("grant_type") === "authorization_code") {
      const c = codes.get(p.get("code"));
      codes.delete(p.get("code"));
      if (!c || c.redirectUri !== p.get("redirect_uri") || c.clientId !== clientId) return json(res, 400, { error: "invalid_grant" });
      if (c.challenge && createHash("sha256").update(p.get("code_verifier") ?? "").digest("base64url") !== c.challenge)
        return json(res, 400, { error: "invalid_grant", error_description: "pkce" });
      email = c.email;
      var nonce = c.nonce;
    } else if (p.get("grant_type") === "refresh_token") {
      email = refresh.get(p.get("refresh_token"));
      if (!email) return json(res, 400, { error: "invalid_grant" });
    } else return json(res, 400, { error: "unsupported_grant_type" });
    const rt = b64u(randomBytes(16));
    refresh.set(rt, email);
    return json(res, 200, { access_token: b64u(randomBytes(16)) + "." + Buffer.from(email).toString("base64url"), token_type: "Bearer",
      expires_in: 3600, refresh_token: rt, id_token: idToken(email, clientId, nonce), scope: "openid email profile" });
  }
  if (url.pathname === "/userinfo") {
    const tok = /^Bearer (.+)$/.exec(req.headers.authorization ?? "");
    const email = tok && Buffer.from(tok[1].split(".")[1] ?? "", "base64url").toString();
    if (!email || !users[email]) return json(res, 401, { error: "invalid_token" });
    return json(res, 200, { sub: users[email].sub, email, email_verified: true, ...(users[email].hd ? { hd: users[email].hd } : {}) });
  }
  json(res, 404, { error: "not found" });
}).listen(Number(port), host, () => console.log(`mock google on ${issuer}`));
