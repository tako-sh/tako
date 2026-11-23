---
title: "tako.toml Reference"
description: "Look up Tako project configuration for identity, runtimes, builds, routes, variables, storage, databases, KV stores, backups, images, workflows, and deployment targets."
---

`tako.toml` describes one app: identity, runtime, build, routes, variables, storage, database, and KV store bindings, backups, SSL provider, source-IP policy, workflows, networks, and target servers. App secrets, storage credentials, and provider credentials are encrypted in ignored `.tako/state.local.json` and encrypted network state, not stored in this file.

App-scoped commands use `./tako.toml` by default. `-c path/to/config` selects another config file and treats that file's parent directory as the app directory.

## Editor Support

A JSON Schema for `tako.toml` is published at `https://tako.sh/schemas/tako-toml.json`. `tako init` writes this directive as the first line of new files, and edits that Tako makes to `tako.toml` keep it:

```toml
#:schema https://tako.sh/schemas/tako-toml.json
```

Editors based on Taplo, such as the Even Better TOML extension for VS Code, read the directive to validate fields, complete keys, and show field descriptions on hover. Add the line to an existing `tako.toml` to opt in.

The schema catches unknown fields, wrong value types, and common shape mistakes such as an environment with both `route` and `routes`. It can be looser than the CLI's validation: rules that depend on other files or on combinations of fields, such as storage references, route patterns, or container release restrictions, are checked only by the CLI. The schema is designed never to reject a config the CLI accepts.

## Minimal Config

```toml
name = "my-app"
runtime = "bun"
preset = "tanstack-start"

[envs.production]
route = "my-app.example.com"
servers = ["prod-a"]
```

Unknown top-level fields are rejected. Paths such as `app_root`, `container`, and asset directories must be relative and cannot contain parent traversal (`..`).

`name` is optional but recommended. If omitted, Tako derives the app name from the selected config file's parent directory. Remote identity is `{name}/{env}`, so renaming the app or directory fallback creates a separate deployed app.

App names are DNS-compatible. They use lowercase letters, numbers, and hyphens, start with a lowercase letter, end with a lowercase letter or number, and contain at most 63 characters.

## Top-Level Fields

| Field             | Type     | Purpose                                                                                                                                                                                                                                                                                     |
| ----------------- | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `name`            | string   | Stable app identity. Must be DNS-friendly lowercase letters, numbers, and hyphens.                                                                                                                                                                                                          |
| `runtime`         | string   | Runtime adapter, optionally pinned with `@version`, such as `bun@1.2.3`, `node`, or `go`.                                                                                                                                                                                                   |
| `package_manager` | string   | JS package manager override: `bun`, `npm`, `pnpm`, or `yarn`.                                                                                                                                                                                                                               |
| `preset`          | string   | Runtime-local preset alias such as `vite`, `tanstack-start`, or `nextjs`.                                                                                                                                                                                                                   |
| `main`            | string   | Runtime entrypoint override. May be a file path or module specifier.                                                                                                                                                                                                                        |
| `app_root`        | string   | JS channels, presence, stores, and workflows root, relative to `tako.toml`. Defaults to `src`; use `.` for root-level files.                                                                                                                                                                |
| `schemas`         | string[] | External `tako-schema.json` files or HTTP(S) URLs. File paths are relative to `tako.toml` and may refer to sibling apps with `..`. Defaults to `[]`. `tako gen` compiles them into native types. Remote sources are pinned in `tako-schema.lock`; refresh with `tako gen --update-schemas`. |
| `dev`             | array    | Custom `tako dev` command. Overrides preset and runtime defaults unless a CLI command is passed.                                                                                                                                                                                            |
| `start`           | array    | Native deploy start command for prebuilt artifacts.                                                                                                                                                                                                                                         |
| `assets`          | array    | Additional asset directories merged into deployed `public/`.                                                                                                                                                                                                                                |
| `container`       | string   | Container file path for container releases.                                                                                                                                                                                                                                                 |
| `release`         | string   | One-shot command run on the leader server before rolling update.                                                                                                                                                                                                                            |

`image = "registry/name:tag"` selects a prebuilt container image. `container`, `image`, and `[compose]` are mutually exclusive.

`[container_options]` supports `port` (default 3000), `sdk` (default true), `health_path` (default `/status`), optional `health_host` for plain images, `data_dir` (default `/tako/data`), optional `user = { uid = 1000, gid = 1000 }`, `secret_env = ["DATABASE_URL"]`, `command = ["arg"]`, and `entrypoint = "/program"`. Paths must be absolute, non-root paths without parent traversal. Options require `container` or `image`. See [Container releases](/docker/).

