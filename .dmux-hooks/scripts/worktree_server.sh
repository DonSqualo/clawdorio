#!/bin/bash
set -euo pipefail

ACTION="${1:-}"
if [ -z "$ACTION" ]; then
  echo "Usage: $0 <start|stop|status|logs>" >&2
  exit 1
fi

ROOT="${DMUX_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
CLI="$ROOT/clawdorio"

if [ ! -x "$CLI" ]; then
  CLI="$ROOT/scripts/clawdorio"
  if [ ! -x "$CLI" ]; then
    echo "[Hook] Missing executable CLI: $ROOT/clawdorio or $ROOT/scripts/clawdorio" >&2
    exit 1
  fi
fi

case "$ACTION" in
  start)
    URL="$("$CLI" dev start --open)"
    PORT="$(echo "$URL" | sed -E 's#.*:([0-9]+)$#\1#')"
    STATUS_CMD="./clawdorio dev status --slug ${DMUX_SLUG:-<worktree-slug>}"
    URL_CMD="./clawdorio dev url --slug ${DMUX_SLUG:-<worktree-slug>}"
    ALIAS_NAME="${DMUX_SLUG:-<worktree-slug>}-p${PORT}"
    ALIAS_WORKTREE="${DMUX_ROOT:-$(pwd)}/.dmux/worktrees/${ALIAS_NAME}"

    echo "[Hook] Clawdorio dev started at $URL (port $PORT)"
    echo "[Hook] Alias branch: $ALIAS_NAME"
    echo "[Hook] Alias worktree: $ALIAS_WORKTREE"
    echo "[Hook] Check status: $STATUS_CMD"
    echo "[Hook] Get URL: $URL_CMD"

    if command -v tmux >/dev/null 2>&1 && [ -n "${DMUX_TMUX_PANE_ID:-}" ]; then
      tmux send-keys -t "$DMUX_TMUX_PANE_ID" \
        "echo '[dmux hook] clawdorio dev: $URL (port $PORT)'" C-m
      tmux send-keys -t "$DMUX_TMUX_PANE_ID" \
        "echo '[dmux hook] alias branch: $ALIAS_NAME'" C-m
      tmux send-keys -t "$DMUX_TMUX_PANE_ID" \
        "echo '[dmux hook] alias worktree: $ALIAS_WORKTREE'" C-m
      tmux send-keys -t "$DMUX_TMUX_PANE_ID" \
        "echo '[dmux hook] status cmd: $STATUS_CMD'" C-m
      tmux send-keys -t "$DMUX_TMUX_PANE_ID" \
        "echo '[dmux hook] url cmd: $URL_CMD'" C-m
    fi
    ;;
  stop)
    "$CLI" dev stop
    echo "[Hook] Clawdorio dev stopped for slug ${DMUX_SLUG:-unknown}"
    ;;
  status)
    "$CLI" dev status
    ;;
  logs)
    "$CLI" dev logs
    ;;
  *)
    echo "Unknown action: $ACTION" >&2
    exit 1
    ;;
esac
