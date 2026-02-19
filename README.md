# sublinear

`sublinear` is a **dev-only**, Turso/libSQL-backed Linear GraphQL replacement.

Crate name on crates.io: `sublinear-dev` (library path: `sublinear_dev`).

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

This covers the currently implemented GraphQL surface.

## Run

```bash
cargo run
```

Server defaults:
- GraphQL: `http://127.0.0.1:8787/graphql`
- Health: `http://127.0.0.1:8787/healthz`

## Use As Dependency

```bash
cargo add sublinear-dev
```

Then in Rust:

```rust
use sublinear_dev::run_from_env;
```

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

## Point Your App To sublinear

Set your Linear GraphQL endpoint to:

```bash
export LINEAR_API_URL="http://127.0.0.1:8787/graphql"
export LINEAR_API_KEY="dev-token"
```

## Parity Tests

Run GraphQL parity checks for the implemented query/mutation shapes:

```bash
./scripts/parity_test.sh
```

The script boots `sublinear`, executes those operations end-to-end, and fails fast on any GraphQL errors or shape regressions.

To compare shape parity directly against real Linear:

```bash
./scripts/parity_with_real_linear.sh
```

## Real Project Sync (1:1 IDs)

To copy real Linear projects into `sublinear` with exact project IDs and metadata (`id`, `name`, `slugId`, `state`, `archivedAt`, `url`):

```bash
./scripts/sync_projects_from_linear.sh
```

This requires:
- `sublinear` running (`SUBLINEAR_GRAPHQL_URL`, default `http://127.0.0.1:8787/graphql`)
- Linear API key in `REAL_LINEAR_API_KEY` (or `LINEAR_API_KEY`)

## License

MIT
