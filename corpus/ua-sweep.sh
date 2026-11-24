#!/usr/bin/env bash
# Sweep upstream User-Agents through the Worker: one `wrangler dev` run
# per UA, a full corpus run against each, then a compare of every UA
# against the first.
#
#   ua-sweep.sh [--remote] [--list FILE] <ua> [<ua> ...]
#
# --local (default) uses this machine's egress; --remote runs the Worker
# on Cloudflare's edge (needs `wrangler login`; nothing is deployed).
# Output lands in .cache/corpus/ua-<timestamp>/ (gitignored via .cache/).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
worker="$root/crates/corx-worker"
mode="--local"
list=""

while [ $# -gt 0 ]; do
    case "$1" in
    --remote) mode="--remote"; shift ;;
    --local) mode="--local"; shift ;;
    --list) list="${2:?--list needs a file}"; shift 2 ;;
    --) shift; break ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) break ;;
    esac
done
if [ $# -lt 1 ]; then
    echo "usage: ua-sweep.sh [--remote] [--list FILE] <ua> [<ua> ...]" >&2
    exit 2
fi

out="$root/.cache/corpus/ua-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$out"
server_pid=""
server_log=""
persist=""

cleanup() {
    if [ -n "$server_pid" ]; then
        # wrangler runs in its own process group (see `set -m` below):
        # signal the whole group so node/workerd children die with it.
        kill -- "-$server_pid" 2>/dev/null || kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
        server_pid=""
    fi
    if [ -n "$persist" ]; then
        rm -rf "$persist"
        persist=""
    fi
}
trap cleanup EXIT

wait_ready() {
    local url="$1"
    for _ in $(seq 1 600); do
        if curl -s -o /dev/null "$url/" 2>/dev/null; then
            return 0
        fi
        if [ -n "$server_pid" ] && ! kill -0 "$server_pid" 2>/dev/null; then
            echo "worker died; log follows" >&2
            cat "$server_log" >&2 2>/dev/null || true
            return 1
        fi
        sleep 0.5
    done
    echo "worker did not become ready; log follows" >&2
    cat "$server_log" >&2 2>/dev/null || true
    return 1
}

free_port() {
    python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

cargo build --release --manifest-path "$root/Cargo.toml" -p corx-corpus
corpus="$root/target/release/corx-corpus"

set -m
i=0
for ua in "$@"; do
    i=$((i + 1))
    port="$(free_port)"
    persist="$(mktemp -d)"
    server_log="$(mktemp)"
    (
        cd "$worker"
        npx --yes wrangler@4.164.0 dev "$mode" --port "$port" \
            --var "ALLOWED_ORIGINS:*" \
            --var "USER_AGENT:$ua" \
            --persist-to "$persist" \
            >"$server_log" 2>&1
    ) &
    server_pid=$!
    echo "=== [$i] UA: $ua (port $port)"
    wait_ready "http://127.0.0.1:$port"
    "$corpus" run "http://127.0.0.1:$port" \
        ${list:+--list "$list"} \
        --json "$out/$i.json" | tee "$out/$i.txt"
    cleanup
done
set +m

echo
echo "=== UA sweep summary"
printf '%-70s  %-6s  %-6s  %-8s  %-8s\n' "user-agent" "usable" "pass" "p50" "p95"
for n in $(seq 1 "$i"); do
    ua="$(printf '%s\n' "${@:$n:1}")"
    python3 - "$out/$n.json" "$ua" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
ua = sys.argv[2]
lat = doc["totals"]["latencyMs"]
usable = sum(
    1 for r in doc["records"]
    if r.get("kind") == "media" or ("fields" in r and ("T" in r["fields"] or "I" in r["fields"]))
)
print(f'{ua[:70]:<70}  {usable:<6}  {doc["totals"]["pass"]:<6}  {lat["p50"]:<8}  {lat["p95"]:<8}')
PY
done

for n in $(seq 2 "$i"); do
    echo
    echo "=== compare: UA 1 vs UA $n"
    "$corpus" compare "$out/1.json" "$out/$n.json"
done

echo
echo "results: $out"
