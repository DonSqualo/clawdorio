#!/bin/bash
set -euo pipefail

ROOT="${DMUX_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
HOOK_NAME="${DMUX_HOOK_NAME:-unknown_hook}"
SLUG="${DMUX_SLUG:-unknown_slug}"
PANE="${DMUX_PANE_ID:-unknown_pane}"
WORKTREE="${DMUX_WORKTREE_PATH:-}"
TS="$(date '+%Y-%m-%d %H:%M:%S')"

RUNTIME_DIR="$ROOT/.dmux-hooks/.runtime"
mkdir -p "$RUNTIME_DIR" "$ROOT/.dmux"

LINE="[$TS] hook=$HOOK_NAME pane=$PANE slug=$SLUG worktree=$WORKTREE"

printf '%s\n' "$LINE" >> "$ROOT/.dmux/hooks-fired.log"
printf '%s\n' "$LINE" > "$RUNTIME_DIR/hook-fired-last.txt"
printf '%s\n' "$LINE" >> "/tmp/dmux-hook-fired-${SLUG}.log"

if [ -n "$WORKTREE" ] && [ -d "$WORKTREE" ]; then
  printf '%s\n' "$LINE" >> "$WORKTREE/.dmux-hook-fired.log"
fi

if command -v tmux >/dev/null 2>&1 && [ -n "${DMUX_TMUX_PANE_ID:-}" ]; then
  tmux send-keys -t "$DMUX_TMUX_PANE_ID" "echo '[dmux hook fired] $LINE'" C-m
fi