`[compose]` accepts only `file`, inside the app directory. A Compose project is an application group; services with `x-tako` are apps, and services without it remain ordinary Docker services. Put per-app routes in `services.<name>.x-tako.envs.<environment>` in Compose, using `route` or `routes` and optional `source_ip`. Group environments in `tako.toml` keep deployment settings and cannot define routes. Tako infers app ports from Compose exposure or image `EXPOSE` and uses Docker health checks. Each environment targets one server. See [Docker Compose](/docker-compose/) for networking, volumes, and update behavior.

When `container`, `image`, or `[compose]` is set, native release fields are invalid: `main`, `start`, `assets`, `[build]`, and `[[build_stages]]`. For `container` and `image`, `tako dev` uses a CLI command override, the dev command, or runtime/preset dev defaults. For `[compose]`, `tako dev` builds and starts the Compose stack; configure commands and source mounts in Compose instead of a native dev command.

`start` is for native artifacts such as compiled binaries. An exact `{main}` argument is replaced with the resolved entrypoint. The process must still use a Tako SDK listener so fd 3 bootstrap, fd 4 readiness, secrets, storage bindings, and health checks work.

## Routes And Environments

```toml
[envs.production]
route = "app.example.com"
servers = ["la", "nyc"]
idle_timeout = 300
ssl = "letsencrypt"
source_ip = "auto"
storages = { uploads = "prod_uploads" }
backup = { storage = "private_backups" }

[envs.staging]
routes = ["staging.example.com", "example.com/api/*"]
servers = ["staging"]
release = ""
```

Each environment can use either `route` or `routes`, not both. Non-development deploy environments must define at least one route. `development` is reserved for `tako dev`; deploy validation ignores `servers` there.

Routes support exact hosts, wildcard hosts, host-plus-path routes, and wildcard-plus-path routes. Path-only routes are invalid.

Environment tables accept route declarations, `network`, `servers`, `storages`, `db`, `kv`, `channels`, `workflows`, `backup`, `source_ip`, `ssl`, `idle_timeout`, and `release`. Put environment variables in `[vars]` or `[vars.<env>]`, not in `[envs.<env>]`.

