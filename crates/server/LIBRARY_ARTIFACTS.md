# Deterministic Library Artifacts

Library now stores a **single deterministic markdown artifact** per agent/work-item build.

## Hierarchy schema

Rendered hierarchy is stable and strict:

1. Root (`Library Artifact: <agent_id>`)
2. `Runs`
3. Per run (`Run <run_id>`) sorted ascending
4. `Context`
5. `Steps` sorted by `(step_index, step_id)`
6. `Skill Context (University -> Library)` appended after steps

Each node has:

- `title`
- `items[]` (sorted)
- `children[]` (sorted by title)

Renderer guarantees same input graph => same markdown bytes.

## Skill append semantics

Skill context blocks are resolved from:

- `context_json.skill_contexts[]`
- `input_json.skill_contexts[]`
- `assignment.skills[]` hooks

Each normalized block preserves provenance markers:

- `scope`
- `source`
- `node_ref`
- `title`
- `body`

Blocks are deterministically sorted by `(scope, source, node_ref, title, body)` and appended under `Skill Context (University -> Library)`.

## Storage

`library_artifacts` table stores:

- `agent_id`, `base_id`, `run_id`
- `source_event`
- `hierarchy_json` (canonical, sorted JSON)
- `document_md` (rendered deterministic markdown)
- `content_hash` (sha256 of markdown)
- `version`, `created_at_ms`

## API

- `POST /api/library/artifacts/rebuild`
  - body: `{ agent_id, base_id?, run_id?, source_event? }`
- `GET /api/library/artifacts/latest?agent_id=...&base_id?&run_id?`
- `GET /api/library/artifacts?agent_id?&base_id?&run_id?&limit?`
- `GET /api/library/memory?agent_id?&base_id?&run_id?&limit?&before_created_at_ms?&before_id?`
  - Memory Inspector list API. Returns deterministic, pagination-friendly records ordered by `created_at_ms DESC, id DESC`.
  - Fields include `source`, `timestamp_ms`, `size_bytes`, `content_hash`, `version`, `scope`, `tags`, and a short `summary`.
- `GET /api/library/memory/{id}`
  - Memory Inspector detail API. `id` supports `artifact:<artifact_id>` and raw `artifact_id`.
  - Detail payload caps markdown at 50k chars to protect UI rendering.

## Lifecycle linkage

Artifacts are generated on:

- run queued (`run.queued`)
- run done (`run.done`)
- run failed (`run.failed`)
- manual/UI rebuild (`manual.rebuild`, `ui.rebuild`)
