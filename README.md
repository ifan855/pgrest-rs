# pgrest-rs

A lean, PostgREST-compatible REST API server for PostgreSQL, written in Rust.
Point it at a database and every table, view and function becomes an HTTP API —
with resource embedding, aggregates, row-level-security passthrough, OpenAPI
docs and Prometheus metrics.

Built for the low-RAM edge: **~2 MB static binary, ~4 MB resident RSS at idle
and under load.**

```
Requests:  curl "http://localhost:3000/api/films?select=title,actors(name),director:directors!director_id(name)&year=gte.2010&order=rating.desc"
```

## Quick start

```bash
# 1. Configure (env vars, PostgREST-compatible names)
export PGRST_DB_URI="postgresql://postgres@127.0.0.1:5432/appdb"
export PGRST_DB_SCHEMAS="api"            # exposed schemas (comma separated)
export PGRST_DB_ANON_ROLE="anon"         # role for unauthenticated requests
export PGRST_JWT_SECRET="..."            # optional, enables HS256 JWT auth
export PGRST_SERVER_PORT=3000

# 2. Run
cargo run --release
```

Docker-style single binary: `target/release/pgrest-rs` (static-ish, musl
build supported: `cargo build --release --target x86_64-unknown-linux-musl`).

## What is implemented

- **Auto REST API** for every table, view, materialized view, partitioned and
  foreign table in the exposed schemas: `GET`, `POST`, `PATCH`, `PUT`, `DELETE`
  on `/schema/table`
- **Vertical filtering**: `select=id,title,rating::text,aliased:year,data->>key`,
  spreads (`select=*`, `select=*,embed(...)`), aliases, casts and JSON arrows
- **Filter operators**: `eq neq gt gte lt lte like ilike match imatch in is
  isdistinct cs cd ov sl sr nxl nxr adj fts plfts phfts websearch`, plus
  `and=(...)` / `or=(...)` blocks, `not.` negation and `col=not.eq.x`
  (with `*` to `%` translation for like/ilike)
- **Ordering**: multi-key, `asc/desc`, `nullsfirst/nullslast`; grammar errors
  (unknown order keywords, negative limit/offset) are rejected as `PGRST100`
- **Pagination**: `limit/offset`, HTTP `Range: items=0-14` (implies exact
  count) and the `Range` + `Range-Unit: items` form, `Content-Range`
  responses, `206 Partial Content`, `416` on unsatisfiable ranges,
  `Prefer: count=exact|planned|estimated|none`, canonical `Content-Location`
- **Resource embedding**:
  - one-to-many, many-to-one, one-to-one (unique FK), many-to-many through
    auto-detected join tables
  - **computed relationships**: `fn(parent) returns setof child` functions
    exposed as embed relations
  - arbitrarily nested embeds, `!inner` and `!left` hints, aliases
    (`director:directors!director_id`), disambiguation hints
    (`films!director_id`, `films!constraint_name`, join-table hints)
  - embedded filters / order / limit / offset, including nested paths
    (`films.actors.name=eq.X`) and alias-keyed params
  - **aggregates on embeds**: `reviews(count)`, `reviews(sum(stars))`
- **Top-level aggregates**: `select=count(),sum(rating),avg(rating)` with
  automatic `GROUP BY` when mixed with plain columns, aggregate aliases and
  casts (`total:count()::text`), typed JSON output
- **Mutations**: single + bulk JSON insert, **CSV upload** (`Content-Type:
  text/csv`), `?columns=`, **upsert** (`Prefer: resolution=merge-duplicates /
  ignore-duplicates` + `?on_conflict=`), `Prefer: missing=default`,
  `Prefer: return=representation|minimal`, `Location` header on insert,
  **PUT upsert-by-primary-key**, and `select=` on any mutation to shape (and
  embed into) the returned rows
- **RPC**: every tracked function at `/rpc/<fn>`; POST (+GET for non-volatile),
  named args from JSON body and/or query string, default args, **function
  overloads resolved by provided arguments** (`PGRST203` on ambiguity),
  **procedures via `CALL`** (POST-only), return-shape mapping (void→204,
  scalar→text/json, composite→object, setof/table→array),
  `select`/`order`/`limit`/`offset` and filters on setof results, and bulk
  calls with `Prefer: params=multiple-objects`
- **Auth (PostgREST model)**: JWT **HS256** (shared secret) and **RS256**
  (keys fetched from `PGRST_JWKS_URL`, `kid` matching, `oct` keys supported)
  → `role` claim → `SET LOCAL ROLE` + claims as GUCs (`request.jwt.claims`,
  `request.jwt.claim.<key>`, `request.method`, `request.path`) → **Row Level
  Security does the authz**. Anonymous requests run as `db-anon-role`;
  tokens are ignored when no key is configured.
- **Multi-schema profiles**: `Accept-Profile` (reads) and `Content-Profile`
  (writes) narrow the request to one exposed schema
- **OpenAPI 2.0** at `/`, generated from the schema cache with per-column
  filter-operator documentation, PUT operations and procedure paths
