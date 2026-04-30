#!/usr/bin/env bash
# watchdog_check.sh — verify the parent-death watchdog terminates rust-watcher
# when its parent process dies without closing stdin.
#
# Strategy:
#   1. Spawn a wrapper shell that launches rust-watcher with stdin held open by
#      an unrelated subshell (`while sleep 60; do :; done`). When the wrapper
#      dies, the subshell survives, so stdin will *not* EOF — the only thing
#      that can terminate rust-watcher is the watchdog thread.
#   2. SIGKILL the wrapper. rust-watcher is now orphaned.
#   3. Verify the watchdog kills rust-watcher within a deadline.
#
# Works on: Linux, macOS, Windows (Git Bash).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUST_BIN="$REPO_ROOT/target/release/rust-watcher"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) RUST_BIN="$RUST_BIN.exe" ;;
esac

if [[ ! -x "$RUST_BIN" ]]; then
  echo "watchdog_check: $RUST_BIN not found — build the release binary first" >&2
  exit 1
fi

PIDFILE=$(mktemp)
WRAPPER_SCRIPT=$(mktemp)
trap 'rm -f "$PIDFILE" "$WRAPPER_SCRIPT"' EXIT

cat > "$WRAPPER_SCRIPT" <<EOF
#!/usr/bin/env bash
"$RUST_BIN" < <(while sleep 60; do :; done) &
echo \$! > "$PIDFILE"
wait
EOF
chmod +x "$WRAPPER_SCRIPT"

"$WRAPPER_SCRIPT" &
WRAPPER_PID=$!

# Wait for rust-watcher to start.
for _ in $(seq 1 50); do
  [[ -s "$PIDFILE" ]] && break
  sleep 0.1
done
RUST_PID=$(cat "$PIDFILE" 2>/dev/null || true)
if [[ -z "$RUST_PID" ]] || ! kill -0 "$RUST_PID" 2>/dev/null; then
  echo "watchdog_check: FAIL — rust-watcher did not start" >&2
  kill -9 "$WRAPPER_PID" 2>/dev/null || true
  exit 1
fi

# Orphan rust-watcher.
kill -9 "$WRAPPER_PID"
wait "$WRAPPER_PID" 2>/dev/null || true

# Watchdog must terminate rust-watcher within the deadline.
DEADLINE_S=5
START=$(date +%s)
while :; do
  if ! kill -0 "$RUST_PID" 2>/dev/null; then
    elapsed=$(( $(date +%s) - START ))
    echo "watchdog_check: OK — rust-watcher (pid $RUST_PID) exited ${elapsed}s after parent death"
    exit 0
  fi
  if (( $(date +%s) - START >= DEADLINE_S )); then
    echo "watchdog_check: FAIL — rust-watcher (pid $RUST_PID) still alive ${DEADLINE_S}s after parent death" >&2
    kill -9 "$RUST_PID" 2>/dev/null || true
    exit 1
  fi
  sleep 0.1
done
