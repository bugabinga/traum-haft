#!/usr/bin/env bash
# traum-haft end to end, on one machine, no real secrets:
#   mock Google -> real oauth2-proxy (x2) -> real Caddy (the infra module's
#   Caddyfile, local CA) -> gateway -> real SpacetimeDB -> template app,
#   driven by a real Chromium.
#
# Needs root (Caddy on 443), cargo + wasm32 target, node + global playwright,
# tofu, python3, openssl, and:
#   INFRA_REPO      isp-insoft-cloud checkout (default: ../isp-insoft-cloud next to this repo)
#   SPACETIME_BIN   dir with spacetimedb-cli + spacetimedb-standalone (v2.11.0)
#   OAUTH2_PROXY    oauth2-proxy binary (v7.15.5)
#   CADDY           caddy binary (>= 2.11.2)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
infra="${INFRA_REPO:-$root/../isp-insoft-cloud}"
stdb_bin="${SPACETIME_BIN:?}"; o2p="${OAUTH2_PROXY:?}"; caddy="${CADDY:-caddy}"
work="$(mktemp -d)"
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; [ -n "${KEEP:-}" ] && echo "kept $work" || rm -rf "$work"; }
trap cleanup EXIT
bg() { local log="$1"; shift; "$@" > "$work/$log.log" 2>&1 & pids+=($!); }
wait_for() { for _ in $(seq 1 60); do curl -sk -o /dev/null "$1" && return 0; sleep 0.5; done; echo "timeout: $1"; exit 1; }

export MOCK_GOOGLE=http://127.0.0.1:39300
issuer=http://127.0.0.2:8080

echo "== render the production Caddyfile from $infra"
cp "$infra/tests/traum-haft-edge/main.tf" "$work/render.tf"
mkdir -p "$work/render" && mv "$work/render.tf" "$work/render/main.tf"
sed -i "s#\${path.module}/../..#$infra#g" "$work/render/main.tf"
(cd "$work/render" && tofu init -input=false >/dev/null && tofu apply -auto-approve -input=false >/dev/null && tofu output -raw caddyfile > ../Caddyfile.prod)
mkdir -p "$work/apps" "$work/llms"
echo "llms e2e" > "$work/llms/llms.txt"
python3 "$here/to_https.py" "$work/Caddyfile.prod" "$work" > "$work/Caddyfile"

echo "== backends"
bg mock-google node "$here/mock-google.mjs" 127.0.0.1:39300
wait_for "$MOCK_GOOGLE/.well-known/openid-configuration"
# shellcheck source=oauth2-proxy.env.sh
(source "$here/oauth2-proxy.env.sh"; apps_proxy; exec "$o2p") > "$work/oauth2-proxy.log" 2>&1 & pids+=($!)
(source "$here/oauth2-proxy.env.sh"; connect_proxy; exec "$o2p") > "$work/oauth2-proxy-connect.log" 2>&1 & pids+=($!)

mkdir -p "$work/stdb/keys"
openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out "$work/stdb/keys/id_ecdsa" 2>/dev/null
openssl ec -in "$work/stdb/keys/id_ecdsa" -pubout -out "$work/stdb/keys/id_ecdsa.pub" 2>/dev/null
bg spacetimedb "$stdb_bin/spacetimedb-standalone" start --data-dir "$work/stdb/data" --listen-addr 127.0.0.4:3000 \
  --jwt-pub-key-path "$work/stdb/keys/id_ecdsa.pub" --jwt-priv-key-path "$work/stdb/keys/id_ecdsa" --non-interactive

(cd "$root" && cargo build -q --release -p traum-haft-gateway)
GATEWAY_APPS_DOMAIN=apps.isp-insoft.de GATEWAY_ISSUER="$issuer" GATEWAY_APPS_DIR="$work/apps" \
  GATEWAY_KEY_FILE="$work/gateway-key.pem" GATEWAY_LISTEN=127.0.0.2:8080 \
  bg gateway "$root/target/release/traum-haft-gateway"
wait_for http://127.0.0.4:3000/v1/ping
wait_for http://127.0.0.3:4180/ping
wait_for http://127.0.0.6:4180/ping
wait_for "$issuer/.well-known/jwks.json"

echo "== build and publish the template as apps 'notes' and 'other'"
for app in notes other; do
  (cd "$root/template/module" && TRAUM_HAFT_ISSUER="$issuer" TRAUM_HAFT_APP="$app" CARGO_TARGET_DIR="$work/target-$app" \
    cargo build -q --release --target wasm32-unknown-unknown)
  HOME="$work/clihome" "$stdb_bin/spacetimedb-cli" --root-dir "$work/cli" publish --server http://127.0.0.4:3000 \
    --bin-path "$work/target-$app/wasm32-unknown-unknown/release/app_module.wasm" "$app" -y > "$work/publish-$app.log" 2>&1 \
    || { cat "$work/publish-$app.log"; exit 1; }
done
(cd "$root/template/web" && npm ci --silent --no-audit --no-fund && npx vite build --logLevel error --outDir "$work/web-dist")
for app in notes other; do mkdir -p "$work/apps/$app" && cp -r "$work/web-dist" "$work/apps/$app/current"; done

echo "== edge"
bg caddy "$caddy" run --config "$work/Caddyfile" --adapter caddyfile
for _ in $(seq 1 60); do
  curl -sk -o /dev/null --resolve apps.isp-insoft.de:443:127.0.0.1 https://apps.isp-insoft.de/llms.txt && break; sleep 0.5
done

if [ -n "${HOLD:-}" ]; then echo "== stack up (HOLD), work dir $work"; sleep infinity; fi

echo "== browser"
# The browser must reach the local edge directly, not through an HTTP proxy
# from the environment (Playwright passes those on to Chromium).
PLAYWRIGHT_ROOT="$(npm root -g)" env -u HTTPS_PROXY -u https_proxy -u HTTP_PROXY -u http_proxy -u ALL_PROXY -u all_proxy \
  node "$here/browser.mjs"