- **Media negotiation**: JSON (default), `text/csv` output (union of keys
  across rows), `application/vnd.pgrst.object+json` (singular; 406
  `PGRST116` on zero or multiple rows)
- **Errors**: PGRST-style JSON errors with codes (`PGRST100` parse,
  `PGRST118` unknown relation, `PGRST201`/`PGRST203` ambiguity, `PGRST202`
  unknown function, `PG23505` unique violation → 409, `PG42501` → 403, ...)
  and `Allow` headers on 405s
- **Schema cache**: one introspection pass over `pg_catalog` (tables, views,
  constraints, functions, procedures, enums, computed relationships),
  hot-reloaded via `NOTIFY pgrst, 'reload schema'`
- **CORS** on every response + OPTIONS preflight
- `db-max-rows` server-side cap
- **Admin endpoints**: `/admin/live`, `/admin/ready`, `/admin/metrics`
  (Prometheus text format: request counters by status, latency histogram,
  pool gauges, resident RSS)
- **Server-Timing** header on every response
- **PostgREST-Plan** header via `Prefer: planning` (EXPLAIN)
- **Transaction control**: `Prefer: tx=commit|rollback`
- **Config table**: `PGRST_DB_CONFIG=true` reads `postgrest.config`
  (name/setting) over env vars; reload with `NOTIFY pgrst, 'reload config'`
- **Pre-request hook**: `PGRST_DB_PRE_REQUEST=schema.fn` runs inside the
  request transaction before the statement (custom auth, auditing, ...)
- **Config**: file-less, 12-factor env vars with PostgREST-compatible names

### Deliberately out of scope

Rate limiting, caching/ETags, migrations, admin UI, GraphQL, file storage.
This is a translator, not an application server.

### Known gaps

- Responses are buffered per request (bounded by `db-max-rows`); row-streaming
  off the wire protocol is the next performance step
- `count=planned` uses the plan's row estimate for the read query, which can
  drift from a freshly analyzed table
- OpenAPI output does not model per-function return shapes or embedded
  relationship definitions

## Configuration reference

| Env var | Default | Meaning |
|---|---|---|
| `PGRST_DB_URI` | `postgresql://postgres@127.0.0.1:5432/postgres` | Connection string |
| `PGRST_DB_SCHEMAS` | `public` | Exposed schemas |
| `PGRST_DB_ANON_ROLE` | `anon` | Role for unauthenticated requests |
| `PGRST_DB_POOL` | `4` | Connection pool size |
| `PGRST_DB_MAX_ROWS` | unset | Hard cap on rows per response |
| `PGRST_DB_PRE_REQUEST` | unset | Function called before every request |
| `PGRST_DB_PRE_CONFIG` | unset | Function called on config reload |
| `PGRST_DB_CONFIG` | `false` | Read `postgrest.config` table |
| `PGRST_JWT_SECRET` | unset | HS256 secret; unset = tokens ignored unless `PGRST_JWKS_URL` is set |
| `PGRST_JWKS_URL` | unset | JWKS endpoint for RS256 (and `oct`) keys |
| `PGRST_SERVER_HOST` | `0.0.0.0` | Bind host |
| `PGRST_SERVER_PORT` | `3000` | Bind port |
| `PGRST_DB_TX_END` | `commit` | Default tx ending |

## Security model

The server holds **no authorization logic**. Requests run inside a
transaction that does `SET LOCAL ROLE <role>` and publishes JWT claims as
GUCs; PostgreSQL grants + RLS policies decide everything. Recommended setup:

```sql
create role anon;            -- no login
create role authenticated;   -- no login
create role authenticator login password '...';
grant anon, authenticated to authenticator;
```

Point `PGRST_DB_URI` at the `authenticator` role; JWTs carry `role: "authenticated"`.

## Development

```bash
cargo test                      # parser/compiler unit tests
scripts/reset-db.sh             # (re)create + seed the integration database (needs psql)
scripts/integration.sh          # 113-check integration battery (needs running server)
```

Layout:

```
src/
  main.rs       startup, pool, router
  config.rs     env + config table + hot reload
  schema.rs     pg_catalog introspection -> schema cache
  parser.rs     URL grammar -> AST (select tree, filters, order)
  ast.rs        AST types
  compiler.rs   AST -> SQL (embeds via LATERAL, count, aggregates)
  mutate.rs     insert/update/delete/put/upsert/rpc compilers
  pg.rs         execution: pool, tx, role+GUC impersonation, LISTEN reload
  handlers.rs   HTTP dispatch, headers, status codes
  media.rs      JSON/CSV/object shaping
  auth.rs       HS256 + RS256 (JWKS) JWT
  openapi.rs    OpenAPI 2.0 generation
  metrics.rs    Prometheus metrics
  error.rs      PGRST error model
```

## Performance notes

- Idle RSS ~3.5-4 MB; flat under load (no per-request growth)
- Single-digit-millisecond p99 for simple reads against local Postgres
- Every read compiles to a single SQL round trip (embeds via `LATERAL`,
  counts via CTE)
- 2 tokio worker threads by default; scale pool + workers with cores

## License

MIT
