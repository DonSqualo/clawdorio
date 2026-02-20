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
    "$CLI" dev start --open
    ;;
  stop)
    "$CLI" dev stop
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
