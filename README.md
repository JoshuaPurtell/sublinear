# sublinear

`sublinear` is a **dev-only**, Turso/libSQL-backed replacement for the subset of Linear GraphQL used by:

- `/Users/joshpurtell/Documents/Github/synth-background`
- `/Users/joshpurtell/Documents/Github/synth-managed-research`

It is intentionally narrow and intentionally not production-grade.

## Not For Production

- No OAuth flows
- Minimal auth checks
- No permission model
- No SLA / durability guarantees beyond your Turso/local setup

Use it only for local/integration testing.

## What It Implements

Queries:
- `viewer`
- `teams`
- `team(id)`
- `projects`
- `project(id)`
- `issue(id)`
- `issues(...)` with the currently used filters (`eq`, `neq`, `in`)
- `workflowStates(...)`

Mutations:
- `projectCreate`
- `issueCreate`
- `issueUpdate`
- `issueArchive`
- `issueAddLabel`
- `commentCreate`

This covers the API calls currently made by the Rust Linear clients in `synth-background` and `synth-managed-research`.

## Run

```bash
cargo run
```

Server defaults:
- GraphQL: `http://127.0.0.1:8787/graphql`
- Health: `http://127.0.0.1:8787/healthz`

## Environment

Copy `/Users/joshpurtell/Documents/Github/sublinear/.env.example` to `.env` (or export vars directly):

- `SUBLINEAR_PORT` (default `8787`)
- `SUBLINEAR_BASE_URL` (default `http://localhost:<port>`)
- `SUBLINEAR_REQUIRE_AUTH` (default `true`)
- `SUBLINEAR_API_KEY` (optional; if set, must match `Authorization` value or `Bearer <key>`)
- `TURSO_DATABASE_URL`:
  - local file path like `sublinear.db`, or
  - remote Turso URL like `libsql://...`
- `TURSO_AUTH_TOKEN` (required for remote Turso)

Seed defaults:
- `SUBLINEAR_SEED_VIEWER_NAME`
- `SUBLINEAR_SEED_VIEWER_EMAIL`
- `SUBLINEAR_SEED_TEAM_NAME`
- `SUBLINEAR_SEED_TEAM_KEY`

## Point Existing Apps To sublinear

For `synth-background`, set:

```bash
export SB_LINEAR_API_URL="http://127.0.0.1:8787/graphql"
export SB_LINEAR_API_KEY="dev-token"
```

For `synth-managed-research`, use the same token style already supported by its client (`raw` or `Bearer`) and wire endpoint in your local integration path to `http://127.0.0.1:8787/graphql`.

## Parity Tests

Run GraphQL parity checks for the exact query/mutation shapes currently used by `synth-background` and `synth-managed-research`:

```bash
./scripts/parity_test.sh
```

The script boots `sublinear`, executes those operations end-to-end, and fails fast on any GraphQL errors or shape regressions.

## License

MIT
