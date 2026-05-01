#!/usr/bin/env bash
# parity_check.sh — compare rust-watcher output against chokidar 3 (91c7b45)
#
# Usage:
#   ./scripts/parity_check.sh [--no-build]
#
# Requires: cargo, node (>=12), git
# Works on: Linux, macOS

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUST_BIN="$REPO_ROOT/target/release/rust-watcher"
CHOKIDAR_WORKTREE="/tmp/lspfw-91c7b45"
CHOKIDAR_CLI="$CHOKIDAR_WORKTREE/chokidar/chokidar-cli/index.js"

# ── Build ─────────────────────────────────────────────────────────────────────

if [[ "${1:-}" != "--no-build" ]]; then
  echo "[1/3] Building rust-watcher..."
  cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" 2>&1 | tail -3
fi

# ── Prepare chokidar worktree ─────────────────────────────────────────────────

if [[ ! -f "$CHOKIDAR_CLI" ]]; then
  echo "[2/3] Setting up chokidar worktree at 91c7b45..."
  if [[ -d "$CHOKIDAR_WORKTREE" ]]; then
    rm -rf "$CHOKIDAR_WORKTREE"
  fi
  git -C "$REPO_ROOT" worktree add "$CHOKIDAR_WORKTREE" 91c7b45
  (cd "$CHOKIDAR_WORKTREE/chokidar" && npm install --silent)
else
  echo "[2/3] Chokidar worktree already present."
fi

# ── Run parity test ───────────────────────────────────────────────────────────

echo "[3/3] Running parity test..."

TMPDIR_RUST=$(mktemp -d)
TMPDIR_CHOK=$(mktemp -d)
RUST_OUT=$(mktemp)
CHOK_OUT=$(mktemp)
trap 'rm -rf "$TMPDIR_RUST" "$TMPDIR_CHOK" "$RUST_OUT" "$CHOK_OUT"' EXIT

UID_VAL=42

REG_RUST=$(printf '{"register":{"uid":%d,"cwd":"%s","patterns":["**/*"],"events":["create","change","delete"],"ignores":["**/.git/**"]}}\n' \
  "$UID_VAL" "$TMPDIR_RUST")
REG_CHOK=$(printf '{"register":{"uid":%d,"cwd":"%s","patterns":["**/*"],"events":["create","change","delete"],"ignores":["**/.git/**"]}}\n' \
  "$UID_VAL" "$TMPDIR_CHOK")

# Drive function: produce filesystem events in the given dir.
# Uses only $dir for temp files so parallel invocations don't collide.
drive() {
  local dir="$1"
  sleep 0.6
  touch "$dir/a.txt"
  sleep 0.7
  echo hello > "$dir/a.txt"
  sleep 0.7
  mv "$dir/a.txt" "$dir/b.txt"
  sleep 0.7
  rm "$dir/b.txt"
  sleep 0.7
  mkdir "$dir/sub"
  touch "$dir/sub/c.txt"
  sleep 0.7
  rm "$dir/sub/c.txt"
  sleep 0.7
  rmdir "$dir/sub"
  sleep 0.7
  # Atomic save-by-replace: write to a temp file then rename onto target.
  # Use a name matching chokidar's DOT_RE so it is auto-ignored on both sides
  # and only the final `change d.txt` surfaces.
  echo first > "$dir/d.txt"
  sleep 0.7
  echo second > "$dir/.d.txt.swp"
  mv "$dir/.d.txt.swp" "$dir/d.txt"
  sleep 0.7
  rm "$dir/d.txt"
  sleep 0.7
}

# Launch rust-watcher
(
  printf '%s\n' "$REG_RUST"
  drive "$TMPDIR_RUST"
) | timeout 20 "$RUST_BIN" 2>/dev/null > "$RUST_OUT" &
RUST_PID=$!

# Launch chokidar
(
  printf '%s\n' "$REG_CHOK"
  drive "$TMPDIR_CHOK"
) | timeout 20 node "$CHOKIDAR_CLI" 2>/dev/null > "$CHOK_OUT" &
CHOK_PID=$!

wait "$RUST_PID" "$CHOK_PID" || true

# Normalise: strip UID prefix (different dirs), strip <flush>, sort within each batch
normalise() {
  local file="$1"
  python3 - "$file" <<'EOF'
import sys, re

batches = []
current = []

for line in open(sys.argv[1]):
    line = line.rstrip()
    if line == '<flush>':
        if current:
            batches.append(sorted(current))
            current = []
    elif line:
        # strip uid: prefix and directory prefix, keep event:filename
        m = re.match(r'\d+:(create|change|delete):.*?([^/\\]+)$', line)
        if m:
            current.append(f"{m.group(1)}:{m.group(2)}")

if current:
    batches.append(sorted(current))

for b in batches:
    for e in b:
        print(e)
    print('---')
EOF
}

echo ""
echo "rust-watcher output:"
normalise "$RUST_OUT"
echo ""
echo "chokidar output:"
normalise "$CHOK_OUT"
echo ""

RUST_NORM=$(normalise "$RUST_OUT")
CHOK_NORM=$(normalise "$CHOK_OUT")

if [ "$RUST_NORM" = "$CHOK_NORM" ]; then
  echo "✓ PARITY OK — outputs match"
  exit 0
else
  echo "✗ PARITY MISMATCH"
  diff <(echo "$RUST_NORM") <(echo "$CHOK_NORM") || true
  exit 1
fi
