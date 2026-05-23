#!/usr/bin/env bash
# wisp-net-test: orchestrate a server + N observer clients, emit per-process
# JSONL trace files, then exit when every observer's script reaches its
# `exit` command (or a timeout fires).
#
# Usage:
#   tools/net-test/run_session.sh <session_dir> <observer_count> [client_script...]
#
# Examples:
#   tools/net-test/run_session.sh /tmp/wisp-s1 1 tools/net-test/scripts/idle.script
#   tools/net-test/run_session.sh /tmp/wisp-s2 2 tools/net-test/scripts/throw.script tools/net-test/scripts/watcher.script
#
# After the run completes, the session dir contains:
#   server.jsonl                — server trace events
#   observer-<N>.jsonl          — per-observer trace events
#   server.log, observer-<N>.log — stderr from each process
#   summary.json                — counts of each event kind per source

set -uo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <session_dir> <observer_count> [observer_script...]" >&2
    exit 2
fi

session_dir="$1"; shift
n_observers="$1"; shift

mkdir -p "$session_dir"

# Resolve repo root from this script's location.
repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

# Build first so each spawned process exits fast on build failure.
echo "[wisp-net-test] building binaries…" >&2
if ! nix develop --command cargo build --quiet --bin server --bin observer 2>"$session_dir/build.log"; then
    echo "[wisp-net-test] build failed, see $session_dir/build.log" >&2
    exit 1
fi

cleanup() {
    local sig="${1:-}"
    if [[ -n "${server_pid:-}" ]]; then
        kill -TERM "$server_pid" 2>/dev/null || true
    fi
    for pid in "${observer_pids[@]}"; do
        kill -TERM "$pid" 2>/dev/null || true
    done
    if [[ -n "$sig" ]]; then
        exit 130
    fi
}
trap 'cleanup INT' INT TERM

# --- spawn server -----------------------------------------------------------

echo "[wisp-net-test] starting server (trace=$session_dir/server.jsonl)" >&2
WISP_TRACE_FILE="$session_dir/server.jsonl" \
WISP_TRACE_SRC="server" \
RUST_LOG="${RUST_LOG:-info}" \
    nix develop --command ./target/debug/server \
    >"$session_dir/server.log" 2>&1 &
server_pid=$!

# Give the server a moment to bind its socket.
sleep 1

if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "[wisp-net-test] server failed to start, see $session_dir/server.log" >&2
    exit 1
fi

# --- spawn observers --------------------------------------------------------

observer_pids=()
observer_scripts=("$@")

for ((i = 1; i <= n_observers; i++)); do
    script_arg="${observer_scripts[$((i - 1))]:-}"
    client_id=$((1000 + i))
    spawn_pos="$((i * 2 - n_observers - 1)),1.5,0"
    echo "[wisp-net-test] starting observer-$i (client_id=$client_id, script=$script_arg)" >&2
    if [[ -n "$script_arg" && -f "$script_arg" ]]; then
        export WISP_OBSERVER_SCRIPT="$(realpath "$script_arg")"
    else
        unset WISP_OBSERVER_SCRIPT
    fi
    WISP_TRACE_FILE="$session_dir/observer-$i.jsonl" \
    WISP_TRACE_SRC="observer-$i" \
    WISP_CLIENT_ID="$client_id" \
    WISP_SPAWN_POS="$spawn_pos" \
    RUST_LOG="${RUST_LOG:-info}" \
        nix develop --command ./target/debug/observer \
        >"$session_dir/observer-$i.log" 2>&1 &
    observer_pids+=("$!")
done

# --- wait for observers to finish ------------------------------------------

# Each observer is expected to call `exit` from its script. Wait with a
# timeout so a forgotten exit can't hang the harness.
timeout="${WISP_NET_TEST_TIMEOUT:-30}"
elapsed=0
while (( elapsed < timeout )); do
    all_done=1
    for pid in "${observer_pids[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            all_done=0
            break
        fi
    done
    if (( all_done == 1 )); then
        break
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done

# Kill any stragglers.
for pid in "${observer_pids[@]}"; do
    kill -TERM "$pid" 2>/dev/null || true
done
sleep 0.5
for pid in "${observer_pids[@]}"; do
    kill -KILL "$pid" 2>/dev/null || true
done

# --- stop server ------------------------------------------------------------

kill -TERM "$server_pid" 2>/dev/null || true
sleep 0.3
kill -KILL "$server_pid" 2>/dev/null || true
wait 2>/dev/null

# --- emit summary -----------------------------------------------------------

bash "$repo_root/tools/net-test/summarize.sh" "$session_dir" >"$session_dir/summary.json"
cat "$session_dir/summary.json"

echo "[wisp-net-test] session complete: $session_dir" >&2
