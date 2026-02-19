# Skill Graph + skills/clawhub Adapter Notes

University skill graphs can be supplied by either:

- `skills install <pack>` / `skills update <pack>`
- `clawhub skills install <pack>` / `clawhub skills update <pack>`

Server adapter endpoint:

- `POST /api/skills/cli` with `{ "action": "install|update", "package": "..." }`

Flow:
1. install/update pack via CLI
2. import index graph via `POST /api/skills/import`
3. assign nodes by scope via `POST /api/skills/assign`
4. verify resolved prompt context via `GET /api/skills/preview`

Safety constraints:
- action allowlist: `install`, `update`
- direct process spawn without shell interpolation
- stdout/stderr logged to `event_log` (`kind='skills.cli'`)
