#!/usr/bin/env python3
"""leak_check.py — stress rust-watcher and sample its memory over time.

Reproduction harness for sublimelsp/LSP-file-watcher-rust#20 (memory growth
on Windows). Each scenario starts a fresh rust-watcher, registers several
overlapping watchers (like multiple LSP servers on one folder), drives a
file-system workload and samples private memory and handle count.

Usage: python scripts/leak_check.py [--duration SEC] [--limit-mb MB] [scenario ...]
Requires: psutil.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import psutil

REPO_ROOT = Path(__file__).resolve().parent.parent
RUST_BIN = REPO_ROOT / 'target' / 'release' / ('rust-watcher.exe' if os.name == 'nt' else 'rust-watcher')

PATTERNS = [['**/*'], ['**/*.py', '**/pyproject.toml'], ['**/*.md']]
IGNORES = ['**/.git/**', '**/__pycache__/**']


def make_tree(root: Path, dirs: int = 40, files_per_dir: int = 10) -> list[Path]:
    files = []
    for d in range(dirs):
        sub = root / f'pkg{d}' / 'sub'
        sub.mkdir(parents=True, exist_ok=True)
        for f in range(files_per_dir):
            p = sub / f'mod{f}.py'
            p.write_text(f'x = {f}\n')
            files.append(p)
    (root / '.git').mkdir(exist_ok=True)
    return files


class Watcher:
    def __init__(self) -> None:
        self.proc = subprocess.Popen(
            [str(RUST_BIN)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.ps = psutil.Process(self.proc.pid)
        self.lines = 0
        self.stderr: list[str] = []
        # Drain both pipes so a full pipe never blocks the watcher.
        threading.Thread(target=self._drain_stdout, daemon=True).start()
        threading.Thread(target=self._drain_stderr, daemon=True).start()

    def _drain_stdout(self) -> None:
        assert self.proc.stdout
        for _ in self.proc.stdout:
            self.lines += 1

    def _drain_stderr(self) -> None:
        assert self.proc.stderr
        for line in self.proc.stderr:
            if len(self.stderr) < 50:
                self.stderr.append(line.decode(errors='replace').rstrip())

    def send(self, msg: dict) -> None:
        assert self.proc.stdin
        self.proc.stdin.write((json.dumps(msg) + '\n').encode())
        self.proc.stdin.flush()

    def register(self, uid: int, cwd: Path, patterns: list[str]) -> None:
        self.send({'register': {'uid': uid, 'cwd': str(cwd), 'patterns': patterns,
                                'events': ['create', 'change', 'delete'], 'ignores': IGNORES}})

    def unregister(self, uid: int) -> None:
        self.send({'unregister': uid})

    def sample(self) -> tuple[float, int]:
        mem = self.ps.memory_info()
        private = getattr(mem, 'private', mem.rss)  # Windows: private bytes
        handles = self.ps.num_handles() if os.name == 'nt' else self.ps.num_fds()
        return private / 2**20, handles

    def close(self) -> None:
        try:
            self.proc.stdin.close()  # type: ignore[union-attr]
            self.proc.wait(5)
        except Exception:
            self.proc.kill()


# --- workloads: each is called repeatedly until the scenario ends ----------------------------------------------------

def wl_idle(root: Path, files: list[Path], w: Watcher, i: int) -> None:
    time.sleep(0.5)


def wl_file_churn(root: Path, files: list[Path], w: Watcher, i: int) -> None:
    for p in random.sample(files, 20):
        p.write_text(f'x = {i}\n')
    time.sleep(0.05)


def wl_atomic_save(root: Path, files: list[Path], w: Watcher, i: int) -> None:
    # Sublime-style save: write temp next to target, then replace.
    for p in random.sample(files, 20):
        tmp = p.with_name(p.name + '.subl1234.tmp')
        tmp.write_text(f'x = {i}\n')
        os.replace(tmp, p)
    time.sleep(0.05)


def wl_dir_churn(root: Path, files: list[Path], w: Watcher, i: int) -> None:
    # Like `uv venv` / git checkout: create a nested tree, then remove it.
    d = root / f'churn{i % 4}'
    if d.exists():
        shutil.rmtree(d, ignore_errors=True)
    else:
        for k in range(10):
            sub = d / f'lib{k}' / 'site'
            sub.mkdir(parents=True, exist_ok=True)
            for f in range(5):
                (sub / f'm{f}.py').write_text('pass\n')
    time.sleep(0.1)


def wl_git_like(root: Path, files: list[Path], w: Watcher, i: int) -> None:
    # Activity in ignored .git dir (index.lock create/rename/delete, objects).
    git = root / '.git'
    lock = git / 'index.lock'
    lock.write_bytes(os.urandom(256))
    os.replace(lock, git / 'index')
    obj = git / 'objects' / f'{i % 256:02x}'
    obj.mkdir(parents=True, exist_ok=True)
    (obj / f'{i:038x}').write_bytes(os.urandom(64))
    time.sleep(0.05)


def wl_reregister(root: Path, files: list[Path], w: Watcher, i: int) -> None:
    # LSP sessions restarting: unregister/register the same tree.
    uid = 100 + i
    w.register(uid, root, PATTERNS[0])
    time.sleep(0.2)
    w.unregister(uid)
    files[i % len(files)].write_text(f'x = {i}\n')
    time.sleep(0.1)


SCENARIOS = {
    'idle': wl_idle,
    'file_churn': wl_file_churn,
    'atomic_save': wl_atomic_save,
    'dir_churn': wl_dir_churn,
    'git_like': wl_git_like,
    'reregister': wl_reregister,
    # Large tree, no activity: on Windows each watched dir costs a handle + 16 KB notify buffer.
    'big_tree': wl_idle,
}


def run(name: str, duration: float, interval: float, big_dirs: int) -> dict:
    workload = SCENARIOS[name]
    # resolve(): expand macOS /var symlink and Windows 8.3 short names (RUNNER~1).
    root = Path(tempfile.mkdtemp(prefix=f'lspfw-leak-{name}-')).resolve()
    files = make_tree(root, dirs=big_dirs, files_per_dir=1) if name == 'big_tree' else make_tree(root)
    w = Watcher()
    for uid, pats in enumerate(PATTERNS, 1):
        w.register(uid, root, pats)
    time.sleep(1.0)

    samples: list[tuple[float, float, int]] = []
    start = time.monotonic()
    next_sample = start
    i = 0
    try:
        while (now := time.monotonic()) - start < duration:
            if w.proc.poll() is not None:
                print(f'  !! watcher exited with {w.proc.returncode}')
                break
            if now >= next_sample:
                mb, h = w.sample()
                samples.append((now - start, mb, h))
                print(f'  {name:12s} t={now - start:6.1f}s  private={mb:8.1f} MB  handles={h:6d}  out_lines={w.lines}',
                      flush=True)
                next_sample += interval
            workload(root, files, w, i)
            i += 1
    finally:
        w.close()
        shutil.rmtree(root, ignore_errors=True)

    if w.stderr:
        print('  stderr (first lines):\n    ' + '\n    '.join(w.stderr[:10]))
    if not samples:
        return {'name': name, 'start': 0, 'peak': 0, 'end': 0, 'handles_start': 0, 'handles_end': 0}
    return {
        'name': name,
        'start': samples[0][1],
        'peak': max(s[1] for s in samples),
        'end': samples[-1][1],
        'handles_start': samples[0][2],
        'handles_end': samples[-1][2],
        'iterations': i,
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument('scenarios', nargs='*', default=list(SCENARIOS))
    ap.add_argument('--duration', type=float, default=120)
    ap.add_argument('--interval', type=float, default=5)
    ap.add_argument('--limit-mb', type=float, default=200)
    ap.add_argument('--big-dirs', type=int, default=20000, help='big_tree: number of pkgN/sub dir pairs')
    args = ap.parse_args()

    if not RUST_BIN.exists():
        print(f'leak_check: {RUST_BIN} not found — build the release binary first', file=sys.stderr)
        return 1

    results = [run(s, args.duration, args.interval, args.big_dirs) for s in args.scenarios]

    print('\nSummary')
    print(f'{"scenario":12s} {"start MB":>9s} {"peak MB":>9s} {"end MB":>9s} {"handles":>15s} {"iters":>7s}')
    failed = False
    for r in results:
        bad = r['peak'] > args.limit_mb
        failed |= bad
        print(f'{r["name"]:12s} {r["start"]:9.1f} {r["peak"]:9.1f} {r["end"]:9.1f} '
              f'{r["handles_start"]:>6d} -> {r["handles_end"]:<6d} {r.get("iterations", 0):7d}'
              f'{"  <-- over limit" if bad else ""}')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
