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

## Deterministic Library artifact API

- `POST /api/library/artifacts/rebuild`
  - Trigger deterministic markdown rebuild for an agent/base/run.
- `GET /api/library/artifacts/latest?agent_id=<agent-id>&base_id=<base-id?>&run_id=<run-id?>`
  - Fetch latest artifact document + metadata.
- `GET /api/library/artifacts?agent_id=<agent-id?>&base_id=<base-id?>&run_id=<run-id?>&limit=30`
  - List artifact versions.
- `GET /api/library/memory?agent_id=<agent-id?>&base_id=<base-id?>&run_id=<run-id?>&limit=40&before_created_at_ms=<cursor-ts?>&before_id=<cursor-id?>`
  - Memory Inspector list API (deterministic ordering: `created_at_ms DESC, id DESC`) with summary metadata (`source`, timestamp, size, hash/version, scope/tags).
- `GET /api/library/memory/{id}`
  - Fetch one memory record detail (`id` accepts `artifact:<artifact-id>` or raw artifact id).
  - Response payload is capped to avoid oversized UI render hangs (document markdown truncated at 50k chars).

See `crates/server/LIBRARY_ARTIFACTS.md` for hierarchy schema and skill append semantics.

## Skill Graph / University APIs

Scope model: `global -> base -> agent` (agent = specialization). University now assigns skill graph nodes to these scopes; Library remains the inspectable artifact sink in-world.

### Endpoints

- `POST /api/skills/import`
  - Body: `{ pack_name, source_root, index_path, graph_id?, title? }`
  - Imports markdown skill graph from index wikilinks + linked node files.
- `GET /api/skills/graphs`
- `GET /api/skills/nodes?graph_id=<id>`
- `GET /api/skills/assignments?scope_kind=global|base|agent&scope_ref=<optional>`
- `POST /api/skills/assign`
  - Body: `{ graph_id, node_id, scope_kind, scope_ref? }`
- `POST /api/skills/unassign`
- `GET /api/skills/preview?run_id=<id>&step_id=<id>&query=<optional>&max_depth=2&max_nodes=8`
  - Resolves by precedence `global -> base -> agent`, then bounded traversal with relevance scoring.
- `POST /api/skills/cli`
  - Body: `{ action: "install"|"update", package }`
  - Adapter for `skills` CLI, fallback `clawhub skills`; logs command output into `event_log`.

### Graph format

- Index markdown references nodes via wikilinks: `[[NodeName]]`
- Node markdown files (`NodeName.md`) support YAML frontmatter:
  - `title`
  - `description`
- Wikilinks inside node bodies build graph edges.

## Internal pipeline docs

- PR creation + auto-rebase path: `crates/server/PR_CREATION_AGENT.md`
