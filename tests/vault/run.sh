#!/usr/bin/env bash
# VaultStore against a real Vault dev server, set up like production:
# KV v2 at kv/, policy tests/vault/policy.hcl, AppRole login.
# Needs VAULT_BIN (vault binary).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
work="$(mktemp -d)"
"${VAULT_BIN:?}" server -dev -dev-root-token-id=root -dev-listen-address=127.0.0.1:38200 > "$work/vault.log" 2>&1 &
pid=$!
trap 'kill $pid; rm -rf "$work"' EXIT
export VAULT_ADDR=http://127.0.0.1:38200 VAULT_TOKEN=root
for _ in $(seq 1 40); do "$VAULT_BIN" status >/dev/null 2>&1 && break; sleep 0.25; done
# The dev server mounts KV v2 at secret/; production uses kv/.
"$VAULT_BIN" secrets enable -path=kv -version=2 kv >/dev/null
"$VAULT_BIN" policy write traum-haft-gateway "$here/policy.hcl" >/dev/null
"$VAULT_BIN" auth enable approle >/dev/null
"$VAULT_BIN" write auth/approle/role/traum-haft-gateway token_policies=traum-haft-gateway token_ttl=20m >/dev/null
export TH_VAULT_ROLE_ID="$("$VAULT_BIN" read -field=role_id auth/approle/role/traum-haft-gateway/role-id)"
export TH_VAULT_SECRET_ID="$("$VAULT_BIN" write -f -field=secret_id auth/approle/role/traum-haft-gateway/secret-id)"
export TH_VAULT_ADDR="$VAULT_ADDR"
"$VAULT_BIN" kv put kv/other/secret value=not-for-the-gateway >/dev/null
cd "$root" && cargo test -q -p traum-haft-gateway --test vault -- --ignored --nocapture
