# traum-haft

isp-insoft's internal app platform. Coworkers describe an app to a Claude
Cowork agent; the platform builds, hosts and secures it, and no secret ever
reaches the builder or the agent.

Architecture and decisions live in the design doc; infrastructure (OpenTofu,
Caddy, cloud-init) lives in `isp-insoft-gmbh/isp-insoft-cloud` under
`modules/traum-haft`.

## Components

| Path | What | Status |
| --- | --- | --- |
| `crates/gateway` | App tokens, OIDC discovery + JWKS, SpacetimeDB token-exchange check, on-demand TLS `ask` | prototype |
| integrations, connect pages | Per-user Jira / CRM Plus tokens in Vault | planned |
| platform MCP | Cowork-facing tools: create, edit, build, deploy, issues | planned |
| app template | Rust SpacetimeDB module + TypeScript frontend | planned |

## Gateway

Runs behind Caddy, which authenticates every request (oauth2-proxy, Google)
and sets `X-User-Sub`, `X-User-Email` and `X-App`. The gateway trusts only
those headers.

| Route | Purpose |
| --- | --- |
| `GET /.well-known/openid-configuration`, `/.well-known/jwks.json` | Issuer metadata SpacetimeDB uses to verify app tokens |
| `GET /_auth/token` | Short-lived ES256 token: `sub` = visitor, `aud` = app |
| `/_internal/stdb/verify` | `forward_auth` check before the SDK's token exchange: own signature, `aud` = this app, `sub` = this visitor |
| `/_internal/tls-ask?domain=` | Certificates only for deployed apps |

Configuration (environment): `GATEWAY_APPS_DOMAIN` (required),
`GATEWAY_ISSUER`, `GATEWAY_APPS_DIR`, `GATEWAY_KEY_FILE` (created with mode
0600 if missing), `GATEWAY_TOKEN_TTL_SECS`, `GATEWAY_LISTEN`.

```sh
cargo test
```
