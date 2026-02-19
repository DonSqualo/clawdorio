# PR Creation Agent Flow (internal/pr)

This document describes the `internal/pr` path used by the Clawdorio run pipeline.

## Existing PR creation path

Feature runs enqueue these steps:

1. `feature-dev/planner`
2. `feature-dev/setup`
3. `feature-dev/developer`
4. `feature-dev/verifier`
5. `feature-dev/tester`
6. `internal/pr`
7. `feature-dev/reviewer`

Step `internal/pr` is the only place that creates/pushes PR state (`gh pr create`, branch push, etc.).

## Auto-rebase uses the same `internal/pr` path

Auto-rebase does **not** run ad-hoc shell jobs outside the run pipeline. It enqueues an `auto-rebase` run whose single step is:

- `step_id=auto-rebase`
- `agent_id=internal/pr`

`internal/pr` switches behavior by `context_json.action`:

- default: PR creation flow
- `action=auto_rebase_sweep`: rebase flow

## Triggers

### Webhook trigger

`POST /api/github/webhook` accepts GitHub webhook payloads and queues a base sweep for:

- `push` on default branch
- `pull_request` actions: `synchronize`, `opened`, `reopened`, `closed` when merged

### Periodic fallback reconciler

Runloop periodically checks each base repo’s upstream default-branch HEAD (`git ls-remote`) and queues a sweep if upstream moved and interval elapsed.

This is the safety net when webhook delivery is missed.

## Base settings

Stored in base `payload_json`:

- `auto_rebase_enabled` (bool)
- `auto_rebase_interval_sec` (int, >=30)

Defaults are code-defined when absent.

## Rebase sequence (inside `internal/pr`)

For eligible open PR branches (currently `clawdorio/*`):

1. `git fetch origin`
2. `git checkout <branch>`
3. `git rebase origin/<default>`
4. run checks if configured in future payload/pipeline extensions
5. `git push --force-with-lease origin <branch>`

Results are written to `event_log` (`auto_rebase.result`) and step/run status.

## Safety controls

- Idempotency guard: avoid duplicate queued/running `auto-rebase` runs per base.
- Interval/backoff guard from payload timestamps.
- Conflict handling: `git rebase --abort`, step fails with actionable `needs-attention` output.
- Bounded retries tracked in run context (`auto_rebase_attempt`, max configured in code).

## Requirements / limits

- `gh` CLI installed and authenticated (`gh auth status`) for PR discovery.
- Base repo must be a valid git repo with `origin` configured.
- Only branches associated with Clawdorio naming are auto-rebased.
- Force push is done with `--force-with-lease` only.
