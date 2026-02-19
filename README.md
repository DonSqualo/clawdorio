# Clawdorio

Tauri desktop shell + headless Rust engine/API (SQLite-backed).

## Headless hosting (Docker)

Pull prebuilt image (GitHub Container Registry):

```bash
docker pull ghcr.io/donsqualo/clawdorio:latest
```

Build:

```bash
docker build -t clawdorio .
```

Run (persists SQLite DB under local `./data`):

```bash
mkdir -p data
docker run --rm -p 39333:39333 -v "$PWD/data:/home/clawdorio/data" clawdorio
```

Health check:

```bash
curl http://127.0.0.1:39333/health
```

## Headless hosting (native)

```bash
cargo run -p clawdorio-server -- --host 0.0.0.0 --port 39333
```

## Desktop dev (Tauri)

```bash
npm install
npm run tauri dev
```


## Mobile PR feed + comment/reemit API

- `GET /api/pr-feed?base_id=<base-id>&limit=30`
  - Lists PR-linked feature runs for swipe cards.
  - Includes run/factory/base linkage, PR URL/number, branch, status, updated time, and changed-files summary.
  - Changed-file summary is sourced from `gh pr view --json files` when available; otherwise returns a fallback warning (no hard failure).
- `GET /api/pr-feed/{run_id}/files?max_patch_chars=1600`
  - Returns per-file additions/deletions and a diff snippet for the PR linked to the run.
  - Returns actionable dependency errors (`gh_missing`, `github_auth_required`, `github_permission_required`) via `424 Failed Dependency` when GitHub CLI/auth is unavailable.
- `POST /api/prs/comment`
  - Body: `{ run_id?|pr_url?|pr_number?, comment, idempotency_key? }`
  - Persists `pr.comment.reemit` event in `event_log`, then triggers worker reemit (base-scoped when factory/base link exists).
  - Safety controls: idempotency key replay suppression + short per-base rate guard to prevent spam reemit loops.
  - Returns actionable `404 no_linked_factory_or_run` when no run/factory can be resolved.

## Internal pipeline docs

- PR creation + auto-rebase path: `crates/server/PR_CREATION_AGENT.md`