`[envs.<env>].network` is the stable 32-character lowercase hexadecimal network ID from `tako network list --json`. Local aliases are not accepted in the config. Set it explicitly or let the first deploy record its chosen network, then commit the config. `development` runs locally and cannot set `network`. See [Choose an environment's network](/networks/#choose-an-environments-network).

Within the environment's network, `servers` selects deployment hosts by enrolled name or server device ID. Omit `servers` to target every trusted server in the network. A supplied list must be nonempty, and every entry must identify exactly one trusted server. Native management uses pinned HTTPS. See [Native networks](/networks/).

```toml
[envs.production]
servers = ["edge-a", "edge-b"]
route = "app.example.com"
```

`idle_timeout` must be greater than zero and defaults to 300 seconds. `ssl` defaults to `letsencrypt` and can be `cloudflare`. `source_ip` can be omitted or set to `auto`, `direct`, `cloudflare-proxy`, or `trusted-proxy`.

Environment-level `release` overrides the top-level release command. An empty string clears an inherited top-level command for that environment.

## Variables

```toml
[vars]
API_URL = "https://api.example.com"
FEATURE_FLAG = true

[vars.staging]
API_URL = "https://staging-api.example.com"
```

Variables merge in this order: `[vars]`, then `[vars.<env>]`, then Tako runtime values. TOML strings, numbers, booleans, and datetimes are accepted and converted to process-env strings. Arrays and tables are invalid variable values.

`ENV` is reserved and ignored with a warning if set by the user. Set framework log variables such as `LOG_LEVEL` yourself when needed.

## Build

Simple build:

```toml
[build]
install = "bun install"
run = "bun run build"
cwd = "packages/web"
include = ["dist/**", "package.json"]
exclude = ["**/*.map"]
```

Multi-stage build:

```toml
[[build_stages]]
name = "shared-ui"
cwd = "packages/ui"
install = "bun install"
run = "bun run build"
exclude = ["**/*.map"]

[[build_stages]]
name = "web"
cwd = "apps/web"
run = "bun run build"
```

Use `[build]` for a simple build or `[[build_stages]]` for multiple stages. Tako rejects combining stages with a non-empty `[build].run` or any `[build].include`/`exclude` entries. Build stage precedence is `[[build_stages]]`, then `[build]`, then the runtime default. JS defaults run the runtime lane's build script when present. Go defaults build `app`, and also `worker` when `cmd/worker/main.go` exists.

`[[build_stages]].run` is required and cannot be empty. Each stage accepts only `name`, `cwd`, `install`, `run`, and `exclude`; artifact `include` is available only on `[build]`.

Simple `[build].cwd` must stay inside the project. Multi-stage `cwd` can use `..` for monorepos but cannot escape the source workspace.

## Images

```toml
[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
local_patterns = ["/images/**"]
sizes = [320, 640, 960, 1200, 1920]
qualities = [75]
formats = ["webp", "avif"]
```

The default sizes are `[320, 640, 960, 1200, 1920]`, qualities `[75]`, and formats `["webp"]`. Each array replaces its default and must be non-empty; sizes must be positive and qualities must be between 1 and 100. An explicit URL quality must appear in `qualities`; a request without one uses 75. The example above enables AVIF as well as WebP.

The JavaScript `imageUrl` and `imageSrcSet` helpers currently accept only the five default widths, even if you configure additional server sizes. Keep helper-generated widths in the server allowlist. Non-default quality values also need to be allowed by the server.

Local image paths default to `["/**"]`; setting `local_patterns` replaces that default. Remote images are denied unless they match `remote_patterns`. Patterns are glob-like URL strings: `*` matches one segment and `**` matches the rest of a path. Remote patterns without a protocol match both `http` and `https` on their standard ports. An explicit port must match exactly; a pattern with a protocol and no port permits only that protocol's default port. Query strings do not participate in path matching. Sources with dot segments, encoded path separators, backslashes, or control characters are rejected.

## Storage

```toml
[envs.production]
storages = { uploads = "prod_uploads" }

[storages.prod_uploads]
provider = "s3"
bucket = "app-uploads"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
force_path_style = false
public_base_url = "https://cdn.example.com/uploads"
```

Top-level storage resources store non-secret metadata. S3 resources require `bucket`, `endpoint`, and `region`; endpoints and public base URLs must use HTTPS. Set `force_path_style = true` when your S3-compatible provider needs path-style bucket URLs instead of virtual-hosted bucket URLs. Credentials are set by `tako storage add` or `tako storage credentials` and stored encrypted in ignored `.tako/state.local.json` and encrypted network state.

`local` is a built-in resource name. Bind to it with `storages = { uploads = "local" }`; do not declare `[storages.local]`. Local storage deploys only to single-server environments. In `development`, undeclared storage resources default to local storage.

## Databases

```toml
[envs.production]
db = { main = "main" }

[envs.development]
db = { main = "main" }

# Optional: back this database up to another storage than backup.storage.
[db.main]
backup = "private_backups"
```

`[envs.<env>].db` maps app-facing binding names, read as `tako.db.<binding>`, to database names in the environment's network. On a one-server network a full client's deploy creates missing databases there. On a larger network, first run `tako db create <name> --server <server>` with full access; the database can live outside this app's target servers. Development uses local files in `.tako/data/databases/<database-name>.sqlite`. Without `[envs.development].db`, development binds the app SQL aliases from the other environments.

`[db.<name>]` is optional and has one field:

| Field    | Purpose                                                                                                                                                                                                                                                                                           |
| -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `backup` | Private S3 storage resource that receives the database's encrypted continuous backups, instead of the deploying environment's `backup.storage`. It must use provider `s3` and no `public_base_url`. Without either, the database is not backed up; removing them and deploying stops its backups. |

`id`, `server`, and `access` are rejected: run `tako db list` to see where databases live. Binding and database names start with a lowercase letter and contain lowercase letters, digits, `-`, and `_`, at most 63 characters. SQL migrations live in `migrations/` (see [Migrations](/databases/#migrations)). Container and Compose releases cannot expose app SQL bindings. See [Databases](/databases/).

## Channel replay

```toml
[envs.production.channels]
db = "chat-prod-replay"
```

The table is optional. When present, `db` is the name of a hosted SQLite database used by the channel runtime, not an app SQL alias. Without it, channel-capable deploys use a generated app/environment database. The same single-server creation and multi-server placement rules apply as for app databases. Multiple environments can explicitly name one database without sharing rooms. `tako dev` keeps replay in the project's `.tako/data/tako/channels.sqlite`. See [Deploy Replay Storage](/channels/#deploy-replay-storage) before changing a running environment's database.

## Workflow Storage

```toml
[envs.production.workflows]
db = "shop-workflows"
```

The table is optional. When present, `db` is the name of the hosted SQLite database that holds the environment's workflow runs; it is not an app SQL alias and app code never receives its token. Without it, a deploy of a release that runs workflows uses a generated app/environment database, created by a full client on a one-server network. On a larger network, create the database with `tako db create <name> --server <server>` and name it here. Channels and workflows may name the same database. `tako dev` keeps runs in the project's `.tako/data/tako/workflows.sqlite`. See [Workflows](/workflows/#storage-and-multiple-servers).

## KV Stores

```toml
[envs.production]
db = { main = "main" }
kv = { auth = "sessions", limits = "rate-limits" }

[envs.staging]
kv = { auth = "sessions_staging" }

# Optional: snapshot this store to another storage than backup.storage.
[kv.sessions]
backup = "private_backups"
```

`[envs.<env>].kv` maps app-facing binding names, read as `tako.kv.<binding>` (Go `tako.GetKV`, Rust `tako::kv`), to the names of KV stores created with `tako kv create`. Databases and KV stores share the network's records, so a deploy fails when the bound name is missing from the network or belongs to a database. An environment's `db` and `kv` share one set of binding names: binding a name in both is a validation error. Binding and store names follow the database naming rules.

Development uses `[envs.development].kv` when it is set; otherwise it binds every `kv` name used by any environment, taking the store from the first environment in name order. `tako dev` and `tako run` serve those stores from `.tako/data/kv/<store-name>.redb`.

`[kv.<name>]` is optional and has one field, `backup`: the private S3 storage resource that receives the store's encrypted snapshots, instead of the deploying environment's `backup.storage`, with the same rules as `[db.<name>].backup`. A name cannot have both a `[db.<name>]` and a `[kv.<name>]` table. `tako kv delete` removes a deleted store's bindings and `[kv.<name>]` table and preserves the rest of the file. Container and Compose releases cannot bind KV stores yet. See [KV stores](/kv/).

## Backups

```toml
[envs.production]
backup = { storage = "private_backups" }

[storages.private_backups]
provider = "s3"
bucket = "app-data"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
```

`backup` names the default backup storage of the hosted databases and KV stores the environment binds, unless `[db.<name>].backup` or `[kv.<name>].backup` names another. Files in the app's data directories are not backed up. The former `databases` and `shared_databases` lists are rejected with instructions to move each file to a hosted database. See [Backups](/backups/) for restore commands and retention.

Backup storage must be private S3-compatible storage. `public_base_url` and local storage are rejected for backups. Backup resources are not exposed to `tako.storages` unless they are also listed in `[envs.<env>].storages`.

## SSL Credentials

Use `ssl = "letsencrypt"` for the default. Exact routes can use HTTP-01 without provider credentials. Wildcard Let's Encrypt routes require `ssl.cloudflare` because they use Cloudflare DNS-01. If `ssl.cloudflare` is present, exact Let's Encrypt routes also use DNS-01.

Use `ssl = "cloudflare"` for Cloudflare Origin CA certificates. This also requires `ssl.cloudflare`.

Set provider credentials with:

```bash
tako credential set ssl.cloudflare --env production
```

Provider credentials are encrypted in ignored `.tako/state.local.json` and encrypted network state, not stored in `tako.toml`.

## Workflows

```toml
[workflows]
retention = "184d"

[workflows.email]
run = ["./worker", "email"]
```

`run` provides an explicit worker command for runtimes that need one. In v0, container releases support one configured workflow command.

`retention` controls when finished workflow runs and their saved steps become eligible for cleanup. It defaults to `"184d"`, covering six calendar months; set `"forever"` to keep them. Use a positive whole number followed by `s`, `m`, `h`, or `d`, such as `"1h"` or `"30d"`. Cleanup is best effort and runs in bounded batches while the workflow runtime is active. Pending and running workflows are never pruned. Set retention only in the app's `[workflows]` section, not in a per-server worker group.

Production supervises one scale-to-zero lane per JavaScript worker group, selected by `worker: "name"` in workflow definitions, with an additional default lane. Each lane currently receives concurrency 500 and a 300-second idle timeout. Go and container workers use a single lane.

The parser accepts `run` in named worker groups. It accepts `workers` and `concurrency` in `[workflows]`, `[workflows.<group>]`, `[servers.<server>.workflows]`, and `[servers.<server>.workflows.<group>]`. Parsed count/concurrency precedence is defaults (`workers = 0`, `concurrency = 10`), app base, app group, server base, then server group. These parsed counts and overrides do not yet control production supervision; do not rely on them to tune running workers. `run` must be a non-empty array of non-empty strings. Group and server names follow the app-name rules.

Workflow runs live in a hosted database that every server of the environment shares. On a one-server network a full client's deploy creates it; on a larger network, create it with `tako db create <name> --server <server>` and name it in `[envs.<env>.workflows].db`. See [Workflows](/workflows/#storage-and-multiple-servers).

## Complete Example

```toml
name = "my-app"
runtime = "bun@1.2.3"
package_manager = "bun"
preset = "tanstack-start"
app_root = "src"
release = "bun run db:migrate"

[build]
run = "bun run build"

[vars]
API_URL = "https://api.example.com"

[vars.staging]
API_URL = "https://staging-api.example.com"

[envs.production]
route = "app.example.com"
servers = ["la", "nyc"]
storages = { uploads = "prod_uploads" }
db = { main = "main" }
kv = { auth = "sessions" }
backup = { storage = "private_backups" }
ssl = "letsencrypt"
source_ip = "auto"

[storages.prod_uploads]
provider = "s3"
bucket = "app-uploads"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
public_base_url = "https://cdn.example.com/uploads"

[storages.private_backups]
provider = "s3"
bucket = "app-backups"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
```
