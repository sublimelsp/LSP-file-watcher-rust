#!/usr/bin/env bash
# perf_check.sh — assert that registering a watcher on a realistic LSP-sized
# tree completes promptly. Catches regressions like the FSEvents per-directory
# stream-restart issue (sublimelsp/LSP-file-watcher-rust#3) where macOS
# registration ballooned to >30 s.
#
# Strategy:
#   1. Clone sublimelsp/LSP at a pinned commit (~289 files / 39 dirs).
#   2. Pipe a single register message to rust-watcher and immediately close
#      stdin. The binary processes register synchronously, then exits on EOF.
#      Wall time is therefore an upper bound on register cost.
#   3. Repeat a few times and fail if the best run exceeds the threshold.
#
# Works on: Linux, macOS, Windows (Git Bash).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUST_BIN="$REPO_ROOT/target/release/rust-watcher"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) RUST_BIN="$RUST_BIN.exe" ;;
esac

# Pinned LSP commit for reproducibility.
LSP_REPO="https://github.com/sublimelsp/LSP.git"
LSP_PIN="def3cf814b275c17f710306612138592efe7b764"
WORKDIR="${TMPDIR:-/tmp}/lspfw-perf-$LSP_PIN"

# Per-platform threshold (seconds). Generous enough to absorb CI noise but
# tight enough to catch the FSEvents per-dir regression (which is >30 s).
THRESHOLD=5

if [[ ! -x "$RUST_BIN" ]]; then
  echo "perf_check: $RUST_BIN not found — build the release binary first" >&2
  exit 1
fi

if [[ ! -d "$WORKDIR/.git" ]]; then
  echo "perf_check: cloning LSP at $LSP_PIN..."
  rm -rf "$WORKDIR"
  git clone --quiet "$LSP_REPO" "$WORKDIR"
  git -C "$WORKDIR" checkout --quiet "$LSP_PIN"
fi

REG=$(printf '{"register":{"uid":1,"cwd":"%s","patterns":["**/*"],"events":["create","change","delete"],"ignores":[]}}' "$WORKDIR")

# Pick the best of N runs to filter transient noise (cold-cache, scheduler).
RUNS=3
best_ms=999999
for i in $(seq 1 "$RUNS"); do
  t_start=$(python3 -c 'import time; print(int(time.monotonic()*1000))')
  printf '%s\n' "$REG" | "$RUST_BIN" >/dev/null 2>&1
  t_end=$(python3 -c 'import time; print(int(time.monotonic()*1000))')
  elapsed=$(( t_end - t_start ))
  echo "perf_check: run $i — ${elapsed} ms"
  if (( elapsed < best_ms )); then
    best_ms=$elapsed
  fi
done

threshold_ms=$(( THRESHOLD * 1000 ))
if (( best_ms > threshold_ms )); then
  echo "perf_check: FAIL — best register time ${best_ms} ms exceeds threshold ${threshold_ms} ms" >&2
  exit 1
fi

echo "perf_check: OK — best register time ${best_ms} ms (threshold ${threshold_ms} ms)"
