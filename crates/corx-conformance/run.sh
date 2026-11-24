#!/usr/bin/env bash
# Run the black-box conformance suite against one edition.
#
#   run.sh native   build corx-server and run it with conformance flags
#                   (Origin allowlist + a 5-request client-IP bucket)
#   run.sh worker   serve crates/corx-worker under `wrangler dev --local`
#                   with a generated conformance config (IP_LIMITER = 5)
#   run.sh remote   run the suite against an already-running service;
#                   CORX_URL must be set (e.g. http://host:port)
set -euo pipefail

edition="${1:?usage: run.sh <native|worker|remote>}"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
tmpdir="$(mktemp -d)"
server_pid=""
server_log=""
generated_config=""

cleanup() {
    if [ -n "$server_pid" ]; then
        # The server runs in its own process group (see `set -m`
        # below): signal the whole group, so wrangler's node and
        # workerd children die with it instead of outliving the run.
        kill -- "-$server_pid" 2>/dev/null || kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    if [ -n "$generated_config" ]; then
        rm -f "$generated_config"
    fi
    rm -rf "$tmpdir"
}
trap cleanup EXIT

if [ "$edition" = "remote" ]; then
    : "${CORX_URL:?remote mode needs CORX_URL set to a running service}"
else
    port="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
    export CORX_URL="http://127.0.0.1:$port"
fi

wait_ready() {
    local log="$1"
    for _ in $(seq 1 600); do
        # Any HTTP answer means the server is up; the worker edition
        # has no /healthz and renders the shared 404 instead.
        if curl -s -o /dev/null "$CORX_URL/" 2>/dev/null; then
            return 0
        fi
        if [ -n "$server_pid" ] && ! kill -0 "$server_pid" 2>/dev/null; then
            echo "server died; log follows" >&2
            cat "$log" >&2 2>/dev/null || true
            return 1
        fi
        sleep 0.5
    done
    echo "server did not become ready; log follows" >&2
    cat "$log" >&2 2>/dev/null || true
    return 1
}

start_server() {
    server_log="$tmpdir/server.log"
    case "$edition" in
    native)
        "$root/target/release/corx" \
            --bind "127.0.0.1:$port" \
            --allowed-origins "https://app.example" \
            --client-ip-source CfConnectingIp \
            --ip-burst 5 \
            --ip-rate-per-minute 1 \
            >"$server_log" 2>&1 &
        server_pid=$!
        ;;
    worker)
        # A generated copy of wrangler.jsonc with a tight IP_LIMITER so
        # the rate-limit case deterministically drains one bucket. It
        # lives beside wrangler.jsonc so relative main/build paths keep
        # resolving; gitignored, never deployed.
        generated_config="$root/crates/corx-worker/.wrangler.conformance.jsonc"
        awk '
            /"name": "IP_LIMITER"/ { in_ip = 1 }
            in_ip && /"limit":/ { sub(/"limit": [0-9]+/, "\"limit\": 5"); in_ip = 0 }
            { print }
        ' "$root/crates/corx-worker/wrangler.jsonc" >"$generated_config"
        (
            cd "$root/crates/corx-worker"
            # Persist into the run's temp dir so a stale local cache can
            # never leak entries between conformance runs.
            npx --yes wrangler@4.164.0 dev --local --port "$port" \
                -c .wrangler.conformance.jsonc \
                --var "ALLOWED_ORIGINS:https://app.example" \
                --persist-to "$tmpdir/persist" \
                >"$server_log" 2>&1
        ) &
        server_pid=$!
        ;;
    esac
    wait_ready "$server_log"
}

run_suite() {
    if ! (cd "$root" && cargo test -p corx-conformance -- --ignored --test-threads 8); then
        if [ -n "$server_log" ]; then
            echo "server log follows" >&2
            cat "$server_log" >&2
        fi
        return 1
    fi
}

set -m
case "$edition" in
remote)
    ;;
native)
    cargo build --release --manifest-path "$root/Cargo.toml" -p corx-server
    ;;
worker)
    ;;
*)
    echo "usage: run.sh <native|worker|remote>" >&2
    exit 2
    ;;
esac

if [ "$edition" != "remote" ]; then
    start_server
fi
run_suite
set +m
