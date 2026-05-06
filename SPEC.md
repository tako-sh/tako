# Tako Specification

This is the finalized specification for Tako. It describes the system as designed and implemented. Keep this in sync with code changes - when you modify code, update the corresponding sections here.

## Project Overview

Tako is a deployment and development platform consisting of:

- **`tako` CLI** - Local tool for development, deployment, server/secret management
- **`tako-server`** - Remote server binary that manages app processes, routing, and rolling updates
- **`tako.sh` SDK** - SDK implementations for JavaScript/TypeScript and Go apps

Built in Rust (2024 edition). SDKs available for JavaScript/TypeScript (`tako.sh` npm package) and Go (`tako.sh` Go module). Uses Pingora (Cloudflare's proxy) for production-grade performance.

## Design Goals

**Performance:** On par with Nginx, faster than Caddy. Built in Rust leveraging Pingora.

**Simplicity:** Opinionated defaults, minimal configuration, convention over configuration.

**Reliability:** Strong test coverage, graceful edge case handling, users can delete files/folders safely with recovery paths.

**Extensibility:** Support multiple runtimes (Bun, Node, Go). Runtime-agnostic architecture.

## Configuration

### App Name Requirements

App names must be URL-friendly (DNS hostname compatible):

- **Allowed:** lowercase letters (a-z), numbers (0-9), hyphens (-)
- **Must start with:** lowercase letter
- **Examples:** `my-app`, `api-server`, `web-frontend`

This ensures names work in DNS (`{app-name}.test` by default), URLs, and environment variables.
`name` is optional in `tako.toml`. If omitted, Tako resolves app name from the selected config file's parent directory name.
Using top-level `name` is recommended for stability. Remote server identity is `{name}/{env}`, so the same app name can be deployed to multiple environments on one server. Renaming `name` later creates a new app identity/path; delete the old deployment manually.

### tako.toml (Default Project Config)

Default application configuration file for build, variables, routes, and deployment.
App-scoped commands use `./tako.toml` by default. Passing `-c/--config <file>` selects a
different config file and treats that file's parent directory as the project directory.
The config file's parent directory is the app directory (there is no separate `app_dir` field).

```toml
name = "my-app"              # Optional but recommended stable identity used by deploy/dev
main = "server/index.mjs"   # Optional override; required only when preset does not define `main`
runtime = "bun"              # Optional override; defaults to detected adapter
runtime_version = "1.2.3"   # Optional pinned version; auto-detected if omitted
package_manager = "bun"      # Optional override; auto-detected from package.json or lockfiles
preset = "tanstack-start"   # Optional app preset; provides `main`, `assets`, and `dev` defaults
dev = ["vite", "dev"]        # Optional custom dev command override
assets = ["dist/client"]     # Optional asset directories for deploy artifact
release = "bun run db:migrate"   # Optional release command (run once on the leader server before rolling update)

[build]
run = "vinxi build"       # Build command
install = "bun install"   # Optional pre-build install command
# cwd = "packages/web"   # Optional working directory relative to project root

# OR use multi-stage builds (mutually exclusive with [build]):
# [[build_stages]]
# name = "shared-ui"
# cwd = "packages/ui"
# install = "bun install"
# run = "bun run build"
# exclude = ["**/*.map"]

[vars]
API_URL = "https://api.example.com" # Base variables (all environments)

[vars.production]
API_URL = "https://api.example.com"

[vars.staging]
API_URL = "https://staging-api.example.com"

[envs.production]
route = "api.example.com"  # Single route, or use 'routes' for multiple
servers = ["la", "nyc"]
idle_timeout = 300         # Optional, default: 5 minutes

[envs.staging]
routes = [
  "staging.example.com",
  "www.staging.example.com",
  "example.com/api/*"
]
servers = ["staging"]
idle_timeout = 120
```

**Variable merging order (later overrides earlier):**

1. `[vars]` - base
2. `[vars.{environment}]` - environment-specific
3. Auto-set by Tako at runtime: `ENV={environment}` in both dev and deploy, `TAKO_BUILD={version}` on deploys, `TAKO_DATA_DIR=<app data dir>` in both deploy and dev, plus runtime env vars (e.g. `NODE_ENV` for all JS runtimes, `BUN_ENV` for Bun)

`ENV` is reserved. If you set `ENV` in `[vars]` or `[vars.{environment}]`, Tako ignores it and prints a warning. `LOG_LEVEL` (and any other log-verbosity env var your framework reads) is owned by you — set it in `[vars]` / `[vars.<env>]` if you want it per environment.

**Build/deploy behavior:**

- `name` in `tako.toml` is optional.
- App name resolution order for deploy/dev/logs/secrets/delete:
  1. top-level `name` (when set)
  2. sanitized selected-config parent directory name fallback
- Remote deployment identity on servers is `{app}/{env}`. Set `name` explicitly to keep the `{app}` segment stable across deploys.
- Renaming app identity (`name` or directory fallback) is treated as a different app; remove the previous deployment manually if needed.
- `main` in `tako.toml` is an optional runtime entrypoint override written to deployed `app.json`. It accepts file paths and module specifiers (e.g. `@scope/pkg`).
- If `main` is omitted in `tako.toml`, deploy/dev check the manifest main field (e.g. `package.json` `main`), then fall back to preset `main`.
- For JS adapters (`bun`, `node`), when preset `main` is `index.<ext>` or `src/index.<ext>` (`ext`: `ts`, `tsx`, `js`, `jsx`), deploy/dev resolve in this order: existing `index.<ext>`, then existing `src/index.<ext>`, then preset `main`.
- If neither `tako.toml main`, manifest main, nor preset `main` is set, deploy/dev fail with guidance.
- Top-level `runtime` is optional; when set to `bun`, `node`, or `go`, it overrides adapter detection for default preset selection in `tako deploy`/`tako dev`.
- Top-level `runtime_version` is optional; when set (e.g. `"1.2.3"`), deploy uses it directly instead of auto-detecting with `<runtime> --version`. `tako init` pins the locally-installed version by default.
- Top-level `package_manager` is optional; when set (e.g. `"npm"`, `"pnpm"`, `"yarn"`, `"bun"`), it overrides auto-detection from `package.json` `packageManager` field or lockfiles.
- Top-level `preset` is optional. Presets are metadata-only (`name`, `main`, `assets`, `dev`) providing entrypoint, asset, and dev-command defaults. They do not contain build, install, or start commands.
- Top-level `dev` is optional; when set (e.g. `["vite", "dev"]`), it overrides both preset and runtime default dev commands for `tako dev`.
- Top-level `assets` is optional; lists asset directories to include in the deploy artifact (e.g. `["dist/client"]`). Asset roots are preset `assets` plus top-level `assets` (deduplicated).
- Top-level `release` is optional. When set, deploy runs the command once
  on the **leader server** (first entry in `[envs.<env>].servers`) inside
  the new release directory after extract + production install but
  before any rolling update.
- `[envs.<env>].release` overrides the top-level value for that
  environment. An empty string (`release = ""`) explicitly clears the
  inherited top-level command for that env.
- The release command runs as `sh -c "<command>"` with cwd set to the
  new release directory and the same env an HTTP instance receives
  (merged `[vars]` + `[vars.<env>]` + secrets + `TAKO_BUILD` +
  `TAKO_DATA_DIR` + `ENV` + runtime defaults). Secrets are injected as
  env vars (release commands are one-shot; the fd 3 mechanism is
  reserved for long-running app/worker processes).
- Hard timeout: 10 minutes per release-command invocation. On timeout,
  the process is killed and deploy fails.
- `preset` supports:
  - runtime-local aliases: `tanstack-start`, `nextjs` (resolved under selected runtime, e.g. `runtime = "bun"`)
  - pinned runtime-local aliases: `tanstack-start@<commit-hash>`, `nextjs@<commit-hash>`
- namespaced preset aliases in `tako.toml` (for example `js/tanstack-start`) are rejected; choose runtime via top-level `runtime` and keep `preset` runtime-local.
- `github:` preset references are not supported in `tako.toml`.
- Preset definitions live in `presets/<language>.toml` (for example `presets/javascript.toml`), where each preset is a section (`[tanstack-start]`, etc.). Each section contains `name` (optional, fallback: section name), `main`, `assets`, and `dev` (custom dev command). Tako caches fetched preset manifests locally. `tako dev` prefers cached or embedded preset data and only fetches from GitHub when nothing local is available; deploy refreshes unpinned aliases from GitHub and falls back to cached content on fetch failure.
- `tanstack-start` preset defaults `main = "dist/server/tako-entry.mjs"`, `assets = ["dist/client"]`, and `dev = ["vite", "dev"]`. The `main` file is emitted by `tako.sh/vite` during `vite build` and wraps the SSR bundle with tako endpoint handling.
- `nextjs` preset defaults `main = ".next/tako-entry.mjs"` and `dev = ["next", "dev"]`.
- `vite` preset defaults `dev = ["vite", "dev"]` for projects using Vite as their dev server.
- Presets may declare runtime-local overrides as nested sections (`[<preset>.<runtime>]`) inside the family manifest. Only the `dev` field can be overridden — `main`, `assets`, and `name` always come from the base section. For example, `presets/javascript.toml` overrides the `vite` and `tanstack-start` dev commands for Bun (`[vite.bun]`, `[tanstack-start.bun]`) because `bunx --bun` drops fds > 2, which breaks the fd-4 readiness handshake.
- Official preset definitions live in the `tako-sh/presets` GitHub repo (overridable via the `PACKAGE_REPOSITORY_URL` env var for testing). Fetched branch manifests are cached locally for roughly one hour; on fetch failure, Tako falls back to any previously cached copy or the manifests embedded in the CLI binary. GitHub preset fetches use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.
- Deploy restores local JS build caches from workspace-root `.turbo/` and app-root `.next/cache/` into the temporary build workspace when present, then excludes those cache directories from the final deploy artifact.
- Runtime behavior (install commands, launch args, entrypoint resolution) lives in runtime plugins (`tako-runtime/src/plugins/`), not in presets.
- `tako init` installs the `tako.sh` SDK via the selected runtime's package-manager `add` command.
- Server membership is declared per environment with `[envs.<name>].servers`.
- The same server name may be assigned to multiple non-development environments in one project. Each environment deploys to its own server-side app identity and filesystem path under `/opt/tako/apps/{app}/{env}`.
- `development` is for `tako dev`; `servers` declared there are ignored by deploy validation.
- Deployed app instances bind to `127.0.0.1` on an OS-assigned port. The SDK signals readiness to `tako-server` by writing the bound port to fd 4 (file descriptor 4) once listening. The server then routes traffic to that loopback endpoint.
- `tako dev` resolves the dev command with this priority:
  1. `dev` in `tako.toml` (user override, e.g. `dev = ["custom", "cmd"]`)
  2. Preset `dev` command (e.g. vite preset uses `vite dev`)
  3. Runtime default: JS runtimes run through the SDK dev entrypoint (`bun run node_modules/tako.sh/dist/entrypoints/bun-dev.mjs {main}`, or the `node-dev.mjs` equivalent), Go uses `go run .`
- `tako dev` marks an app running only after the app writes its bound loopback port to fd 4. Direct Vite dev commands (for example `vite` or `vite dev`) must use the `tako.sh/vite` plugin for fd-4 readiness; if the command looks like Vite and no readiness signal arrives, the CLI reports a Vite-specific plugin hint. Tako does not parse Vite stdout URLs as readiness.
- The dev entrypoints host the HTTP server. Workflow workers run as a **separate, scale-to-zero subprocess** managed by tako-dev-server's embedded `WorkflowManager` — same architecture as production, but `workers: 0` with a 3s idle timeout so the worker only exists while there's real work. The SDK wraps `export default function fetch()` or `export default { fetch }` into a proper HTTP server on `PORT`; worker stdout/stderr is tee'd into the CLI log stream with `scope: "worker"`.
- Process exit detection: `tako dev` polls `try_wait()` every 500ms to detect when the app process exits. On exit, the route goes idle (proxy stops forwarding) and the next HTTP request triggers a restart. A route is activated only after fd-4 readiness succeeds.
- `tako dev` resolves unpinned official preset aliases from cached or embedded preset data when available and only fetches from the `master` branch as a last resort.
- `tako deploy` resolves unpinned official preset aliases from the `master` branch on each deploy; if the refresh fails, it falls back to cached content.
- Deploy sends app vars + runtime vars to `tako-server` in the `deploy` command payload (non-secret env vars in `app.json`); secrets are sent separately and stored encrypted in SQLite. `tako-server` passes secrets to HTTP instances and workflow workers via fd 3 (file descriptor 3) at spawn time — the server writes secrets as JSON to a pipe and the child process reads fd 3 before any user code runs.
- `[build]` section has `run` (build command), `install` (optional pre-build install command), `cwd` (optional working directory relative to project root), plus `include`/`exclude` for artifact filtering. `[build]` is a shortcut for a single-stage `[[build_stages]]` list.
- `[build]` and `[[build_stages]]` are mutually exclusive: having both `build.run` and `[[build_stages]]` is an error. `[build].include`/`[build].exclude` cannot be used alongside `[[build_stages]]`; use per-stage `exclude` instead.
- Build stage resolution precedence (first non-empty wins): `[[build_stages]]` → `[build]` (normalized to a single stage) → runtime default. The runtime default is the runtime plugin's build command: `bun/npm/pnpm/yarn run --if-present build` for JS runtimes and no default for Go. When nothing resolves, the build phase is a no-op.
- App-level custom build stages can be declared in `tako.toml` under `[[build_stages]]` (top-level array):
  - `name` (optional display label)
  - `cwd` (optional, relative to app root; `..` is allowed for monorepo traversal but guarded against escaping the workspace root)
  - `install` (optional command run before `run`)
  - `run` (required command)
  - `exclude` (optional array of file globs to exclude from the deploy artifact)
- Build uses a build dir approach: copies the project from source root into `.tako/build` (respecting `.gitignore`), symlinks `node_modules/` directories from the original tree, runs build commands, then archives the result without `node_modules/`.
- During `tako deploy`, source files are bundled from source root (`git` root when available, otherwise app directory).
- Deploy always force-excludes `.git/`, `.tako/`, `.env*`, and `node_modules/` from the deploy archive. Additional exclusions come from `[build].exclude` and `.gitignore`.
- After extracting the deploy artifact, `tako-server` runs the runtime plugin's production install command (e.g. `bun install --production`) before starting instances.
- When `runtime_version` is set in `tako.toml`, deploy uses it directly. Otherwise, runtime version resolution runs `<tool> --version` directly, falling back to `latest`.
- Deploy saves the resolved runtime version into `app.json` (`runtime_version` field).
- Built target artifacts are cached locally under `.tako/artifacts/` using a deterministic cache key that includes source hash, target label, resolved preset source/commit, build commands, include/exclude patterns, asset roots, and app subdirectory.
- Cached artifacts are checksum/size verified before reuse; invalid cache entries are automatically discarded and rebuilt.
- Non-dry-run `tako deploy` acquires a project-local `.tako/deploy.lock` before local server checks/build/deploy work begins. If another deploy already holds the lock, the second CLI exits immediately with the owning PID.
- After build, deploy verifies the resolved runtime `main` file exists in the build workspace before artifact packaging; missing files fail deploy with an explicit error.
- On every deploy, local artifact cache is pruned automatically (best-effort): keep the 90 most recent target artifacts (`{version}.tar.zst` under `.tako/artifacts/` and its per-target subdirectories) and remove orphan target metadata files.
- Artifact include patterns are resolved in this order:
  - `build.include` (if set)
  - fallback `**/*`
- Artifact exclude patterns: `[build].exclude` entries.
- Asset roots are preset `assets` plus top-level `assets` (deduplicated), merged into app `public/` after build in listed order (later entries overwrite earlier ones).

**Instance behavior:**

- Desired instances are runtime app state stored on each server, not `tako.toml` config.
- New app deploys start with desired instances `1` on each server. The first request after deploy hits a hot instance — no cold start. Opt into scale-to-zero with `tako scale 0 --env <environment>` from a project directory, or `tako scale 0 --server <server> --app <app>/<env>` outside one.
- `tako scale` changes the desired instance count per targeted server, and that value persists across server restarts, deploys, and rollbacks.
- Desired instances `0`: On-demand with scale-to-zero. Deploy keeps one warm instance running so the app is immediately reachable after deploy. Instances are stopped after idle timeout.
  - Once scaled to zero, the next request triggers a cold start and waits for readiness up to startup timeout (default 30 seconds). If no healthy instance is ready before timeout, proxy returns `504 Gateway Timeout` with a generic body.
  - If cold start setup fails before readiness, proxy returns `502 Bad Gateway` with a generic body.
  - Startup timeout diagnostics include captured startup stdout/stderr when the process produced output before readiness.
  - While a cold start is already in progress, requests are queued up to 1000 waiters per app (default). If the queue is full, proxy returns `503 Service Unavailable` with a generic body.
  - If warm-instance startup fails during deploy, deploy fails.
- Desired instances `N` (`N > 0`): keep at least `N` instances running on that server.
- `idle_timeout`: Applies per-instance (default 300s / 5 minutes)
- Instances are not stopped while serving in-flight requests.
- Explicit scale-down drains in-flight requests first, then stops excess instances.

### config.toml (Global User Config)

Global user-level settings and server inventory. Stored in the platform config directory (`~/Library/Application Support/tako/` on macOS, `~/.config/tako/` on Linux). NOT in the project.

```toml
[[servers]]
name = "la"
host = "1.2.3.4"
port = 22                 # Optional, defaults to 22
arch = "x86_64"
libc = "glibc"

[[servers]]
name = "nyc"
host = "5.6.7.8"
arch = "aarch64"
libc = "musl"
```

`[[servers]]` entries are managed by `tako servers add/rm/ls`. All names and hosts must be globally unique.
Detected server build target metadata is stored directly in each `[[servers]]` entry (`arch`, `libc`).

**SSH authentication:**

- `tako` authenticates using local SSH keys from `~/.ssh` (common filenames like `id_ed25519`, `id_rsa`, etc.).
- If a key file is passphrase-protected, `tako` will prompt for the passphrase when running interactively (or you can provide `TAKO_SSH_KEY_PASSPHRASE`).
- If no suitable key files are found or usable, `tako` falls back to `ssh-agent` via `SSH_AUTH_SOCK` (when available).

- `tako dev` uses a fixed local HTTPS listen port (`47831`).
- On macOS, `tako dev` uses a dedicated loopback alias (`127.77.0.1`) plus a launchd-managed dev proxy so public URLs stay on default ports (`:443` for HTTPS, `:80` for HTTP redirect).
- On Linux, `tako dev` uses the same loopback alias (`127.77.0.1`) with iptables redirect rules (443→47831, 80→47830, 53→53535) to achieve portless URLs without a proxy binary. One-time `sudo` sets up the rules, a systemd oneshot service persists them across reboots. On NixOS, a `configuration.nix` snippet is printed instead of imperative setup.

CLI prompt history is stored separately at `history.toml` (not in `config.toml`).

### .tako/secrets.json (Project - Encrypted)

Per-environment encrypted secrets (JSON format, AES-256-GCM encryption):

```json
{
  "production": {
    "key_id": "0123456789abcdef",
    "secrets": {
      "DATABASE_URL": "encrypted_value",
      "API_KEY": "encrypted_value"
    }
  },
  "staging": {
    "key_id": "fedcba9876543210",
    "secrets": {
      "DATABASE_URL": "encrypted_value_different"
    }
  }
}
```

Each environment has a `key_id` (16 hex characters) and a `secrets` map. Secret names are plaintext; values encrypted with AES-256-GCM.

`tako init` ensures the app's `.tako/` directory stays ignored while `.tako/secrets.json` remains trackable:

- inside a git repo, it updates the repo root `.gitignore` with app-relative rules
- outside a git repo, it creates or updates `.gitignore` in the app directory

Encryption keys are stored outside the project:

- By default, environment-specific keys are cached under Tako's data directory as `keys/{key_id}`, where `key_id` is the environment key id stored in `.tako/secrets.json`.
- On macOS, interactive key creation and key import offer `Use iCloud Keychain?`. Choosing yes stores the key as a synchronizable Keychain item named `Tako {key_id}` instead of writing a local key file. Tako reads keys from Keychain or from `keys/{key_id}`.

When the first secret is set for an environment, Tako generates a random environment key. Keys are shared with other machines via `tako secrets key export` and `tako secrets key import`. Teams that prefer a memorized shared secret can initialize an environment key with `tako secrets key import --passphrase --env {environment}` before setting secrets.

## Tako CLI Commands

### Installation and upgrades

Install the CLI on your local machine:

```bash
curl -fsSL https://tako.sh/install.sh | sh
```

The hosted installer installs `tako` and `tako-dev-server` from the same archive. On macOS, the archive also includes `tako-dev-proxy`.

Upgrade local CLI:

```bash
tako upgrade
```

`tako upgrade` upgrades only the local CLI installation.

Rolling release model:

- `latest` is a single moving GitHub release updated by CI on each `master` push. There are no versioned releases and no stable/canary distinction; every build is a rolling build while Tako's protocol is v0.
- CLI and server artifacts report `<base>-<sha7>` in `--version` output, where `<base>` is the package version (always `0.0.0`) and `<sha7>` is the 7-character source commit.
- The npm-published SDK (`tako.sh`) uses `0.0.0-<sha7>` version strings and is published under the `latest` dist-tag on every push.
- GitHub-backed update checks and release downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`. Tokens are sent only as `Authorization: Bearer ...` request headers, not in URLs.

### Global options

- `--version`: Print version and exit (format: `<base>-<sha7>`).
- `-v, --verbose`: Show verbose output as an append-only execution transcript with timestamps and log levels.
- `--ci`: Deterministic non-interactive output (no colors, no spinners, no prompts). Can be combined with `--verbose`.
- `--dry-run`: Show what would happen without performing any side effects. Skips SSH connections, file uploads, config writes, and remote commands. Prints `⏭ ... (dry run)` for each skipped action. Production deploy confirmation is auto-skipped. Supported by: `deploy`, `servers add`, `servers rm`, `delete`.
- `-c, --config {config}`: Use an explicit app config file instead of `./tako.toml`. If the provided path does not end with `.toml`, Tako appends it automatically. App-scoped commands treat the selected file's parent directory as the project directory. This allows multiple config files in one folder.

CLI output modes:

- **Normal mode** (default): Concise interactive UX with rich prompts and inline progress rendering. Commands that already know their plan may render a persistent task tree that shows waiting work up front (`○` with `...` labels), updates running tasks in place, keeps completed tasks visible, and may render reporter-specific error lines under failed task rows.
- **Verbose mode** (`--verbose`): Append-only execution transcript. Each line: `HH:MM:SS LEVEL message`. It only prints work as it starts or finishes; upcoming tasks are not pre-rendered. Prompts render as transcript-style (still interactive). DEBUG-level messages are shown.
- **CI mode** (`--ci`): No ANSI colors, no spinners, no interactive prompts. It stays transcript-style and emits only current work plus final results. If a required prompt value is missing, fails with an actionable error message suggesting CLI flags or config.
- **CI + Verbose** (`--ci --verbose`): Detailed append-only transcript with no colors or timestamps.
- On `Ctrl-C`, Tako clears any active prompt or spinner it controls, leaves one blank line, prints `Operation cancelled`, and exits with code 130.

All status/progress/log output goes to stderr. Only actual command results (URLs, machine-readable data) go to stdout.

Config selection is global for app-scoped commands:

- default: `./tako.toml`
- override: `-c path/to/config` (recommended shorthand; `.toml` suffix is optional)
- project directory: parent directory of the selected config file

App-scoped commands that honor `-c/--config`: `init`, `dev`, `logs`, `deploy`, `releases`,
`delete`, `secrets`, and `scale` when it is using project context.

### tako init

Create `tako.toml` template with helpful comments.

```bash
tako init
```

Template behavior:

- Leaves only minimal starter options uncommented:
  - `name`
  - `[envs.production].route`
  - top-level `runtime`
  - top-level `runtime_version` (pinned from locally-installed runtime version via `<runtime> --version`)
  - top-level `preset` only when a non-base preset is selected (for base adapter presets and custom mode, it remains commented/unset)
- Updates `.gitignore` so the app's `.tako/*` stays ignored while `.tako/secrets.json` remains trackable (repo-root `.gitignore` when inside git, app-local `.gitignore` otherwise)
- Includes commented examples/explanations for all supported `tako.toml` options:
  - `name`, `main`, top-level `runtime`/`preset`/`assets`/`dev`, `[build]` (`run`, `install`, `include`, `exclude`), and `[[build_stages]]` (with per-stage `exclude`)
  - `[vars]`
  - `[vars.<env>]`
  - `[envs.<env>]` route declarations (`route`/`routes`), server membership (`servers`), and idle scaling policy (`idle_timeout`)
- Includes a docs link to `https://tako.sh/docs/tako-toml`.
- Writes the selected config file (default `./tako.toml`).
- Prompts for required app `name` (default from selected-config parent directory-derived app name).
- Prompts for required production route (`[envs.production].route`) with default `{name}.example.com`.
- Detects adapter (`bun`, `node`, `go`, fallback `unknown`) and prompts for runtime selection.
- After generating `tako.toml`, init installs the `tako.sh` SDK package via the selected runtime's package-manager `add` command (for JS: `bun add tako.sh`, etc.; for Go: `go get tako.sh`).
- In interactive mode, init fetches runtime-family preset names from official family manifest files (`presets/<language>.toml`) and shows `Fetching presets...` while loading.
- For built-in base adapters, init defaults to:
  - Bun: `bun`
  - Node: `node`
  - Go: `go`
- Init prints the full "Detected" summary block only in verbose mode; default output keeps setup concise and action-oriented.
- If no family presets are available after fetch, init skips preset selection and uses the runtime base preset.
- When "custom preset reference" is selected, init leaves top-level `preset` unset (commented) but still writes top-level `runtime`.
- For `main`, init behavior is:
  - if adapter inference finds an entrypoint and it differs from preset default `main`, write it as top-level `main`;
  - if inferred `main` matches preset default (or preset default exists and no inference is available), omit top-level `main`;
  - prompt only when neither adapter inference nor preset default `main` is available.

If the selected config file already exists:

- Interactive terminal: `tako init` asks for overwrite confirmation.
- Non-interactive terminal: skips overwrite, leaves the existing file untouched, and prints `Operation cancelled`.

### tako help

Show all commands with brief descriptions.

### tako version

Show version information (same as `--version` flag).

### tako typegen

Generate typed accessors for the current project: `tako.gen.ts` for JS/TS apps (runtime state + typed `Secrets` interface) and `tako_secrets.go` for Go apps. For JS/TS projects, `tako.gen.ts` is written next to any existing copy if one is found, otherwise placed inside `src/` or `app/` when those directories exist, or at the project root. Legacy `tako.d.ts` files left over from the pre-v0-global design are removed on regeneration. If a JS/TS project already has `channels/` or `workflows/` directories, typegen also scaffolds `demo.ts` in empty dirs and adds missing default `defineChannel(...)` / `defineWorkflow(...)` exports to existing definition files that have no default export yet. Generated channel stubs use the file stem as the initial channel `name`, but typegen does not rewrite existing explicit channel names.

### tako upgrade

Upgrade the local `tako` CLI binary to the latest available build.

CLI upgrade strategy:

- Homebrew install detection: runs `brew upgrade tako`
- Default/fallback: downloads and runs hosted installer (`https://tako.sh/install.sh`) via `curl`/`wget`

### tako dev [--variant {variant}]

Start (or connect to) a local development session for the current app, backed by a persistent dev daemon.

- `--variant` (alias `--var`) runs a DNS variant of the app (e.g. `--variant foo` → `myapp-foo.test`).
- `tako dev` is a **client**: it ensures `tako-dev-server` is running, then registers the selected config file with the daemon.
  - On macOS, `tako dev` also ensures the socket-activated `tako-dev-proxy` helper is installed and loaded for loopback-only `:80/:443` ingress.
  - On Linux, `tako dev` ensures iptables redirect rules and a loopback alias (`127.77.0.1`) are configured for portless HTTPS. On NixOS, it prints a `configuration.nix` snippet instead of imperative setup.
  - When running from a source checkout, `tako dev` prefers the repo-local `target/debug|release/tako-dev-server` binary.
  - When running from a source checkout on macOS, `tako dev` can also build the repo-local `tako-dev-proxy` binary when the helper needs installation or repair.
  - If no local daemon binary exists, `tako dev` falls back to `tako-dev-server` on `PATH`.
  - If that fallback binary is missing:
    - source checkout flow reports a build hint (`cargo build -p tako --bin tako-dev-server`)
    - installed CLI flow reports a reinstall hint (`curl -fsSL https://tako.sh/install.sh | sh`)
  - If daemon startup fails, `tako dev` reports the last lines from `{TAKO_HOME}/dev-server.log`.
  - `tako dev` waits up to ~15 seconds for the daemon socket after spawn before reporting startup failure.
  - The daemon performs an upfront bind-availability check for its HTTPS listen address and exits immediately with an explicit error when that address is unavailable.
- `tako dev` **registers** the app with the daemon (selected config path is the unique key, state is persisted in SQLite at `{TAKO_HOME}/dev-server.db`).
- App statuses: `running` (actively serving), `idle` (process stopped, routes retained for wake-on-request), `stopped` (unregistered, routes removed).
- The app starts immediately when `tako dev` starts (1 local instance) and transitions to idle after 30 minutes of no attached CLI clients.
  - After an idle transition, the next HTTP request triggers wake-on-request: the daemon spawns the app process and routes the request once the app is healthy.
  - Idle shutdown is suppressed while there are in-flight requests.
  - When `Ctrl+c` is pressed, Tako unregisters the app (sets status to stopped, removes routes, kills the process).
  - Pressing `b` (background) hands the running process off to the daemon and exits the CLI. The daemon monitors the process and keeps routes active.
  - Running `tako dev` again with the same selected config file attaches to the existing session if the app is running or idle.
  - Dev logs are written to a shared per-app/per-config stream at `{TAKO_HOME}/dev/logs/{app}-{hash}.jsonl`.
  - Each persisted log record stores a single `timestamp` token (`hh:mm:ss`) instead of split hour/minute/second fields.
  - When a new owning session starts, Tako truncates that shared stream before writing fresh logs for the new session.
  - Attached clients replay the existing file contents, then follow new lines from the same stream.
  - App lifecycle state (`starting`, `running`, `stopped`, app PID, and startup errors) is persisted to the same shared stream, so attached sessions reconstruct the same status/CPU/RAM view as the owning session.
  - The CLI prints `App started` once the daemon has confirmed the app is live. Active routes are shown in the status footer, not in this message.
- The daemon supports **multiple concurrent apps** and maintains hostname-based routing for `*.test` (and `*.tako.test` as a fallback).
  - When LAN mode is enabled from the interactive UI (`l`), the same registered dev routes are also reachable via `.local` aliases. Hostnames are rewritten only at the suffix, so subdomains, wildcard hosts, and path-prefixed routes keep the same shape.
    - Concrete hostnames are advertised to the LAN via mDNS (Bonjour on macOS, Avahi on Linux) so phones and tablets resolve them by name.
    - Wildcard routes (e.g. `*.app.test`) cannot be advertised via mDNS — the protocol only supports concrete records. They still match at the proxy, so devices with their own DNS server for the subdomain can reach them, but plain mDNS clients (phones) cannot. Tako surfaces a warning under the LAN mode route list pointing to the wildcard routes and suggesting an explicit subdomain route (e.g. `api.app.test`) as the fix.
- When running in an interactive terminal, `tako dev` prints a branded header (logo + version + app info) once at startup, then streams logs and status updates directly to stdout.
  - Native terminal features (scrollback, search, copy/paste, clickable links) are preserved — no alternate screen is used.
  - Log levels are `DEBUG`, `INFO`, `WARN`, `ERROR`, and `FATAL`; the level token is colorized using pastel colors (electric blue, green, yellow, red, and purple respectively).
  - The timestamp token (`hh:mm:ss`) is rendered in a muted color.
  - Log lines are prefixed as `hh:mm:ss LEVEL [scope] message`.
    - Common scopes: `tako` (local dev daemon) and `app` (the app process).
    - For app-process output, Tako infers the level from leading tokens like `DEBUG`, `INFO`, `WARN`/`WARNING`, `ERROR`, and `FATAL` (including bracketed forms such as `[DEBUG]`), and maps `TRACE` to `DEBUG`.
  - App lifecycle state changes (starting, stopped, errors) are printed inline as `── {status} ──` lines in the log stream.
  - Keyboard shortcuts (interactive terminal only):
    - `r` restart the app process
    - `l` toggle LAN mode (expose the same routes via `.local` aliases on the local network)
    - `b` background the app (hand off to daemon, CLI exits)
    - `Ctrl+c` stop the app and quit
  - When stdout is not a terminal (piped or redirected), `tako dev` falls back to plain `println`-style output with no color or raw mode.
  - `tako dev` always watches `tako.toml` and:
  - restarts the app when effective dev environment variables change
  - updates dev routing when `[envs.development].route(s)` changes
- Source hot-reload is runtime-driven (e.g. Bun watch/dev scripts); Tako does not watch source files for auto-restart.
- HTTPS is terminated by the local dev daemon using certificates issued by the local CA (SNI-based cert selection).
- `tako dev` ensures daemon TLS files exist at `{TAKO_HOME}/certs/fullchain.pem` and `{TAKO_HOME}/certs/privkey.pem` before spawning the daemon.
  - The daemon reuses existing TLS files when present.
- `tako dev` listens on `127.0.0.1:47831` in HTTPS mode.
- When `[envs.development].routes` is not configured, Tako registers `https://{app}.test:47831/` on non-macOS and `https://{app}.test/` on macOS. When the user configures explicit `.test`/`.tako.test` routes, those managed dev routes replace the default entirely — the default `{app}.test` host is not added, leaving that slug free for other apps. External development routes (for example a hostname forwarded by Cloudflare Tunnel) are additive: if no managed dev route is configured, Tako still registers the default `{app}.test` route alongside the external routes. Both `.test` and `.tako.test` DNS zones resolve simultaneously (`.tako.test` remains available as a DNS fallback; the proxy still only routes hosts that are actually registered).
  - In LAN mode, managed `.test`/`.tako.test` dev routes are additionally served via `.local` aliases (for example `app.test/api/*` also answers on `app.local/api/*`). External development routes are routable by the local proxy but are not rewritten to `.local`, advertised with mDNS, or resolved by Tako DNS.
  - On macOS, Tako configures split DNS by writing `/etc/resolver/test` and `/etc/resolver/tako.test` (one-time sudo), pointing to a local DNS listener on `127.0.0.1:53535`. If `/etc/resolver/test` already exists and was not created by Tako, Tako skips it and warns about the conflict (`.tako.test` still works).
  - On Linux, systemd-resolved routes both `~test` and `~tako.test` to the local DNS listener.
  - The dev daemon answers `A` queries for active `*.test` and `*.tako.test` hosts.
    - On macOS, it maps to a dedicated loopback address (`127.77.0.1`) used by the dev proxy.
    - On non-macOS, it maps to `127.0.0.1`.
  - On macOS, `tako dev` automatically installs and repairs a launchd-managed dev proxy when missing (one-time sudo prompt):
    - Tako also installs a boot-time launchd helper that ensures the dedicated loopback alias (`127.77.0.1`) exists before the proxy is re-registered
    - launchd owns listening sockets only on `127.77.0.1`
    - `127.77.0.1:443 -> 127.0.0.1:47831`
    - `127.77.0.1:80 -> 127.0.0.1:47830` (HTTP redirect to HTTPS)
  - The dev proxy is socket-activated and may exit after a long idle window; launchd reactivates it on the next request.
  - If the dev proxy later appears inactive, `tako dev` explains that it is reloading or reinstalling the launchd helper before prompting for sudo.
  - On macOS, Tako always requires this dev proxy and always advertises `https://{app}.test/` (no explicit port).
  - After applying or repairing the dev proxy, Tako retries loopback 80/443 reachability and fails startup if those endpoints remain unreachable.
  - On macOS, Tako probes HTTPS for the app host via loopback and fails startup if that probe does not succeed.
  - If the daemon is reachable on `127.0.0.1:47831` but `https://{app}.test/` still fails, Tako reports a targeted hint that the local launchd dev proxy is not forwarding correctly.
  - `tako dev` uses routes from `[envs.development]` when configured; otherwise it defaults to `{app}.test`.
    - Dev routes may use any valid hostname. Tako only manages DNS and `.local` LAN aliases for `.test` and `.tako.test` routes.
    - Wildcard dev routes participate in proxy routing, but cannot be advertised with mDNS in LAN mode.
    - If configured dev routes contain no managed `.test`/`.tako.test` routes, Tako keeps the default `{app}.test` route and treats the configured external routes as additional host aliases.
    - Unknown managed local DNS hosts (`.test` and `.tako.test`) return a helpful 421 response that lists registered dev routes. Unknown `.local` LAN hosts and unknown external hosts return a generic `Misdirected Request` 421 response and do not enumerate registered routes.
  - The HTTPS daemon listen port for `tako dev` is fixed at `47831`.

**Local CA architecture:**

- Root CA generated once on first run, private key stored in system keychain
- Keychain storage for the CA private key is scoped per `{TAKO_HOME}` to avoid cross-home key/cert mismatches.
- Leaf certificates generated on-the-fly for each app domain
- Public CA cert available at `{TAKO_HOME}/ca/ca.crt` (for `NODE_EXTRA_CA_CERTS`)
- On first run (or whenever not yet trusted), `tako dev` installs the root CA into the system trust store (may prompt for your password)
- Before the sudo prompt, `tako dev` explains why elevated access is needed and what will change.
- No browser security warnings once the CA is trusted

**Environment variables:**

- Loads from `[vars]` + `[vars.development]` in tako.toml
- `ENV=development`
- `NODE_ENV=development`, plus runtime-specific vars (`BUN_ENV=development` for Bun)

### tako dev stop [name] [--all]

Stop a running dev app.

- Without arguments: stops the app for the selected config file (default `./tako.toml`).
- With `name`: stops the app with that name.
- `--all`: stops all registered dev apps.

### tako dev ls

List all registered dev apps.

Alias: `tako dev list`.

### tako doctor

Print a local diagnostic report and exit.

- Reports dev daemon listen info, macOS dev proxy status, and local DNS status.
- On macOS, includes a preflight section with clear checks for:
  - dev proxy install status
  - dev boot-helper load status
  - dedicated loopback alias status
  - launchd load status
  - TCP reachability on `{loopback-address}:443` and `{loopback-address}:80`
- If the local dev daemon is not running (missing/stale socket), doctor reports `status: not running` with a hint to start `tako dev`, and exits successfully.

### tako servers status

Show global deployment status from configured servers, with one server block per configured host and one app block per running build nested under each server:

```
✓ la (v0.1.0) up
  ┌ dashboard (production) running
  │ instances: 2/2
  │ build: abc1234
  └ deployed: 2026-02-08 11:48:19
────────────────────────────────────────
! nyc (v0.1.0) up
  ┌ worker (unknown) running
  │ instances: 1/1
  │ build: old5678
  └ deployed: 2026-02-08 11:40:10
  ┌ worker (unknown) deploying
  │ instances: -/-
  │ build: new9012
  └ deployed: -
```

Shows server connectivity/service lines and per-build app blocks with heading lines in `app-name (environment) state` form.
Each app block uses a tree connector (`┌` heading, `│` detail continuation, `└` final deployed line).
Environment is inferred from deployed release metadata when available; otherwise app status uses `unknown`.
App state text is color-coded (`running` success, `idle` muted, `deploying`/`stopped` warning, `error` error).
Each app block includes instance summary (`healthy/total`), build, and deployed timestamp (formatted in the user's current locale and local time, without timezone suffix).
`tako servers status` prints a single snapshot and exits.

Status flow helpers:

- `tako servers status` does not require `tako.toml` and can run from any directory.
- Uses global server inventory from `config.toml`.
- If no servers are configured and the terminal is interactive, status offers to run the add-server wizard.
- If no deployed apps are found, status reports that explicitly.

Alias: `tako servers info`.

### tako logs [--env {environment}] [--tail] [--days {N}] [--json]

View or stream logs from all servers in an environment.

- Environment defaults to `production`.
- Environment must exist in the selected config file.
- Fetches from all mapped servers in parallel.
- Includes app stdout/stderr plus `tako-server` lifecycle, health, and proxy diagnostics for the
  app's deployed routes. JS/TS production HTTP entrypoints route `console.*`, uncaught
  exceptions, and unhandled rejections into the same app log stream before exiting.
- Prefixes each line with `[server-name]` when multiple servers are present.
- Remote fetch/connect failures are reported as command failures; they are not treated as empty logs.
- `--json` emits compact JSONL for agents and automation: one log event per stdout line with
  stable short keys and no human progress output on stdout.

**History mode (default):**

- Shows the last `N` days of logs (default: 3).
- Applies `--days` to timestamped app log-file lines and server journal diagnostics.
- Consecutive identical messages are deduplicated with "... and N more" suffix.
- All lines across servers are sorted by timestamp.
- Displays in `$PAGER` (default: `less -R`) if interactive, otherwise stdout.

**Streaming mode (`--tail`):**

- Streams logs continuously until interrupted (`Ctrl+c`).
- `--tail` conflicts with `--days`.
- Consecutive identical messages are deduplicated with "... and N more" suffix.

Logs flow helpers:

- For `production`, if no servers are configured and the terminal is interactive, logs offers to run the add-server wizard.

### tako servers add [host] [--name {name}] [--description {text}] [--port {port}]

Add server to global `config.toml` (`[[servers]]`).

- With `host`: adds directly from CLI args.
- With `host`: `--name` is required (no implicit default to hostname).
- Without `host` (interactive terminal): launches a guided wizard (host, required server name, optional description, SSH port) with a final `Looks good?` confirmation. Choosing `No` restarts the wizard.
- The add-server wizard supports `Tab` autocomplete suggestions for host/name/port from existing servers and persisted CLI history.
  - For name/port prompts, suggestions related to the selected host (and selected name for ports) are prioritized first, then global suggestions are shown.
- Successful adds record host/name/port history in `history.toml` for future autocomplete.
- `--description` stores optional human-readable metadata in `config.toml` (shown in `tako servers ls`).
- Re-running with the same name/host/port is idempotent (reports already configured and succeeds).

Tests SSH connection before adding. Connects as the `tako` user.

During SSH checks, `tako servers add` also detects and stores target metadata (`arch`, `libc`) in the matching `[[servers]]` entry in `config.toml`.

If `--no-test` is used, SSH checks and target detection are skipped; deploy later fails for that server until target metadata is captured by re-adding the server with SSH checks enabled.

If `tako-server` is not installed on the target, `tako` warns and expects the user to install it manually.

### tako servers rm [name]

Remove server from `config.toml` (`[[servers]]`).

When `name` is omitted in an interactive terminal, `tako` opens a server selector.
In non-interactive mode, `name` is required.

Confirms before removal. Warns that projects referencing this server will fail.

Aliases: `tako servers remove [name]`, `tako servers delete [name]`.

### tako servers ls

List all configured servers from global config (`config.toml`) as a table:

- Name
- Host
- Port
- Optional description

Alias: `tako servers list`.

If no servers are configured, `tako servers ls` shows a hint to run `tako servers add`.

### tako servers restart {server-name} [--force]

Reload `tako-server` without downtime by default. `--force` performs a full service restart and may cause brief downtime for all apps.

Use default reload for normal config refresh and control-plane restarts. Use `--force` for recovery when graceful reload is not appropriate.

Service-manager reload/restart behavior:

- Default path: `systemctl reload tako-server` on systemd hosts, or `rc-service tako-server reload` on OpenRC hosts. Reload sends `SIGHUP`; the current process spawns a replacement process, the new process takes over the management socket and listener ports, then the old process drains and exits.
- `--force` path: `systemctl restart tako-server` on systemd hosts, or `rc-service tako-server restart` on OpenRC hosts.
- On systemd hosts, installer configures `KillMode=control-group` and `TimeoutStopSec=30min`, allowing all app processes in the service cgroup time to handle graceful shutdown before forced termination.
- On OpenRC hosts, installer configures `retry="TERM/1800/KILL/5"` in the init script so restart/stop waits up to 30 minutes before forced termination.

`tako-server` persists app runtime registration (app config and routes) in SQLite under the data directory and restores it on startup so app routing/config survives reloads, restarts, and crashes. Env vars are stored in `app.json` in the release directory; secrets are stored encrypted (AES-256-GCM) in the same SQLite database using a per-device key. Secrets are pushed to app instances via `POST /secrets` on `Host: tako.internal` over the instance's private TCP endpoint with the per-instance internal token header — they never touch disk as plaintext. Each deployed app also gets a persistent runtime data tree under `{data_dir}/apps/{app}/data/`:

- `app/` — app-owned data exposed to the process as `TAKO_DATA_DIR`
- `tako/` — Tako-owned per-app internal state

Deleting an app removes the entire `{data_dir}/apps/{app}` tree after the app is drained and stopped.

During single-host upgrade orchestration, `tako-server` may enter an internal `upgrading` server mode that temporarily rejects mutating management commands (`deploy`, `stop`, `delete`, `update-secrets`) until the upgrade window ends.
Upgrade mode transitions are guarded by a durable single-owner upgrade lock in SQLite so only one upgrade controller can hold the upgrade window at a time.

### tako servers upgrade [server-name]

Upgrade `tako-server` on one or all configured servers via service-manager reload. When `server-name` is omitted, all servers are upgraded.

1. CLI verifies `tako-server` is active on the host.
2. CLI installs the new server binary on the host.
   - CLI verifies the signed `tako-server-sha256s.txt` release manifest with an embedded public key, selects the expected SHA-256 for the target archive, and the remote host verifies that SHA-256 before extracting the archive into `/usr/local/bin/tako-server`
   - custom `TAKO_DOWNLOAD_BASE_URL` overrides must use `https://`; non-HTTPS overrides are rejected unless `TAKO_ALLOW_INSECURE_DOWNLOAD_BASE=1` is set explicitly for local testing
   - CLI metadata downloads and remote host archive downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`, for GitHub-hosted release URLs only
3. CLI acquires the durable upgrade lock (`enter_upgrading`) and sets server mode to `upgrading`.
4. CLI signals the primary service with:
   - `systemctl reload tako-server` on systemd hosts, or
   - `rc-service tako-server reload` on OpenRC hosts.
     Both paths send `SIGHUP` for graceful reload, start a replacement process before the old process exits, and run with root privileges (root login or sudo-capable user).
5. CLI waits for the primary management socket to report ready.
6. CLI releases upgrade mode (`exit_upgrading`).

`tako servers upgrade` requires a supported service manager on the host (systemd or OpenRC).

Failure behavior:

- If failure happens before the reload signal, CLI performs best-effort cleanup (exits upgrade mode).
- Upgrade keeps the previous on-disk `tako-server` binary until the replacement process reports ready. If readiness does not arrive, the previous binary is restored.
- If the reload was sent but the socket did not become ready within the timeout, CLI warns that upgrade mode may remain enabled until the primary recovers.

### tako servers implode [name] [-y|--yes]

Remove tako-server and all data from a remote server.

1. If `name` is omitted in an interactive terminal, prompts to select from configured servers.
2. Displays what will be removed (services, binaries, data, sockets, service files) and asks for confirmation (skipped with `-y`).
3. SSHes into the server with root/sudo privileges and:
   - Stops and disables `tako-server` and `tako-server-standby` services (systemd and OpenRC).
   - Removes systemd service files, drop-ins, and OpenRC init scripts.
   - Runs `systemctl daemon-reload` on systemd hosts.
   - Removes binaries: `/usr/local/bin/tako-server`, `tako-server-service`, `tako-server-install-refresh`.
   - Removes data directory (`/opt/tako/`) and the management socket directory (`/var/run/tako/`).
4. Removes the server from the local `config.toml` server list.

Alias: `tako servers uninstall`.

### tako servers setup-wildcard [-e|--env ENV]

Configure DNS-01 wildcard certificate support on all servers.

1. Loads all configured servers (or servers for `--env` if specified).
2. Runs an interactive wizard prompting for DNS provider and credentials.
3. Verifies credentials locally against the provider API.
4. Applies the configuration to all servers in parallel:
   - Writes credentials to `/opt/tako/dns-credentials.env` (mode 0600)
   - Merges `dns.provider` into `/opt/tako/config.json`
   - Writes a systemd drop-in to inject the env file and restarts `tako-server`
   - Polls `tako-server` to confirm the provider is active
5. `tako-server` downloads and installs lego on-demand when issuing wildcard certificates.

### tako implode [-y|--yes]

Remove the local Tako CLI and all local data.

1. Gathers removal targets:
   - **User-level:** config directory, data directory, CLI binaries (`tako`, `tako-dev-server`, `tako-dev-proxy`).
   - **System-level (requires sudo):** platform-specific services and config installed by `tako dev`:
     - macOS: dev proxy LaunchDaemons (`sh.tako.dev-proxy`, `sh.tako.dev-bootstrap`), `/Library/Application Support/Tako/`, `/etc/resolver/test`, `/etc/resolver/tako.test`, CA certificate in system keychain, loopback alias `127.77.0.1`.
     - Linux: systemd service (`tako-dev-redirect.service`), systemd-resolved drop-in (`tako-dev.conf`), CA certificate in system trust store, iptables NAT redirect rules, loopback alias `127.77.0.1`.
2. If nothing exists, reports "nothing to remove" and exits.
3. Displays what will be removed (including system items that require sudo) and asks for confirmation (skipped with `-y`).
4. Best-effort stops the dev server (unregisters all dev apps).
5. Removes system-level items via `sudo` (best-effort), then removes user-level directories and binaries.
6. Reports success or partial removal if some items could not be deleted.

Alias: `tako uninstall`.

### tako secrets set [--env {environment}] [--sync] {name}

Set/update secret for an environment.

When `--env` is omitted in an interactive terminal, Tako opens an environment wizard. The first step shows the default environments (`development`, `production`), any environments already declared in `tako.toml` or `.tako/secrets.json`, and a `New environment` option. Choosing `New environment` prompts for the environment name in the next wizard step. In non-interactive mode, `--env` is required.

After the environment is resolved, Tako prompts for the secret value with masked input in an interactive terminal, or reads a single line from stdin in non-interactive mode. If the secret already exists in the selected environment during an interactive run, Tako asks for overwrite confirmation before prompting for the new value. Stores encrypted value locally in `.tako/secrets.json`. Tako does not write `.tako/secrets.json` until the environment wizard and value prompt have both completed.

Uses the environment's cached key from Keychain or Tako's data directory at `keys/{key_id}`. If the environment has no key yet, Tako creates a random key. On macOS interactive runs, Tako can store the new key in Keychain instead of a local file.

When `--sync` is provided, immediately syncs secrets to all servers in the target environment after the local change, triggering a rolling restart of running instances.

Alias: `tako secrets add ...`.

### tako secrets rm [--env {environment}] [--sync] {name}

Remove secret from environment.

Removes from local `.tako/secrets.json`. Omitting `--env` removes the secret from all environments.

When `--sync` is provided, immediately syncs secrets to servers after the local change. If `--env` is specified, syncs to that environment; otherwise syncs to all environments.

Aliases: `tako secrets remove ...`, `tako secrets delete ...`, `tako secrets del ...`.

### tako secrets ls

List all secrets with presence table across environments.

Shows which secrets exist in which environments. Warns about missing secrets. Never displays values.

Aliases: `tako secrets list`, `tako secrets show`.

### tako secrets sync [--env {environment}]

Sync local secrets to servers.

Source of truth: local `.tako/secrets.json`.

By default, sync processes all environments declared in `tako.toml`.
When `--env` is provided, sync processes only that environment.

For each target environment, sync decrypts with the cached key from Keychain or Tako's data directory at `keys/{key_id}`.

Shows a spinner with the total number of target servers while syncing, and reports the elapsed time on completion.

Sync flow helpers:

- If no servers are configured and the terminal is interactive, sync offers to run the add-server wizard.
- Environments with no mapped servers are skipped with a warning.
- Sync sends `update_secrets` to `tako-server`; it does not write remote `.env` files. Secrets updates reconcile the app's workflow runtime and rolling-restart HTTP instances so fresh processes receive the new values via fd 3.

### tako secrets key export [--env {environment}]

Export a self-contained key bundle to clipboard.

Reads the environment's cached key from Keychain or Tako's data directory at `keys/{key_id}` and copies a single exported key string to the clipboard. The string is base64url-encoded JSON containing `version`, `id`, and `key`, so it can be imported without specifying an environment.

When `--env` is omitted in an interactive terminal, Tako opens the environment wizard. In non-interactive mode, `--env` is required.

### tako secrets key import [--exported-key|--passphrase] [--env {environment}]

Import a self-contained exported key string.

In interactive mode, asks for the key source:

- `Exported key`: prompts for an exported key string with masked input. The payload contains the key id, so no environment is needed.
- `Passphrase`: prompts for an environment and passphrase. Tako derives the environment key from the passphrase and the environment key id. If the environment does not have a key id yet, Tako creates one and saves it to `.tako/secrets.json` after the passphrase flow completes.

In non-interactive mode, pass `--exported-key` or `--passphrase`. `--passphrase` also requires `--env`. Both sources read a single line from stdin. Imported keys are stored under Tako's data directory at `keys/{id}` by default. On macOS interactive runs, Tako can store the imported key in Keychain instead of a local file. If the current project has an environment matching the imported `id`, reports that environment name; otherwise reports the imported id.

### tako deploy [--env {environment}] [--yes|-y]

Build and deploy application to environment's servers.

When `--env` is omitted, deploy targets `production`.

Deploy target environment must be declared in `tako.toml` (`[envs.<name>]`) and must define `route` or `routes`.

`development` is reserved for `tako dev` and cannot be used with `tako deploy`.

In interactive terminals, deploying to `production` requires an explicit confirmation unless `--yes` (or `-y`) is provided.

Deploy flow helpers:

- If no servers are configured and the terminal is interactive, deploy offers to run the add-server wizard before continuing.
- For `production`, if `[envs.production].servers` is empty:
  - with one global server: deploy selects it and writes it to `[envs.production].servers` in `tako.toml`
  - with multiple global servers (interactive terminal): deploy asks you to pick one, then writes it to `[envs.production].servers`
- Interactive deploy progress:
  - after config/server/build planning is known, interactive pretty output renders tasks and sub tasks instead of a static plan box
  - waiting tasks render as muted `○`
  - deploy renders `Connecting to <server>` as a single sub task when there is one target server; with multiple target servers it renders a `Connecting` task with one sub task per server
  - if there is only one obvious build task, deploy renders it as a single `Building` sub task line
  - pending pretty task rows render with a `...` suffix
  - succeeded sub tasks hide the `✔` icon (render with a blank icon slot) only when their parent task also succeeded; when the parent failed, was cancelled, or is still running, succeeded sub tasks keep their `✔` so the completed work stays visible. Failed (`✘`), cancelled (`⊘`), skipped (`⏭`), running (spinner), and pending (`○`) sub tasks always keep their icons
  - cancelled and skipped rows render fully muted (icon, label, and detail); accent color is reserved for live or successfully completed rows
  - after planning completes, deploy starts the pretty `Connecting` and `Building` sections together
  - deploy does not keep startup metadata summaries inside the live tree
  - deploy renders one task per target server, with sub tasks for `Uploading`, `Preparing`, and `Starting`
  - deploy adds a blank line after each top-level pretty task section (`Connecting`, `Building`, each `Deploying to ...`)
  - sub task failures may render their related error detail on an indented line below the failed sub task
  - if a connection check or build step fails, deploy aborts the remaining incomplete pretty task-tree rows and marks them as `Aborted` instead of leaving them pending
  - verbose and CI deploy output stay transcript-style and only print work as it is happening
  - When `release` is configured for the resolved env, deploy adds a
    release sub-step under each server's `Preparing` task. The leader's
    row reads `Running release command`; followers' rows read
    `Waiting for release command` and resolve once the leader finishes.
    On leader failure, followers' rows are marked `Cancelled` with a
    `leader failed` detail.

**Steps:**

1. Pre-deployment validation (secrets present, server target metadata present/valid for all selected servers)
2. Resolve source bundle root (git root when available; otherwise app directory)
3. Resolve app subdirectory from the selected config file's parent directory relative to source bundle root
4. Resolve deploy runtime `main` (`main` from `tako.toml`; otherwise manifest main such as `package.json` `main`; otherwise preset `main`, with JS index fallback order: `index.<ext>` then `src/index.<ext>` for `ts`/`tsx`/`js`/`jsx` when applicable)
5. Resolve app preset (top-level `preset` in `tako.toml`), fetching unpinned official aliases from `master`
6. Prepare build dir: copy project from source root into `.tako/build` (respecting `.gitignore`), symlink `node_modules/` directories from original tree
7. Run build commands in build dir:
   - Resolve stage list by precedence: `[[build_stages]]` → `[build]` (single-stage form) → runtime default stage → no-op
   - Run resolved stages in declaration order (`install` then `run` per stage)
   - Merge configured assets into app `public/`
   - Verify resolved runtime `main` exists in the built app directory
   - Save resolved runtime version into `app.json` (`runtime_version` field) for server-side version pinning
8. Archive build dir (excluding `node_modules/`) as deploy artifact
   - Version format: clean git tree => `{commit}`; dirty git tree => `{commit}_{source_hash8}`; no git commit => `nogit_{source_hash8}`
   - Best-effort local artifact cache prune runs before builds (retention: 90 target artifacts; orphan target metadata is removed)
   - Package filtered artifact tarball using include/exclude rules and store in local cache
9. On all servers in parallel: upload artifact, extract, and run production install
   - Require `tako-server` to be pre-installed and running on each server
   - Upload and extract target-specific artifact
   - Query server for the app's current secrets hash; if it matches the local secrets hash, skip sending secrets (server keeps existing). If hashes differ (or app is new), include decrypted secrets in the deploy command.
   - `tako-server` acquires a per-app deploy lock in memory, reads non-secret runtime/app config from release `app.json`, creates per-app runtime data directories, and runs the runtime plugin's production install command
   - If another deploy for the same app environment is already running on that server, the deploy command fails immediately with a retry message
10. Run release command on the leader server (when configured):
    - If `release` is configured for the resolved env, the leader server
      runs `sh -c "<command>"` once inside the new release directory.
      Followers' `Preparing` task blocks until the leader publishes its
      result.
    - On success, all servers proceed into rolling update (existing
      behavior).
    - On failure (non-zero exit, timeout, or signal), deploy aborts on
      every server. The existing partial-release cleanup removes the new
      release directory on each server. The `current` symlink is not
      updated; old instances keep serving.
11. Rolling update and finalize on all servers:
    - `tako-server` performs first start or rolling update
    - Update `current` symlink and clean up old releases (>30 days)

**Version naming:**

- Clean git tree: `{commit_hash}` (e.g., `abc1234`)
- Dirty working tree: `{commit_hash}_{content_hash}` (first 8 chars each)
- No git commit/repo: `nogit_{content_hash}` (first 8 chars)

**Source deploy contract:**

- Deploy archive source is the app's source bundle root (git root when available; otherwise selected-config parent directory).
- Deploy target app path is the selected config file's parent directory relative to the source bundle root.
- Build uses a build dir: copies project from source root into `.tako/build` (respecting `.gitignore`), symlinks `node_modules/` from the original tree (build tools read but don't modify), runs build commands in the build dir, then archives the result excluding `node_modules/`.
- These paths are always force-excluded from the deploy archive: `.git/`, `.tako/`, `.env*`, `node_modules/`. Additional exclusions come from `[build].exclude` and `.gitignore`.
- Servers receive prebuilt artifacts and do not run app build steps during deploy. After extracting the artifact, `tako-server` runs the runtime plugin's production install command (e.g. `bun install --production`) before starting instances.
- Build logic runs in the build dir against the resolved stage list (precedence: `[[build_stages]]` → `[build]` → runtime default). Each stage runs `install` then `run` in declaration order.
- Deploy uses `runtime_version` from `tako.toml` when set. Otherwise it resolves runtime version by running `<tool> --version` directly, falling back to `latest`.
- Artifact include precedence: in simple build mode, `build.include` -> `**/*`. In multi-stage mode, `**/*` is used (stages control output via `exclude` patterns only).
- Asset roots are preset `assets` plus top-level `assets` (deduplicated), merged into app `public/` after build with ordered overwrite.
- Target artifacts are cached locally by deterministic key and reused across deploys when build inputs are unchanged.
- Cached artifacts are validated by checksum/size before reuse; invalid cache entries are rebuilt automatically.
- Deploy artifacts include the canonical `app.json` used by `tako-server` at runtime.
- Release `app.json` contains resolved runtime metadata (`runtime`, `main`, `package_manager`), non-secret env vars, environment idle timeout, and optional release metadata (`commit_message`, `git_dirty`) used by `tako releases ls`.
- Deploy does not write a release `.env` file; non-secret env vars live in release `app.json`, secrets are stored encrypted in SQLite on the server, and `tako-server` injects runtime vars (`TAKO_BUILD`, `TAKO_DATA_DIR`) when spawning HTTP instances and workflow workers.
- Deploy queries each server's secrets hash before sending the deploy command. If the hash matches the local secrets, secrets are omitted from the payload and the server keeps its existing secrets. This avoids unnecessary secret transmission and ensures new servers or servers with stale secrets are automatically provisioned.
- Deploy requires valid `arch` and `libc` metadata in each selected `[[servers]]` entry.
- Deploy does not probe server targets during deploy; missing/invalid target metadata fails deploy early with guidance to remove/re-add affected servers.
- Deploy pre-validation still fails when target environment is missing secret keys used by other environments.
- Deploy pre-validation warns (but does not fail) when target environment has extra secret keys not present in other secret environments.

**Deploy lock (server-side):**

- `tako-server` serializes deploys per deployed app id (`{name}/{env}`) using an in-memory lock
- A second deploy command for the same app environment on the same server fails immediately with `Deploy already in progress for app '{app}'. Please wait and try again.`
- No `.deploy_lock` directory is written to disk
- Restarting `tako-server` clears the lock; the interrupted deploy fails and can be retried without manual lock cleanup
- The same per-app deploy lock guards the release command; a concurrent
  deploy attempt for the same app sees the existing
  "Deploy already in progress" error.

**Rolling update (per server):**

1. Start new instance
2. Wait for health check pass (30s timeout)
3. Add to load balancer
4. Gracefully stop old instance (drain connections, 30s timeout)
5. Repeat until all instances replaced
6. Update `current` symlink to the new release directory
7. Clean up releases older than 30 days

Rolling update target counts use the app's current desired instance count stored on that server (not old+new combined counts).
When the stored desired instance count is `0`, rolling deploy still starts one warm instance for the new build so traffic is immediately served after deploy.

**On failure:** Automatic rollback - kill new instances, keep old ones running, return error to CLI.

**App start command (current):**

- Release `app.json` is required for app startup.
- tako-server derives the start command from the runtime plugin, resolving the SDK entrypoint from the app dir or parent dirs:
  - `bun`: `bun run <resolved-entrypoint> <app.json.main>`
  - `node`: `node --experimental-strip-types <resolved-entrypoint> <app.json.main>`
  - `go`: `<app.json.main>` (compiled binary runs directly — no runtime binary or SDK entrypoint wrapper needed)
  - if the entrypoint is missing, warm-instance startup fails with an explicit error
- Unknown runtime values in `app.json` are rejected with an explicit unsupported-runtime error.

**Partial failure:** If some servers fail while others succeed, deployment continues. Failures are reported at the end.

**Disk space preflight:** Before uploading artifacts, `tako deploy` checks free space under `/opt/tako` on each target server.

- Required free space is based on archive size plus unpack headroom.
- If free space is insufficient, deploy fails early with required vs available sizes.

**Failed deploy cleanup:** If a deploy fails after creating a new release directory, `tako deploy` automatically removes that newly-created partial release directory before returning an error.

**Deployment target:**

- If `[envs.<env>].servers` exists in `tako.toml` → deploy to those servers
- If deploying to `production` with no `[envs.production].servers` mapping:
  - exactly one server in `config.toml` `[[servers]]` → use it and persist it into `[envs.production].servers`
  - multiple servers in `config.toml` `[[servers]]` (interactive terminal) → prompt to select one and persist it into `[envs.production].servers`
- If no servers exist in `config.toml` `[[servers]]` → fail with hint to run `tako servers add <host>`
- Otherwise, require explicit `[envs.<env>].servers` mapping in tako.toml

**Release command test coverage note:** End-to-end coverage for `release` (docker compose harness with success and failure fixtures) is deferred. Behavior is currently verified by Rust unit tests at the runner, dispatch, resolver, orchestration, and task-tree layers.

### tako releases ls [--env {environment}]

List release/build history for the current app across mapped environment servers.

- Environment defaults to `production`.
- Environment must exist in `tako.toml` (`[envs.<name>]`).
- Server targeting follows `[envs.<name>].servers` for the selected environment.
- Output is release-centric and sorted newest-first:
  - line 1: release/build id + deployed timestamp
    - when deployed within 24 hours, append a muted relative hint in braces (for example `{3h ago}`)
  - line 2: commit message + cleanliness marker (`[clean]`, `[dirty]`, or `[unknown]`)
- `[current]` marks the release currently pointed to by server `current` symlink.
- Commit metadata (`commit_message`, `git_dirty`) comes from release `app.json` when available; older releases may show `[unknown]` or `(no commit message)`.

### tako releases rollback {release-id} [--env {environment}] [--yes|-y]

Roll back the current app/environment to a previously deployed release/build id.

- Environment defaults to `production`.
- In interactive terminals, rollback to `production` requires explicit confirmation unless `--yes` (or `-y`) is provided.
- Rollback is executed per mapped server in parallel.
- tako-server performs rollback by reusing current app routes/env/secrets/scaling config and switching runtime path/version to the target release, then running the standard rolling-update flow.
- Partial failures are reported per server; successful servers remain rolled back.

### tako scale {instances} [--env {environment}] [--server {server}] [--app {app}]

Change the desired instance count for a deployed app.

- `instances` is the desired instance count per targeted server.
- In project context, Tako resolves the app name from the selected config file (or selected-config parent directory fallback when top-level `name` is unset).
- In project context, app-scoped server commands target the remote deployment identity `{app}/{env}`.
- Outside project context, `--app` is required. Use `--app <app> --env <env>` or pass the full deployment id as `--app <app>/<env>`.
- When `--server` is omitted, `--env` is required and Tako scales every server listed in `[envs.<env>].servers`.
- When `--server` is provided, Tako scales only that server.
- In project context, `tako scale --server <server>` defaults to `production`.
- When both `--env` and `--server` are provided, the server must belong to that environment.
- Scale uses persisted runtime app state on the server, so the desired instance count survives deploys, rollbacks, and server restarts.
- Scaling to `0` drains and stops excess instances after in-flight requests finish (or drain timeout).

### tako delete [--env {environment}] [--server {server}] [--yes|-y]

Delete a deployed app from one specific environment/server deployment target.

Target selection behavior:

- `tako delete` removes exactly one deployment target, not every server in an environment.
- In an interactive terminal, when Tako needs more information it first loads deployment state with a `Getting deployment information` spinner.
- In project context (selected config file present), Tako resolves the app name from the selected config:
  - with neither `--env` nor `--server`, Tako prompts with deployed targets like `production from hkg`
  - with `--env` only, Tako prompts for a matching server
  - with `--server` only, Tako prompts for a matching environment
  - with both `--env` and `--server`, Tako skips discovery and goes straight to confirmation
- Outside project context, Tako discovers deployed targets across configured servers and includes app selection when needed because there is no local app context.
- In non-interactive mode, `--yes`, `--env`, and `--server` are all required. Outside project context, those flags must still identify a single deployed target; otherwise the command fails with guidance to rerun interactively from the app directory or with `-c`.

Validation:

- In project mode, `--env` must be declared in the selected config file (`[envs.<name>]`).
- `--server` must name a configured server from `config.toml` `[[servers]]`.
- `development` is reserved for `tako dev` and cannot be used with `tako delete`.

Delete confirmation:

- Interactive terminals require explicit confirmation unless `--yes` (or `-y`) is provided.
- The confirmation prompt always names the app, environment, and server being removed.
- Non-interactive terminals require `--yes`.

**Steps:**

1. Connect over SSH to the selected server.
2. Send `delete` to `tako-server` for the remote deployment id `{app-name}/{env-name}`.
3. Remove `/opt/tako/apps/{app-name}/{env-name}` from disk.

- Interactive single-target deletes show a spinner while the selected server is being cleaned up.
- Delete is idempotent for absent app state (safe to re-run for cleanup).

Aliases: `tako rm`, `tako remove`, `tako undeploy`, `tako destroy`.

## Routing and Multi-App Support

### Route Configuration

Apps specify routes at environment level (not per-server). Routes support:

- Exact hostname: `api.example.com`
- Wildcard subdomain: `*.api.example.com`
- Hostname + path: `api.example.com/api/*`
- Wildcard + path: `*.example.com/admin/*`

**Validation rules:**

- Routes must include hostname (path-only routes invalid: `"/api/*"` ❌)
- Exact path routes normalize trailing slash (`example.com/api` and `example.com/api/` are equivalent)
- Each `[envs.{env}]` can have either `route` or `routes`, not both
- `[envs.{env}]` accepts only route keys (`route`/`routes`); env vars belong in `[vars]` / `[vars.{env}]`
- Each non-development environment must define `route` or `routes`
- Empty route lists are invalid for non-development environments
- Development routes may use any valid hostname. Tako manages DNS and `.local` LAN aliases only for `.test` and `.tako.test` routes; external dev routes must be pointed at the dev proxy by the user.

### Multi-App Scenarios

**Apps with routes:**

- Each app specifies its routes
- Requests matched to most specific route (exact > wildcard, longer path > shorter)
- For static asset requests (paths with a file extension), `tako-server` serves files directly from the deployed app `public/` directory when present.
- For path-prefixed routes (for example `example.com/app/*`), static asset lookup also tries the prefix-stripped path (for example `/app/assets/main.js` -> `/assets/main.js`) so public assets work on subpaths.
- Conflict detection during deploy prevents overlapping routes
- Requests without a matching route return `404`

**Wildcard subdomains:**

- `*.example.com` routes to app, app handles tenant logic based on subdomain

### Routing Logic (tako-server)

1. Parse incoming request (Host header, path)
2. Match against deployed apps' routes
3. Select most specific match
4. Route to app's load balancer (strategy: round-robin by default)
5. Return 404 if no match

## Tako Server

### Installation

Manual for v1. Users run a server setup script (or equivalent manual steps) to:

1. Create dedicated OS users: `tako` for SSH access and running `tako-server` (plus `tako-app` for optional privileged process-separation setups)
2. Install `tako-server` to `/usr/local/bin/tako-server`
3. Install and enable a host service definition for `tako-server`:
   - systemd unit on systemd hosts
   - OpenRC init script on OpenRC hosts
4. Create and permissions required directories:
   - Data dir: `/opt/tako`
   - Socket dir: `/var/run/tako`

Recommended: run the hosted installer script on the server (as root):

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

Installer SSH key behavior:

- If `TAKO_SSH_PUBKEY` is set, installer uses it and skips prompting.
- If unset and a terminal is available, installer prompts for a public key to authorize for user `tako` (including `sudo sh -c "$(curl ...)"` and common piped installs such as `curl ... | sudo sh`) and re-prompts on invalid input until a valid SSH public key line is provided.
- If terminal key input cannot be read, installer attempts to reuse the first valid key from the invoking `SUDO_USER` `~/.ssh/authorized_keys`; if unavailable, installer continues without key setup and prints a warning with a `TAKO_SSH_PUBKEY` rerun hint.
- If unset and no terminal is available, installer attempts the same invoking-user key fallback before warning and continuing without key setup.
- CLI SSH connections require host key verification against `~/.ssh/known_hosts` (or configured SSH keys directory); unknown/changed host keys are rejected.
- Installer detects host target (`arch` + `libc`) and downloads matching artifact name `tako-server-linux-{arch}-{libc}` (supported: `x86_64`/`aarch64` with `glibc`/`musl`).
- Installer ensures `nc` (netcat) is available so CLI management commands can talk to `/var/run/tako/tako.sock`.
- Installer ensures basic networking tools are available for server operation.
- Installer creates both `tako` and `tako-app` OS users.
- Installer installs restricted maintenance helpers and scoped sudoers policy so the `tako` SSH user can perform non-interactive server upgrade/reload operations.
- Installer supports systemd and OpenRC hosts.
- Installer supports install-refresh mode (`TAKO_RESTART_SERVICE=0`) for build/image workflows without active init; in this mode, it refreshes binary/users and skips service-definition install/start.
- Installer configures service capability support for privileged binds:
  - systemd: `AmbientCapabilities=CAP_NET_BIND_SERVICE`, `CapabilityBoundingSet=CAP_NET_BIND_SERVICE`
  - non-systemd hosts: installer applies `setcap cap_net_bind_service=+ep /usr/local/bin/tako-server` when available
- Installer configures graceful stop semantics:
  - systemd: `KillMode=control-group`, `TimeoutStopSec=30min`
  - OpenRC: `retry="TERM/1800/KILL/5"`
- Installer verifies `tako-server` is active after service start; if startup fails, installer exits non-zero and prints available service diagnostics.

Reference scripts in this repo:

- `scripts/install-tako-server.sh` (source for `/install-server.sh`, alias `/server-install.sh`)
- `scripts/install-tako.sh` (source for `/install.sh`)

**Runtime binary download engine:**

- `tako-server` downloads runtime binaries directly from upstream releases using download specs in runtime plugins (no external version manager dependency).
- Supports zip and tar.gz archive formats with SHA-256 checksum verification.
- Downloaded binaries are cached at `{data_dir}/runtimes/{tool}/{version}/`.
- GitHub-backed runtime version checks and runtime downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.
- Supports musl detection for Alpine and other musl-based systems.
- If the runtime plugin has no download spec, the binary must be available on PATH.

**Default behavior (no configuration file needed):**

- HTTP: port 80
- HTTPS: port 443
- Data: `/opt/tako`
- Socket: `/var/run/tako/tako.sock`
- ACME: Production Let's Encrypt
- Renewal check interval: Every 12 hours (renews certificates 30 days before expiry)
- HTTP requests redirect to HTTPS (`307`, non-cacheable) by default.
- Exception: `/.well-known/acme-challenge/*` stays on HTTP.
- Forwarded requests for private/local hostnames (`localhost`, `*.localhost`, single-label hosts, and reserved suffixes like `*.local`) are treated as already HTTPS when proxy proto metadata is missing, so local dev proxy setups do not enter redirect loops.
- Upstream response caching is enabled at the edge proxy for `GET`/`HEAD` requests (websocket upgrades are excluded).
- Cache admission follows response headers (`Cache-Control` / `Expires`) with no implicit TTL defaults; responses without explicit cache directives are not stored.
- Cache key includes request host + URI so different route hosts are isolated.
- Proxy cache storage is in-memory with bounded LRU eviction (256 MiB total, 8 MiB per cached response body).
- Per-IP rate limiting: maximum 2048 concurrent connections per client IP; excess requests receive `429`.
- Maximum request body size: 128 MiB; larger requests receive `413`.
- Production browser-facing `tako-server` 5xx responses use generic reason-phrase bodies such as `Internal Server Error`, `Bad Gateway`, `Service Unavailable`, or `Gateway Timeout`; detailed startup, proxy, channel storage, and static file diagnostics are written to server/app logs instead of response bodies.
- No application path namespace is reserved at the edge proxy. Requests are routed strictly by configured routes.

**`/opt/tako/config.json`** — server-level configuration:

```json
{
  "server_name": "prod",
  "dns": {
    "provider": "cloudflare"
  }
}
```

- `server_name` — identity label for Prometheus metrics (defaults to hostname if absent).
- `dns.provider` — DNS provider for Let's Encrypt DNS-01 wildcard challenges (configured via `tako servers setup-wildcard`).
- Written by the installer (server name) and CLI (DNS config). Read by `tako-server` at startup.

### Zero-Downtime Operation

- `tako servers restart` performs a zero-downtime control-plane reload by default (`systemctl reload tako-server` on systemd, `rc-service tako-server reload` on OpenRC). `--force` performs a full service restart instead.
- `tako servers upgrade` performs an in-place upgrade via service-manager reload (`systemctl reload tako-server` on systemd, `rc-service tako-server reload` on OpenRC) with root privileges (root login or sudo-capable user). Reload uses temporary process and listener overlap until the replacement process reports ready.
- Management socket uses a symlink-based path: the active server creates a PID-specific socket (`tako-{pid}.sock`) and atomically updates the `tako.sock` symlink on ready, so clients always connect to the current process.
- Restart/stop still honor graceful shutdown semantics from the host service manager (systemd or OpenRC as described above).

### Directory Structure

```
/opt/tako/
├── config.json
├── tako.db
├── runtimes/
│   └── {tool}/{version}/      # Downloaded runtime binaries
├── acme/
│   └── credentials.json
├── certs/
│   ├── {domain}/
│   │   ├── fullchain.pem
│   │   └── privkey.pem
└── apps/
    └── {deployment-id}/
        ├── current -> releases/{version}
        ├── data/
        │   ├── app/
        │   └── tako/
        ├── logs/
        │   └── current.log
        └── releases/{version}/
            └── build files...
```

## Communication Protocol

### Unix Sockets

**tako-server socket:**

- Symlink path: `/var/run/tako/tako.sock` (always points to the active server socket)
- PID-specific socket path: `/var/run/tako/tako-{pid}.sock` (created by active server; symlink updated atomically on ready)
- Used by: CLI for deploy/delete/status/routes commands

**App instance upstream transport:**

- TCP over loopback
  - `tako-server` sets `PORT=0` and `HOST=127.0.0.1`; the SDK binds to an OS-assigned port
  - The SDK signals readiness by writing the bound port to fd 4
  - `tako-server` delivers the per-instance internal auth token on the fd 3 bootstrap envelope (see below); the SDK uses it for health-probe authentication
  - Used by: tako-server to proxy HTTP requests and probe health

### Environment Variables for Apps

HTTP instances and workflow workers receive the same app/runtime environment, except HTTP-only bind vars (`PORT`, `HOST`) and per-instance CLI args.

| Name                   | Used by      | Meaning                                                                                 | Typical source                                                                                                                   |
| ---------------------- | ------------ | --------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `ENV`                  | app + worker | Active environment name                                                                 | Set by Tako in both dev and deploy (`development`, `production`, `staging`, etc.).                                               |
| `PORT`                 | app          | Listen port for HTTP server                                                             | `0` in both dev and deploy. The SDK binds to an OS-assigned port and reports it to Tako via fd 4.                                |
| `HOST`                 | app          | Listen host for HTTP server                                                             | `127.0.0.1` in both dev and deploy.                                                                                              |
| `TAKO_APP_NAME`        | app + worker | App identity used by the SDK to tag internal-socket RPCs                                | Set by both spawners (tako-server and tako-dev-server) from the deployed app name.                                               |
| `TAKO_INTERNAL_SOCKET` | app + worker | Path to the shared internal unix socket for workflow enqueue/signal and channel publish | Set by both spawners. Together with `TAKO_APP_NAME` this must always be set as a pair; the SDK asserts this at boot.             |
| `TAKO_DATA_DIR`        | app + worker | Persistent app-owned runtime data directory                                             | Set by Tako in both dev and deploy; points to the app's `data/app` directory.                                                    |
| `NODE_ENV`             | app + worker | Node.js convention env                                                                  | Set by runtime adapter / server (`development` or `production`).                                                                 |
| `BUN_ENV`              | app + worker | Bun convention env                                                                      | Set by runtime adapter (`development` or `production`).                                                                          |
| `TAKO_BUILD`           | app + worker | Deployed build/version identifier                                                       | Written into release `app.json` by `tako deploy`; `tako-server` reads it from the manifest and passes it as an env var at spawn. |
| _user-defined_         | app + worker | User config vars                                                                        | From `app.json` in the release dir. Secrets + internal token passed via fd 3 bootstrap envelope, not env vars.                   |

**Instance identity (CLI args, not env vars):** `tako-server` passes per-instance identity to the SDK entrypoint as command-line arguments:

- `--instance <id>` — 8-character nanoid instance identifier

The SDK parses this from `process.argv` (JS) or `os.Args` (Go) at startup and exposes it through the internal status endpoint and health-check responses. Build/version identity comes from `TAKO_BUILD`.

**Bootstrap envelope (fd 3):** `tako-server` always opens a pipe on fd 3 of every spawned instance. The pipe carries one JSON object:

```json
{
  "token": "<per-instance internal auth token>",
  "secrets": { "KEY": "value", ... }
}
```

The SDK reads fd 3 once at startup and closes it. The envelope travels on a pipe (rather than env vars or argv) so neither the token nor secrets inherit into subprocesses the app spawns. The token authenticates `Host: tako.internal` requests (health probes, channel auth callbacks). Secrets populate the `secrets` export from the generated `tako.gen.ts`. The pipe is always present — in dev mode with no secrets, the envelope is `{"token": "...", "secrets": {}}`.

### Messages (JSON over Unix Socket)

**CLI → tako-server (management commands):**

- `hello` (capabilities / protocol negotiation; CLI sends this before other commands):

```json
{ "command": "hello", "protocol_version": 0 }
```

Response:

```json
{
  "status": "ok",
  "data": {
    "protocol_version": 0,
    "server_version": "0.1.0",
    "capabilities": [
      "on_demand_cold_start",
      "idle_scale_to_zero",
      "scale",
      "upgrade_mode_control",
      "server_runtime_info",
      "release_history",
      "rollback"
    ]
  }
}
```

- `server_info` (returns runtime config + upgrade mode):

```json
{ "command": "server_info" }
```

- `enter_upgrading` / `exit_upgrading` (durable single-owner lock transitions):

```json
{ "command": "enter_upgrading", "owner": "upgrade-prod-..." }
```

```json
{ "command": "exit_upgrading", "owner": "upgrade-prod-..." }
```

- `prepare_release` (download runtime and install production dependencies for a release; called before `deploy` so that the deploy step only does app registration and instance startup):

```json
{
  "command": "prepare_release",
  "app": "my-app/production",
  "path": "/opt/tako/apps/my-app/production/releases/1.0.0"
}
```

- `deploy` (includes route patterns and optional secrets payload; env vars are read from `app.json` in the release dir). When `secrets` is omitted or `null`, the server keeps existing secrets for the app:

```json
{
  "command": "deploy",
  "app": "my-app/production",
  "version": "1.0.0",
  "path": "/opt/tako/apps/my-app/production/releases/1.0.0",
  "routes": ["api.example.com", "*.example.com/admin/*"],
  "secrets": {
    "DATABASE_URL": "...",
    "API_KEY": "..."
  }
}
```

- `scale` (updates the desired instance count for an app on one server):

```json
{ "command": "scale", "app": "my-app/production", "instances": 3 }
```

- `get_secrets_hash` (returns the SHA-256 hash of an app's current secrets; used by deploy to skip sending secrets when unchanged):

```json
{ "command": "get_secrets_hash", "app": "my-app/production" }
```

- `run_release` (run a one-shot release command on the leader server before rolling update):

```json
{
  "command": "run_release",
  "app": "my-app/production",
  "version": "abc1234",
  "path": "/opt/tako/apps/my-app/production/releases/abc1234",
  "command_line": "bun run db:migrate",
  "vars": {},
  "secrets": {}
}
```

The server validates the app name and release version, acquires the per-app deploy lock, derives env from release `app.json`, injects `TAKO_BUILD`, `TAKO_DATA_DIR`, stored server-side secrets, and parent `PATH`, then runs `sh -c "<command_line>"` in the release directory. The `vars` and `secrets` fields are present in the v0 wire shape but are not the source of truth for execution env. Success returns exit metadata; non-zero exit or timeout returns an error response with a stderr tail.

Server-side validation on `deploy` and app-scoped commands:

- `app` is the deployment id used on the server. CLI app-scoped commands send `{app}/{env}`. Each segment must be normalized (`[a-z][a-z0-9-]{0,62}` with no trailing `-`).
- `version` must be a simple release id (letters/digits/`.-_`, no path separators).
- `path` must resolve under `<data-dir>/apps/<app>/releases/`.

- `routes` (returns app → routes mapping used for conflict detection/debugging):

```json
{ "command": "routes" }
```

- `list_releases` (returns release/build history for an app):

```json
{ "command": "list_releases", "app": "my-app" }
```

- `rollback` (roll back an app to a previous release/build id):

```json
{ "command": "rollback", "app": "my-app", "version": "abc1234" }
```

- `stop` (stop a running app):

```json
{ "command": "stop", "app": "my-app/production" }
```

- `status` (get status of a specific app):

```json
{ "command": "status", "app": "my-app/production" }
```

- `list` (list all deployed apps with their status):

```json
{ "command": "list" }
```

- `delete` (remove app state/routes):

```json
{ "command": "delete", "app": "my-app" }
```

- `update_secrets` (update secrets for a deployed app; refreshes workflow workers and triggers rolling restart):

```json
{ "command": "update_secrets", "app": "my-app/production", "secrets": { "KEY": "value" } }
```

**Instance communication model:**

- App processes do not connect to the management socket.
- `tako-server` controls lifecycle directly (spawn/stop/rolling update). Startup readiness is signaled by the SDK via fd 4; ongoing health is verified via active HTTP probing.
- App processes receive `PORT=0` and `HOST=127.0.0.1`, bind to an OS-assigned loopback port, and write the actual port to fd 4. The server then routes traffic and health probes to that endpoint.
- Secrets are passed to instances via fd 3 (file descriptor 3) at spawn time. The server creates a pipe, writes JSON-serialized secrets to the write end, and the child process reads fd 3 at startup before any user code runs. EBADF on fd 3 means the process is not running under Tako (dev mode).
- Secret updates (`update_secrets` command) store new secrets in SQLite, drain/restart any workflow worker for the app, and trigger a rolling restart for HTTP instances; fresh processes receive updated secrets via fd 3.

### Health Checks

Active HTTP probing is the source of truth for instance health:

- **Probe interval**: 1 second steady-state, dropped to 100 ms while any instance is still in startup (Starting/Ready, not yet Healthy). The fast startup tier collapses cold-start probe slack from up to 1 s to ~100 ms without paying high-frequency probes at steady state.
- **Probe endpoint**: App's configured health check path (default: `/status`) with `Host: tako.internal`
- **Transport**: Probes use the instance's private TCP endpoint.
- **Process exit fast path**: Before each probe, `try_wait()` checks if the process has exited. If so, the instance is immediately marked dead without waiting for the probe timeout.
- **Failure threshold**: 1 failure → mark dead, trigger replacement. After the first successful probe confirms the app is healthy, any single probe failure means the instance cannot satisfy the runtime health contract.
- **Recovery**: Single successful probe resets failure count and restores to healthy

#### Internal Probe Contract

Tako-server performs health checks against the deployed app process:

```
GET /status
Host: tako.internal
X-Tako-Internal-Token: <instance-token>
```

Expected response:

```json
{
  "status": "healthy",
  "app": "dashboard",
  "version": "abc1234",
  "instance_id": "a1b2c3d4",
  "pid": 12345,
  "uptime_seconds": 3600
}
```

The SDK wrappers implement this endpoint automatically. The edge proxy does not reserve or bypass `Host: tako.internal` routes.
The expected response includes the same `X-Tako-Internal-Token` header value. The SDK wrappers enforce and echo this token automatically.

### Prometheus Metrics

Tako-server exposes a Prometheus-compatible metrics endpoint for observability.

**Endpoint:** `http://127.0.0.1:9898/` (localhost only, not publicly accessible)

**CLI flag:** `--metrics-port <port>` (default: 9898, set to 0 to disable)

**Exposed metrics:**

| Metric                                   | Type      | Labels                      | Description                                                                                               |
| ---------------------------------------- | --------- | --------------------------- | --------------------------------------------------------------------------------------------------------- |
| `tako_http_requests_total`               | Counter   | `server`, `app`, `status`   | Total proxied requests, grouped by status class (2xx/3xx/4xx/5xx)                                         |
| `tako_http_request_duration_seconds`     | Histogram | `server`, `app`             | End-to-end proxy request latency distribution                                                             |
| `tako_upstream_request_duration_seconds` | Histogram | `server`, `app`             | Upstream-only latency (proxy → origin → response headers); subtract from end-to-end to get proxy overhead |
| `tako_http_active_connections`           | Gauge     | `server`, `app`             | Currently active connections                                                                              |
| `tako_cold_starts_total`                 | Counter   | `server`, `app`             | Total cold starts triggered (scale-to-zero apps)                                                          |
| `tako_cold_start_duration_seconds`       | Histogram | `server`, `app`             | Cold start duration distribution (records on success and failure)                                         |
| `tako_cold_start_failures_total`         | Counter   | `server`, `app`, `reason`   | Cold start failures by reason (`spawn_failed`, `instance_dead`)                                           |
| `tako_tls_handshake_failures_total`      | Counter   | `server`, `reason`          | TLS handshake failures by reason (`no_sni`, `cert_missing`)                                               |
| `tako_instance_health`                   | Gauge     | `server`, `app`, `instance` | Instance health status (1=healthy, 0=unhealthy)                                                           |
| `tako_instances_running`                 | Gauge     | `server`, `app`             | Number of running instances                                                                               |

All metrics carry a `server` label (machine hostname) so multi-server deployments are distinguishable without scraper-side relabeling. A single scrape returns data for all deployed apps on that server.

Only proxied requests (routed to a backend) are measured for the request/upstream histograms. ACME challenges, direct static asset responses, and unmatched-host 404s are excluded. `tako_tls_handshake_failures_total` only tracks Tako-visible reasons (missing SNI, cert lookup miss); raw TLS protocol failures inside Pingora's listener are not counted.

**Usage with monitoring platforms:**

- **Self-hosted Prometheus/Grafana**: Add `127.0.0.1:9898` as a scrape target.
- **Hosted platforms (Grafana Cloud, Datadog, etc.)**: Install the platform's agent on the server, configure it to scrape `http://127.0.0.1:9898/metrics`.
- **Tailscale/WireGuard**: Expose port 9898 on the private network interface for remote scraping.

The endpoint uses Pingora's built-in Prometheus server with gzip compression.

## TLS/SSL Certificates

### SNI-Based Certificate Selection

Tako-server uses SNI (Server Name Indication) to select the appropriate certificate during TLS handshake:

1. Client connects and sends SNI hostname
2. Server looks up certificate for that hostname in CertManager
3. If exact match found, use that certificate
4. If no exact match, try wildcard fallback (e.g., `api.example.com` → `*.example.com`)
5. If still no match, serve fallback default certificate so HTTPS can complete and routing can return normal HTTP status codes (for example `404` for unknown routes/hosts)

This requires OpenSSL (not rustls) for callback support.

### Automatic Management

- ACME protocol (Let's Encrypt)
- Automatic issuance for domains in app routes
- For private/local route hostnames (`localhost`, `*.localhost`, single-label hosts, and reserved suffixes such as `*.local`, `*.test`, `*.invalid`, `*.example`, `*.home.arpa`), Tako skips ACME and generates a self-signed certificate during deploy.
- If no certificate exists yet for an SNI hostname, Tako serves a fallback self-signed default certificate so TLS handshakes still complete.
- Automatic renewal 30 days before expiry
- HTTP-01 challenge (port 80)
- Zero-downtime renewal
- DNS-01 challenges are supported for wildcard certificates via the [`lego`](https://go-acme.github.io/lego/) ACME client, which `tako-server` downloads and installs on-demand. Credentials are stored on the server at `/opt/tako/dns-credentials.env` and the provider name is persisted in `/opt/tako/config.json`. Run `tako servers setup-wildcard` to configure DNS credentials before deploying wildcard routes.

### Wildcard Certificate Handling

Routing supports wildcard hosts (e.g. `*.example.com`). For TLS:

- Wildcard certificates are issued automatically via DNS-01 challenges when a DNS provider is configured
- Wildcard certificates are used when present in cert storage
- If no DNS provider is configured when wildcard routes are deployed, deploy fails with an error directing the user to run `tako servers setup-wildcard`

### Certificate Storage

```
/opt/tako/certs/{domain}/
├── fullchain.pem      # Certificate + intermediates
└── privkey.pem        # Private key (0600 permissions)
```

### Development

Pass `--acme-staging` to `tako-server` to use Let's Encrypt staging:

- No rate limits
- Unlimited certificate issuance
- Certificates not trusted by browsers
- Perfect for development/testing

## tako.sh SDK

### JavaScript/TypeScript SDK

#### Installation

```bash
npm install tako.sh
```

#### Interface

Apps export a Web Standard fetch handler:

```typescript
export default function fetch(request: Request): Response | Promise<Response> {
  return new Response("Hello!");
}
```

### Runtime context (`tako.gen.ts`)

Tako v0 does not install any global. `tako typegen` emits a project-local `tako.gen.ts` file (placed inside `src/`/`app/` when those dirs exist, otherwise at the project root) that exports typed runtime state and a typed secrets bag. App code imports what it needs:

```typescript
import { env, isDev, port, dataDir, build, logger, secrets } from "../tako.gen";

logger.info("boot", { env, build });
const dbUrl = secrets.DATABASE_URL;
```

| Export    | Description                                                                                                       |
| --------- | ----------------------------------------------------------------------------------------------------------------- |
| `env`     | `ENV` value (`"development"`, `"production"`, ...)                                                                |
| `isDev`   | `true` when `env === "development"`                                                                               |
| `isProd`  | `true` when `env === "production"`                                                                                |
| `port`    | Port assigned to this app instance                                                                                |
| `host`    | Host/address Tako bound this app instance to                                                                      |
| `build`   | Build identifier injected at deploy time (`"dev"` under `tako dev`)                                               |
| `dataDir` | Persistent app-owned data directory — writes survive restarts                                                     |
| `appDir`  | Directory the app is running from (equivalent to `process.cwd()`)                                                 |
| `secrets` | Typed secret bag — redacts automatically on bulk serialize                                                        |
| `logger`  | Structured JSON logger (`logger.info(...)`) bound to `source: "app"`                                              |
| `Env`     | TypeScript union of environment names declared in `tako.toml`, narrows `env === "staging"` checks at compile time |
| `Secrets` | TypeScript interface of secret keys declared in `.tako/secrets.json`                                              |

`secrets` redacts automatically on `JSON.stringify`, `console.log`, and `toString` (returns `"[REDACTED]"`); individual key access (`secrets.MY_KEY`) returns the value. The `Secrets` interface is regenerated from `.tako/secrets.json` on every `tako dev`, `tako deploy`, `tako typegen`, and `tako secret` change.

Channels and workflows are not on the runtime context — they are regular ES modules you import from their files:

```typescript
import sendEmail from "../workflows/send-email";
import chat from "../channels/chat";

await sendEmail.enqueue({ to: "u@e.co" });
await chat({ roomId: "r1" }).publish({ type: "msg", data: { text: "hi" } });
```

The `tako.sh` package exports `defineChannel`, `defineWorkflow`, `signal`, `TakoError`, and `InferWorkflowPayload`. Server-only plumbing (`loadSecrets`, `createLogger`, `handleTakoEndpoint`, `initServerRuntime`, and the channel/workflow definition types) lives under `tako.sh/internal` and is intended for generated files (`tako.gen.ts`) and framework adapters. The `Channel` class is not exported from `tako.sh`: server code uses the accessor returned by `defineChannel(...).$messageTypes<M>()` (imported from your `channels/` file); browser code imports from `tako.sh/client` (or uses the `useChannel` hook from `tako.sh/react`). There is no `Tako` global.

### Go SDK

#### Installation

```bash
go get tako.sh
```

#### Interface

Go apps use the `tako` package to serve an `http.Handler`:

```go
package main

import (
    "net/http"
    "tako.sh"
)

func main() {
    mux := http.NewServeMux()
    mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
        w.Write([]byte("Hello from Tako!"))
    })
    tako.ListenAndServe(mux)
}
```

For frameworks that manage their own server (e.g. Fiber on fasthttp), use `tako.Listener()` to get a pre-bound `net.Listener` instead.

#### Exports

| Export                                                                                              | Purpose                                                                                              |
| --------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `tako.ListenAndServe(handler)`                                                                      | Wraps an `http.Handler` with Tako protocol support (fd 4 readiness, `Host: tako.internal` handling). |
| `tako.Listener()`                                                                                   | Returns a bound `net.Listener` for frameworks that own their own server loop.                        |
| `tako.InstanceID()` / `tako.Version()` / `tako.Uptime()`                                            | Runtime identity helpers (empty strings in dev mode).                                                |
| `tako.GetSecret(name)`                                                                              | Low-level secret accessor. Prefer the typed `Secrets` struct from `tako typegen`.                    |
| `tako.AllowChannel(grant)` / `tako.RejectChannel()`                                                 | Channel auth helpers for `ChannelDefinition` callbacks.                                              |
| `tako.Channel`, `tako.ChannelRegistry`, `tako.Channels`, `tako.ChannelTransport`, and related types | Channel authoring surface mirrored from `tako.sh/internal`.                                          |

#### Key Differences from JS SDK

- Go compiles to a native binary — no runtime download needed on the server.
- The compiled binary runs directly (`launch_args: ["{main}"]`), no SDK entrypoint wrapper. The `tako` package wires up the protocol from inside the user's own binary.
- `tako.ListenAndServe()` handles the full protocol: CLI arg parsing (`--instance`), TCP serving, `Host: tako.internal` endpoint interception, graceful shutdown on `SIGTERM`/`SIGINT` with a 10s drain window.
- Deploy auto-injects `GOOS=linux` and `GOARCH` for cross-compilation to the target server.
- Default build: `CGO_ENABLED=0 go build -o app .` producing a static binary.
- Secrets: `tako.GetSecret("name")` provides access to Tako-managed secrets. Run `tako typegen` to generate a typed `Secrets` struct in `tako_secrets.go`.

### Vite Plugin

```typescript
import { tako } from "tako.sh/vite";
```

- `tako.sh/vite` provides a plugin that prepares a deploy entry wrapper in Vite output.
- It emits `<outDir>/tako-entry.mjs`, which normalizes the compiled server module to a default-exported fetch handler.
- During `vite dev`, it adds `.test`, `.tako.test`, and configured dev route hostnames to `server.allowedHosts`.
- During `vite dev`, when `PORT` is set, it binds Vite to `127.0.0.1:$PORT` with `strictPort: true`.
- During `tako dev`, it routes Vite-process `console.*`, stdout, and stderr through structured Tako app log events so multi-line framework/runtime errors stay grouped in the CLI log stream.
- Deploy does not read Vite metadata files.
- To use the generated wrapper as deploy entry, set `main` in `tako.toml` to the generated file (for example `dist/server/tako-entry.mjs`) or define preset top-level `main`.

### Next.js Adapter

```typescript
import { withTako } from "tako.sh/nextjs";
```

- `tako.sh/nextjs` provides `withTako()`, a helper that sets `output = "standalone"`, points `adapterPath` at the installed Tako adapter, and appends `"*.test"` and `"*.tako.test"` to `allowedDevOrigins` so `next dev` accepts requests from Tako's dev hostnames.
- On build, the adapter writes `.next/tako-entry.mjs`.
- If Next emits `.next/standalone/server.js`, the adapter copies `public/` and `.next/static/` into `.next/standalone/` so that standalone server can serve them.
- If standalone output is not emitted, the generated wrapper falls back to `next start` against the built `.next/` directory and installed `next` package.
- The generated `tako-entry.mjs` exports a fetch handler that proxies requests to the standalone `server.js`.

### Feature Overview

- Internal fetch handler adapters for Bun/Node runtimes (used by entrypoint binaries)
- Go SDK with `tako.ListenAndServe()` for native http.Handler support
- Deployed app serving over private TCP with `PORT`/`HOST`; `tako dev` also uses TCP (`PORT`)
- Internal status endpoint (`Host: tako.internal` + `/status`)
- Internal channel auth endpoint (`Host: tako.internal` + `POST /channels/authorize`)
- Public durable channel read/connect route at `GET /channels/<name>`
- Graceful shutdown handling

### Built-in Endpoints

**`GET /status` with `Host: tako.internal`**

```json
{
  "status": "healthy",
  "app": "dashboard",
  "version": "abc1234",
  "instance_id": "a1b2c3d4",
  "pid": 12345,
  "uptime_seconds": 3600
}
```

Used for health checks during rolling updates and monitoring.

**`POST /channels/authorize` with `Host: tako.internal`**

Used by `tako-server` to ask the app SDK whether a channel operation is allowed and which lifecycle settings apply. The SDK returns `ok`, optional `subject`, optional `transport`, and channel lifecycle settings such as `replayWindowMs`, `inactivityTtlMs`, `keepaliveIntervalMs`, and `maxConnectionLifetimeMs`.

### Channels

Channels are Tako-owned durable pub-sub streams on public app routes:

- `GET /channels/<name>` with `Accept: text/event-stream` serves SSE with replay + live tail
- `GET /channels/<name>` with `Upgrade: websocket` upgrades to WebSocket

Channels keep a bounded replay window so reconnecting clients can resume across disconnects and `tako-server` reloads. They are not a permanent history API.

- SSE resumes from `Last-Event-ID`
- WebSocket resumes from `last_message_id` in the query string
- If no cursor is provided, Tako starts from the latest retained message
- If the requested cursor is older than the retained replay window, Tako returns `410 Gone`

Browser clients keep reconnecting until explicitly closed. Network loss, laptop sleep, server restarts, and clean stream rotation are treated as transient disconnects: the SDK retries with bounded exponential backoff and jitter, wakes early when the browser reports it is back online, and resumes from the last received message id.

Channel WebSocket transport uses JSON text frames:

- server-to-client text frames are serialized `ChannelMessage` objects
- client-to-server text frames are parsed as `ChannelPublishPayload` objects, routed through the channel's declared `handler`, and the handler's return value is fanned out to subscribers

Channel routes are exact and flat: `defineChannel({ name: "chat", ... })` is served at `/channels/chat`. Dynamic values are query params validated against the channel's declared JSON Schema, for example `/channels/chat?roomId=room-123`.

### Authoring channels

**JS/TypeScript** — file-based discovery: drop a file into `channels/*.ts` with a default export of `defineChannel({ name: "<name>", ... }).$messageTypes<M>()`. The `name` property is the wire channel name and is the source of truth for the public route. Generated/scaffolded files use the file stem as the initial name, but users may choose a different explicit name; discovery rejects duplicate declared names. `paramsSchema` is a TypeBox schema that becomes both the TypeScript params type and the server-side JSON Schema used by `tako-server` before app auth. `.$messageTypes<M>()` is a type-level narrower that declares the message map; at runtime it returns the same export.

```ts
// channels/chat.ts
import { defineChannel } from "tako.sh";

type ChatMessages = {
  msg: { text: string; userId: string };
  typing: { userId: string };
};

export default defineChannel({
  name: "chat",
  paramsSchema: (t) => t.Object({ roomId: t.String({ minLength: 1 }) }),
  auth: {
    headerName: "authorization",
    async verify(input) {
      const session = await readSession(input.header);
      if (!session) return false;
      const allowed = await db.isMember(input.params.roomId, session.userId);
      return allowed ? { subject: session.userId } : false;
    },
  },
  handler: {
    msg: async (data, ctx) => {
      await db.saveMessage(ctx.params.roomId, data);
      return data; // what gets fanned out to subscribers
    },
    typing: async (data) => data,
  },
}).$messageTypes<ChatMessages>();
```

- **`paramsSchema`** — optional TypeBox schema. Omit it for channels with no params. The serialized JSON Schema is sent to `tako-server`, which rejects invalid query params before round-tripping to the app.
- **`auth`** — optional. Omit or set `false` for public channels. Auth is declarative: `{ headerName, cookieName, verify }`. `headerName` defaults to `authorization`; set it to `false` for cookie-only auth. `verify(input)` receives `{ header?, cookie?, params, channel, operation }` and returns `false`, `true`, or `{ subject }`.
- **`handler`** — optional map keyed by message type. Presence of `handler` makes the channel a **WebSocket** channel (bidirectional); absence makes it **SSE** (broadcast-only). Each handler returns the data to broadcast, or `void` / `undefined` to drop the message.
- Lifecycle fields (`replayWindowMs`, `inactivityTtlMs`, `keepaliveIntervalMs`, `maxConnectionLifetimeMs`) — unchanged from before.

**Transport inference:**

- `handler` present → **WS**. Clients can send over the socket; each frame routes through the declared handler; the return value fans out to subscribers. Handler errors or `void` returns drop the message. Types not in the handler map pass through without server processing.
- `handler` absent → **SSE**. Broadcast-only. Server publishes via the imported channel module (`await missionLog({ base }).publish(...)`); clients only receive.

WebSocket header auth is sent as the first text frame:

```json
{ "type": "tako.auth", "token": "Bearer ...", "lastMessageId": "123" }
```

If an auth-required WebSocket does not send a valid first frame within five seconds, `tako-server` closes it with an errkit-generated app close code.

**Go** — programmatic registration mirrors the same wire protocol:

```go
tako.Channels.Register("chat", tako.ChannelDefinition{
  ParamsSchema: []byte(`{"type":"object","properties":{"roomId":{"type":"string"}},"required":["roomId"]}`),
  Auth: &tako.ChannelAuthScheme{HeaderName: "authorization"},
  Verify: func(input tako.VerifyInput) tako.ChannelAuthDecision {
    if input.Header == nil || input.Header.Scheme != "Bearer" {
      return tako.RejectChannel()
    }
    return tako.AllowChannel(tako.ChannelGrant{Subject: "user-123"})
  },
})
```

### Publishing from the server

Server-side code (HTTP handlers, workflow bodies) imports a channel module directly and calls it. Unparameterized channels expose `publish` / `subscribe` / `connect` on the export; parameterized ones are callable with their params, returning the same handle:

```ts
// channels/status.ts (unparameterized)
import status from "../channels/status";
await status.publish({ type: "ping", data: { at: Date.now() } });

// channels/mission-log.ts (parameterized by paramsSchema)
import missionLog from "../channels/mission-log";
await missionLog({ base }).publish({ type: "event", data: event });
```

Params are URL-encoded automatically. `publish` payloads are type-checked against the message map declared via `.$messageTypes<M>()`. The `Channel` class is not re-exported from `tako.sh` — browser code imports from `tako.sh/client` (or uses the `useChannel` hook from `tako.sh/react`).

## Workflows (Durable Runs)

Tako's workflow engine runs durable background work alongside an app's HTTP
instances — retries with exponential backoff, delayed/cron schedules, and
multi-step workflows whose progress survives process restarts via `step.run`
checkpoints. It positions Tako for the "backend of your backend" use case
(image processing, email, reindexing, LLM calls) without requiring a separate
queue service.

**Vocabulary:**

- **workflow** — a named handler (the file in `workflows/*.ts`, or a
  registered handler in the Go worker binary).
- **run** — one execution of a workflow (the row in the queue that gets
  claimed, retried, completed, or moved to dead).
- **step** — a memoized portion inside a run via `step.run(name, fn)`.

### Architecture

- **Queue file**: `{tako_data_dir}/apps/<app>/runs.db` — per-app SQLite with WAL. tako-server is the only process that reads/writes; SDKs reach it exclusively via the per-app unix socket.
- **Tables**:
  - `runs` — one row per run (status, attempts, lease, payload).
  - `steps` — one row per completed step `(run_id, name, result)`. First-write-wins via `INSERT OR IGNORE` so duplicate saves after a retried RPC don't overwrite.
  - `event_waiters` — runs parked on `step.waitFor`, indexed by `event_name` for fast lookup on `signal`.
  - `schedules`, `leader_leases` — cron infrastructure.
- **tako-server (Rust)** — owns the DB, exposes the per-app unix socket, runs the cron ticker, and supervises the worker subprocess. The ticker also calls `reclaim_expired()` every second: any run stuck in `status='running'` past its `lease_until` is moved back to `pending` and the supervisor is woken so a fresh worker picks it up. This is how runs recover from a worker that died mid-execution (SIGKILL, OOM, host crash, server-level restart without graceful drain).
- **Worker process (JS or Go)** — loads user code, claims runs, executes handlers. Separate from HTTP instances so heavy workflow deps don't bloat the request-serving process.
- **SDK** — each workflow module's default export provides `.enqueue(payload, opts?)`; `signal(event, payload?)` is a top-level export from `tako.sh` that throws `TakoError("TAKO_UNAVAILABLE")` when called outside an installed workflow runtime. Workers use the same RPC client for claim/heartbeat/save/complete/cancel/fail/defer/wait. **No SQLite in any SDK.**

### Configuration (tako.toml)

```toml
[workflows]                   # base config inherited by every worker group
workers = 1
concurrency = 10

[workflows.email]             # named worker-group override
workers = 2

[servers.lax.workflows]       # base override on one server
workers = 2

[servers.lax.workflows.email] # named worker-group override on one server
workers = 4
```

Fields:

- **`workers`** — number of always-on worker processes. `0` = scale-to-zero: tako-server spawns the worker on the first enqueue or cron tick, and the worker exits after it has been idle (no claimed runs) long enough for the supervisor's idle window. Default `0`.
- **`concurrency`** — max parallel runs per worker. Default `10`.

Precedence for unnamed workflows: built-in defaults (`workers = 0`, `concurrency = 10`) < `[workflows]` < `[servers.<name>.workflows]`.

Precedence for `worker: "email"`: built-in defaults < `[workflows]` < `[workflows.email]` < `[servers.<name>.workflows]` < `[servers.<name>.workflows.email]`. A top-level `workers = 5` under `[workflows]` is inherited by each worker group unless that group overrides it.

If a JS app has a `workflows/` directory (or a Go app declares a worker binary) but no workflow config anywhere, the app is implicitly scale-to-zero on every server in the env.

### Authoring workflows

**JS/TypeScript** — file-based discovery: drop a file into `workflows/<name>.ts` with a default export from `defineWorkflow<P>(name, opts)`. The `opts.handler` function's second argument is the step context (`step`) — call `step.run`/`step.sleep`/`step.waitFor`/`step.bail`/`step.fail` as needed, and read `step.runId` / `step.workflowName` / `step.attempt` (the 1-indexed run attempt, bumped on each run-level retry) for context:

```ts
// workflows/send-email.ts
import { defineWorkflow } from "tako.sh";

type SendEmailPayload = { userId: string };

export default defineWorkflow<SendEmailPayload>("send-email", {
  retries: 4,
  schedule: "0 9 * * *",
  handler: async (payload, step) => {
    const user = await step.run("fetch-user", () => db.users.find(payload.userId));
    await step.run("send", () => mailer.send(user.email));
  },
}); // 9am daily
```

The `name` is required (it must be a string literal — codegen and the dedup/cron systems read it) and should match the filename for the file-based discovery scan.

Set `worker: "name"` in the workflow opts to assign a workflow to a named worker group; workflows without `worker` belong to the `default` group. Worker processes launched with `TAKO_WORKFLOW_WORKER=<name>` load only workflows assigned to that group. Worker processes without `TAKO_WORKFLOW_WORKER` load all workflows for compatibility with the default single-worker deployment path.

**Go** — explicit registration in a separate `cmd/worker/main.go` binary. Go's separate-binary design is intentional: a single-binary design would link CGO-heavy workflow deps (image libs, ML bindings) into the HTTP server binary.

### Enqueuing

```ts
// anywhere:
import sendEmail from "../workflows/send-email";

await sendEmail.enqueue({ userId: "u1" });
await sendEmail.enqueue(payload, {
  runAt: new Date(Date.now() + 60_000),
  retries: 9,
  uniqueKey: "daily-digest:2026-04-14",
});
```

Each workflow module's default export is a typed handle: `.enqueue(payload, opts?)` is constrained to the payload type declared on `defineWorkflow<P>(name, opts)`. No typegen is needed for workflow enqueue typing — it flows from the module's own types.

`uniqueKey` deduplicates: if an existing non-terminal run has the same key, enqueue is a no-op and returns the existing run's id. Cron ticks use this internally (key = `cron:<name>:<bucket_ms>`) so catching up doesn't double-enqueue.

### Step checkpointing

`step.run(name, fn, opts?)` persists `fn`'s return value as one row in the `steps` table keyed by `(run_id, name)`. On retry, previously-completed steps return their stored value instead of re-executing.

Per-step options:

- `retries: N` — in-step retry budget (default 0). After N+1 in-step attempts the error propagates → run-level retry kicks in.
- `backoff: { base, max }` — between in-step retries.
- `retry: false` — short-circuit: any throw fails the run immediately, skipping both in-step and run-level retries.

### At-least-once contract

If the worker crashes between `fn` returning and the SaveStep RPC completing, `fn` runs again on the next claim. The window is one RPC (~1ms) but it's real. **Make step bodies idempotent**: Stripe idempotency keys, `db.users.upsert` not `create`, dedup keys on outbound webhooks. This contract matches every workflow engine in the industry — it's the cost of durability without two-phase commit.

### Durable `step.sleep`

`step.sleep(name, ms)` waits until the wake time. Short waits (< 30s) run inline; longer waits **defer the run** via `DeferRun` — the worker exits the handler, the run goes back to `pending` with `run_at = wakeAt`, the supervisor wakes the worker on schedule. Crash-safe across days.

### Events: `signal` / `waitFor`

`step.waitFor(name, { timeout })` parks the run waiting for a named event. The handler exits, the run goes to `pending` with no `run_at`, an `event_waiters` row is inserted, and the worker can release.

`signal(name, payload?)` from `tako.sh` (or the equivalent internal-socket call in Go) wakes every parked waiter with matching name. The payload is materialized as the waiter's step result and the run is set runnable. `signal` is runtime-guarded: calling it from browser code (where the workflow runtime is not installed) throws a `TakoError("TAKO_UNAVAILABLE")` instead of silently no-oping.

```ts
// Worker handler — pause until approval arrives
const decision = await step.waitFor<{ approved: boolean }>(`approval:order-${payload.id}`, {
  timeout: 7 * 24 * 3600 * 1000,
});
if (decision === null) step.bail("approval timed out");
```

```ts
// Anywhere else (HTTP handler, webhook receiver, another workflow)
import { signal } from "tako.sh";
await signal(`approval:order-abc`, { approved: true });
```

Routing is by event name only — embed any selectors in the name. No JSON predicates server-side.

### Early exit: `step.bail` / `step.fail`

- `step.bail(reason?)` — end the run cleanly. Status: `cancelled`. No retries.
- `step.fail(error)` — end the run with failure. Status: `dead`. No retries (skips the run-level retry budget).

Both work via sentinel exceptions caught by the worker. Useful for "this work isn't needed anymore" (bail) and "this is permanently broken, don't bother retrying" (fail).

### Run statuses

`pending | running | succeeded | cancelled | dead`. Terminal: `succeeded`, `cancelled`, `dead`.

### Retries / backoff (run level)

- Failed handlers retry with exponential backoff (default base 1s, ±20% jitter, capped at 1h). Override via `defineWorkflow(name, { handler, retries, backoff })`. Default is 2 retries (3 total attempts).
- `attempts` bumps on every claim. When attempts reach the budget, the run moves to `dead`.
- `defer_run` (sleep, waitFor) decrements attempts so parking doesn't consume retry budget.

### Drain on stop / delete

- On `tako stop <app>`: tako-server drains the worker (SIGTERM, waits for in-flight, SIGKILL after 120s).
- On `tako delete <app>`: drain first, then remove per-app data — in-flight runs get a chance to finish before the DB goes away.

### Communication model

- Single shared internal socket at `{tako_data_dir}/internal.sock` (symlink → `internal-{pid}.sock`, atomically swapped during upgrades for zero-downtime handoff — same pattern as the mgmt socket). Workflow RPCs and server-side channel publishes both land here, hence the role-neutral name.
- Every command carries an `app` field so one socket routes for every deployed app.
- Auth: filesystem permissions only (`chmod 0600`, owned by the service user).
- SDKs read `TAKO_INTERNAL_SOCKET` and `TAKO_APP_NAME` env vars. HTTP instance and workflow worker spawners (tako-server in production and tako-dev-server in `tako dev`) share one env contract defined in `tako-core::instance_env::TakoRuntimeEnv` so the dev and prod runtimes can't drift. The SDK asserts the pair is set together at import time — a half-set env (one var without the other) is a platform bug and crashes the process on boot rather than silently failing at the first workflow enqueue or channel publish.
- From any process: `EnqueueRun`, `Signal`, `ChannelPublish` (server-side publish goes straight to the channel store instead of round-tripping through the HTTPS proxy).
- From worker processes: `ClaimRun`, `HeartbeatRun`, `SaveStep`, `CompleteRun`, `CancelRun`, `FailRun`, `DeferRun`, `WaitForEvent`, `RegisterSchedules`.
- The management socket rejects workflow/channel commands with an explicit "must be sent over the internal socket" error, and vice versa — the two sockets never cross wires even though they share the `Command` enum in `tako-core`.
- JSONL protocol, per-call connection (connect → send → read → close).
- Server → Worker for drain: SIGTERM + grace period (120s), then SIGKILL.

### Dev mode

`tako dev` uses the same workflow architecture as production: tako-dev-server owns the runs DB, enqueue socket, and a `WorkerSupervisor` that spawns a worker subprocess on demand. The worker is **scale-to-zero** (`workers: 0`, `idle_timeout_ms: 3_000`) so it only runs while there's real work, and every wake re-spawns it fresh — so code edits take effect on the next enqueue without restarting `tako dev`. Worker stdout/stderr is tee'd into the same log stream as the app process, with `scope: "worker"` so the CLI can prefix it.

On every `RegisterApp`, the dev-server registers the app with its embedded `WorkflowManager` — same `ensure()` call as production — so the first workflow enqueue or channel publish from user code doesn't race the registration.

**Fail-fast on broken workers.** If the worker subprocess exits non-zero without claiming any run (typical for import errors, missing workflow module, crash on boot), the supervisor marks the app unhealthy for 5s. During that window, `EnqueueRun` returns `worker unhealthy: <reason>` instead of silently queuing work that will never execute. The SDK surfaces this to the caller as a normal error so broken dev code is loud instead of hanging. Clean idle-out (exit 0 after `idle_timeout_ms`) never marks unhealthy.

## Edge Cases & Error Handling

| Scenario                             | Behavior                                                                   |
| ------------------------------------ | -------------------------------------------------------------------------- |
| Config/data directory deleted        | Auto-recreate on next command                                              |
| `config.toml` corrupted              | Show parse error with line number, offer to recreate                       |
| `tako.toml` deleted                  | Commands that require project config fail with guidance to run `tako init` |
| `.tako/` deleted                     | Auto-recreate on next deploy                                               |
| `.tako/secrets.json` deleted         | Warn user, prompt to restore secrets                                       |
| Low free space under `/opt/tako`     | Deploy fails before upload with required vs available disk sizes           |
| Concurrent deploy already running    | Later deploy fails immediately with a retry message                        |
| `tako-server` restarts during deploy | In-flight deploy fails; retry does not require lock cleanup                |
| Deploy fails mid-transfer/setup      | Auto-clean newly-created partial release directory                         |
| Health check fails                   | Automatic rollback to previous version                                     |
| Network interruption during deploy   | Partial failure handling, can retry                                        |
| Process crash                        | Auto-restart, health checks detect and handle                              |

## Testing Requirements

- Unit tests for all business logic (config parsing, validation, routing)
- Integration tests for critical paths (deploy, rolling updates, health checks)
- Edge case tests (deleted files, network failures, process crashes)
- Critical-path coverage target: >=80% line coverage across core modules (config parsing, runtime detection, routing, static file resolution, cold-start orchestration)
- TDD mandatory: write tests first, implement after tests pass

## Performance Targets

- Proxy throughput: Faster than Caddy, on par with Nginx
- Cold start: ~100-500ms for on-demand instances
- Health detection: <3s for failed instance detection
- Deploy time: <1 minute for rolling update of 3 instances
- Memory: Minimal footprint with on-demand scaling
