#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "[clawdorio] building server (release)"
cd "$repo_root"
cargo build -p clawdorio-server --release

echo "[clawdorio] installing binary to ~/.local/bin"
install -d "$HOME/.local/bin"
install -m 0755 "$repo_root/target/release/clawdorio-server" "$HOME/.local/bin/clawdorio-server"
install -m 0755 "$repo_root/scripts/clawdorio" "$HOME/.local/bin/clawdorio"

echo "[clawdorio] installing systemd user service"
install -d "$HOME/.config/systemd/user"
install -m 0644 "$repo_root/deploy/systemd-user/clawdorio.service" "$HOME/.config/systemd/user/clawdorio.service"

systemctl --user daemon-reload
systemctl --user enable --now clawdorio.service

echo "[clawdorio] service status:"
systemctl --user --no-pager --full status clawdorio.service || true

echo "[clawdorio] health:"
curl -fsS http://127.0.0.1:39333/health || true

cat <<'EOF'

Next (nginx):
  1) Install nginx: sudo pacman -S --needed nginx
  2) Copy config: sudo cp deploy/nginx/clawdorio.local.conf /etc/nginx/conf.d/clawdorio.conf
  3) Test: sudo nginx -t
  4) Start: sudo systemctl enable --now nginx
  5) Verify: curl http://127.0.0.1:8787/health

EOF
