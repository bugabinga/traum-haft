#!/usr/bin/env bash
# Identity chain against a real SpacetimeDB: gateway token -> SpacetimeDB OIDC
# discovery -> what the module sees. Also documents what SpacetimeDB itself
# does NOT stop (foreign issuers, tokens for other apps), which is why the
# edge and gateway checks exist.
#
# Needs: cargo with wasm32-unknown-unknown, openssl, curl, python3, and the
# SpacetimeDB release binaries in $SPACETIME_BIN (spacetimedb-cli,
# spacetimedb-standalone), e.g. from
# https://github.com/clockworklabs/SpacetimeDB/releases/download/v2.11.0/spacetime-x86_64-unknown-linux-gnu.tar.gz
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
bin="${SPACETIME_BIN:?set SPACETIME_BIN to the directory with the SpacetimeDB binaries}"
work="$(mktemp -d)"
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT

stdb=127.0.0.1:39000; gw=127.0.0.1:39090; rogue=127.0.0.1:39091

# SpacetimeDB with its own signing keys.
mkdir -p "$work/keys" "$work/data"
openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out "$work/keys/id_ecdsa" 2>/dev/null
openssl ec -in "$work/keys/id_ecdsa" -pubout -out "$work/keys/id_ecdsa.pub" 2>/dev/null
"$bin/spacetimedb-standalone" start --data-dir "$work/data" --listen-addr "$stdb" \
  --jwt-pub-key-path "$work/keys/id_ecdsa.pub" --jwt-priv-key-path "$work/keys/id_ecdsa" --non-interactive \
  > "$work/stdb.log" 2>&1 & pids+=($!)

# Gateway plus a second, "rogue" issuer with its own key.
(cd "$root" && cargo build -q --release -p traum-haft-gateway)
start_gw() { # listen dir
  mkdir -p "$2/apps/foo"
  GATEWAY_APPS_DOMAIN=apps.example.test GATEWAY_ISSUER="http://$1" GATEWAY_APPS_DIR="$2/apps" \
  GATEWAY_KEY_FILE="$2/key.pem" GATEWAY_LISTEN="$1" "$root/target/release/traum-haft-gateway" > "$2/gw.log" 2>&1 & pids+=($!)
}
start_gw "$gw" "$work/gw"; start_gw "$rogue" "$work/rogue"

# Module.
(cd "$here/whoami" && CARGO_TARGET_DIR="$work/target" cargo build -q --release --target wasm32-unknown-unknown)
sleep 3
HOME="$work/home" "$bin/spacetimedb-cli" --root-dir "$work/cli" publish --server "http://$stdb" \
  --bin-path "$work/target/wasm32-unknown-unknown/release/whoami.wasm" foo -y > "$work/publish.log" 2>&1 \
  || { cat "$work/publish.log"; exit 1; }

tok() { curl -sf -H "X-User-Sub: $2" -H "X-User-Email: $2@example.test" -H "X-App: $3" "http://$1/_auth/token" \
  | python3 -c 'import sys, json; print(json.load(sys.stdin)["token"])'; }
call() { curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $1" -H 'Content-Type: application/json' \
  -d '[]' "http://$stdb/v1/database/foo/call/whoami"; }
seen() { curl -s -X POST -H "Authorization: Bearer $1" -d 'SELECT * FROM seen' "http://$stdb/v1/database/foo/sql" \
  | python3 -c 'import sys, json; print("\n".join("|".join([r[1][0], r[2], r[3], r[4]]) for r in json.load(sys.stdin)[0]["rows"]))'; }

pass=0; fail=0
check() { if [[ "$2" == "$3" ]]; then echo "PASS  $1"; pass=$((pass+1)); else echo "FAIL  $1 (got $2, want $3)"; fail=$((fail+1)); fi; }

alice=$(tok "$gw" alice foo)
check "gateway token accepted by SpacetimeDB" "$(call "$alice")" 200
check "renewed token accepted"                "$(call "$(tok "$gw" alice foo)")" 200
check "tampered signature refused"            "$(call "${alice%.*}.AAAA")" 401
check "token exchange (websocket-token)"      "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $alice" "http://$stdb/v1/identity/websocket-token")" 200
# What SpacetimeDB does NOT stop on its own (the edge and gateway must):
check "SpacetimeDB accepts a foreign issuer"  "$(call "$(tok "$rogue" alice foo)")" 200
check "SpacetimeDB accepts another app's token" "$(call "$(tok "$gw" alice bar)")" 200

rows="$(seen "$alice")"
check "module sees gateway issuer, sub, aud"  "$(sed -n 1p <<<"$rows" | cut -d'|' -f2-)" "http://$gw|alice|foo"
check "renewal keeps the identity"            "$(sed -n 2p <<<"$rows" | cut -d'|' -f1)" "$(sed -n 1p <<<"$rows" | cut -d'|' -f1)"
check "foreign issuer gets another identity"  "$([[ "$(sed -n 3p <<<"$rows" | cut -d'|' -f1)" != "$(sed -n 1p <<<"$rows" | cut -d'|' -f1)" ]] && echo yes)" yes
check "other app's token = SAME identity (aud not in identity)" "$(sed -n 4p <<<"$rows" | cut -d'|' -f1)" "$(sed -n 1p <<<"$rows" | cut -d'|' -f1)"

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
