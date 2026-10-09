#!/usr/bin/env bash
# The runner against real podman and a real Caddy: deploy, health gate,
# switch, secrets, /data, edge secret, logs, restart, stop.
# Needs: podman, age, caddy (CADDY), cargo. Pulls one small base image.
# RUNNER_PODMAN: podman command the runner uses (e.g. a podman --remote
# wrapper, as on the worker where it talks to another user's podman).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"; root="$(cd "$here/../.." && pwd)"
caddy="${CADDY:-caddy}"
work="$(mktemp -d)"; pids=()
cleanup() {
  kill "${pids[@]}" 2>/dev/null || true
  podman ps -a --filter label=traum-haft.app=demo -q | xargs -r podman rm -f >/dev/null 2>&1 || true
  podman network rm -f werk-demo >/dev/null 2>&1 || true
  podman volume rm -f werk-demo-data >/dev/null 2>&1 || true
  podman images --format '{{.Repository}}:{{.Tag}}' | grep -E '^localhost/(werk|test)/demo:' | xargs -r podman rmi -f >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
pass=0; fail=0
check() { if eval "$2"; then echo "PASS  $1"; pass=$((pass+1)); else echo "FAIL  $1"; fail=$((fail+1)); fi; }
c() { curl -s --noproxy '*' "$@"; }

TOKEN=$(head -c 24 /dev/urandom | base64 | tr -d '/+='); TOKEN="${TOKEN}${TOKEN}"
EDGE=$(head -c 24 /dev/urandom | base64 | tr -d '/+='); EDGE="${EDGE}${EDGE}"
API=http://127.0.0.13:9000; PROXY=http://127.0.0.13:8080
auth=(-H "Authorization: Bearer $TOKEN")

echo "== images"
for v in 1 2 3; do
  broken=""; [ "$v" = 3 ] && broken=1
  podman build -q --no-cache --build-arg APP_VERSION=$v --build-arg BROKEN=$broken -t "localhost/test/demo:$v" "$here/app" >/dev/null
  podman save -q -o "$work/v$v.tar" "localhost/test/demo:$v"
done

(cd "$root" && cargo build -q -p traum-haft-runner)
echo '{"admin":{"listen":"127.0.0.13:2019","origins":["127.0.0.13:2019"]}}' > "$work/caddy.json"
"$caddy" run --config "$work/caddy.json" > "$work/caddy.log" 2>&1 & pids+=($!)
start_runner() {
  RUNNER_TOKEN=$TOKEN RUNNER_EDGE_SECRET=$EDGE RUNNER_DATA_DIR="$work/data" RUNNER_DOMAIN=werk.example.test \
  RUNNER_CADDY_ADMIN=http://127.0.0.13:2019 RUNNER_PROXY_LISTEN=127.0.0.13:8080 RUNNER_LISTEN=127.0.0.13:9000 \
  RUNNER_HEALTH_TIMEOUT_SECS=15 RUNNER_PODMAN="${RUNNER_PODMAN:-podman}" RUST_LOG=info "$root/target/debug/traum-haft-runner" >> "$work/runner.log" 2>&1 & pids+=($!)
  for _ in $(seq 60); do c -o /dev/null "$API/apps" && return; sleep 0.5; done; echo "runner did not start"; cat "$work/runner.log"; exit 1
}
start_runner

release() { # version, [with secrets]
  c -o /dev/null -w '%{http_code}' "${auth[@]}" -X PUT --data-binary @"$work/v$1.tar" "$API/apps/demo/releases/$1/image" >/dev/null
  if [ -n "${2:-}" ]; then
    echo '{"DEMO_SECRET":"geheim-'$1'"}' | age -r "$RECIPIENT" > "$work/s$1.age"
    c "${auth[@]}" -X PUT --data-binary @"$work/s$1.age" "$API/apps/demo/releases/$1/secrets"
  fi
  c "${auth[@]}" -X POST -H 'content-type: application/json' -d '{"port":8000,"health":"/healthz","memory":"128m"}' "$API/apps/demo/releases/$1/activate"
}
app() { c -H "Host: demo.werk.example.test" -H "X-Traum-Haft-Edge: $EDGE" -H "X-User-Email: a@isp-insoft.de" "$PROXY/" "$@"; }
field() { python3 -c "import json,sys; print(json.load(sys.stdin).get('$1'))"; }

# A stale tag from an earlier app of the same name must not win.
podman tag localhost/test/demo:3 localhost/werk/demo:v1
echo "== checks"
RECIPIENT=$(c "${auth[@]}" "$API/identity" | field recipient)
check "identity is an age recipient" '[[ "$RECIPIENT" == age1* ]]'
check "no token: 401" '[ "$(c -o /dev/null -w "%{http_code}" "$API/apps")" = 401 ]'
check "wrong token: 401" '[ "$(c -o /dev/null -w "%{http_code}" -H "Authorization: Bearer x$TOKEN" "$API/apps")" = 401 ]'
check "bad app name: 400" '[ "$(c -o /dev/null -w "%{http_code}" "${auth[@]}" -X POST "$API/apps/..%2Fx/stop")" != 204 ]'

r1=$(release 1 secrets)
check "v1 activates" '[ "$(echo "$r1" | field ok)" = True ]'
body=$(app)
check "v1 serves through the worker Caddy" '[ "$(echo "$body" | field version)" = 1 ]'
check "identity header reaches the app" '[ "$(echo "$body" | field email)" = a@isp-insoft.de ]'
check "edge secret does not reach the app" '[ "$(echo "$body" | field edge_header)" = None ]'
check "secret arrives as env" '[ "$(echo "$body" | field secret)" = geheim-1 ]'
check "platform env set" '[ "$(echo "$body" | field url)" = https://demo.werk.example.test ]'
check "secret not in podman args" '! podman inspect werk-demo-v1 --format "{{.Config.CreateCommand}}" | grep -q geheim'
check "without edge secret: refused" '[ "$(c -o /dev/null -w "%{http_code}" -H "Host: demo.werk.example.test" "$PROXY/")" = 404 ]'
check "wrong edge secret: refused" '[ "$(c -o /dev/null -w "%{http_code}" -H "Host: demo.werk.example.test" -H "X-Traum-Haft-Edge: nope" "$PROXY/")" = 404 ]'
check "other host: refused" '[ "$(c -o /dev/null -w "%{http_code}" -H "Host: other.werk.example.test" -H "X-Traum-Haft-Edge: $EDGE" "$PROXY/")" = 404 ]'

r2=$(release 2)
body=$(app)
check "v2 live" '[ "$(echo "$body" | field version)" = 2 ]'
check "/data survives the deploy" '[ "$(echo "$body" | field counter)" -ge 3 ]'
check "v1 container removed" '! podman container exists werk-demo-v1'

r3=$(release 3)
check "broken v3 refused with reason" '[ "$(echo "$r3" | field ok)" = False ] && echo "$r3" | grep -q "Start fehlgeschlagen"'
check "v2 still live after failed v3" '[ "$(app | field version)" = 2 ]'
check "broken container cleaned up" '! podman container exists werk-demo-v3'

logs=$(c "${auth[@]}" "$API/apps/demo/logs?lines=50")
check "logs show requests" 'echo "$logs" | grep -q "request / from a@isp-insoft.de"'

kill "${pids[1]}"; wait "${pids[1]}" 2>/dev/null || true
podman stop -t 2 werk-demo-v2 >/dev/null
start_runner
sleep 1
check "after runner restart: container back, routed" '[ "$(app | field version)" = 2 ]'

# A rebuilt worker: no containers, no images; the volume still holds the releases.
kill "${pids[-1]}"; wait "${pids[-1]}" 2>/dev/null || true
podman rm -f werk-demo-v2 >/dev/null; podman rmi -f localhost/werk/demo:v2 >/dev/null
start_runner
for _ in $(seq 40); do [ "$(app | field version 2>/dev/null)" = 2 ] && break; sleep 0.5; done
check "rebuilt worker: live release started again from the volume" '[ "$(app | field version)" = 2 ]'
check "rebuilt worker: /data still there" '[ "$(app | field counter)" -ge 4 ]'

c "${auth[@]}" -X POST "$API/apps/demo/stop" >/dev/null
check "stop: offline" '[ "$(c -o /dev/null -w "%{http_code}" -H "Host: demo.werk.example.test" -H "X-Traum-Haft-Edge: $EDGE" "$PROXY/")" = 404 ]'
check "stop: /data kept" 'podman volume exists werk-demo-data'
check "identity file private" '[ "$(stat -c %a "$work/data/identity.txt")" = 600 ]'

echo "== runner: $pass passed, $fail failed"
[ "$fail" = 0 ] || { tail -30 "$work/runner.log"; exit 1; }
