# tako

Rust crate for the `tako` CLI, `tako-dev-server`, and `tako-dev-proxy` local binaries.

## Responsibilities

- Project initialization (`tako init`).
- Local development flow (`tako dev`, `tako doctor`).
- Local development daemon runtime (`tako-dev-server`).
- macOS dev proxy for loopback-only local ingress (`tako-dev-proxy`).
- Deployment orchestration (`tako deploy`).
- Release history and rollback (`tako releases list`, `tako releases rollback`).
- Remote operational commands (`logs`, `backups`, `delete`, `servers`, `secrets`).
- Config loading/validation, runtime detection, and SSH interactions.

## Command Surface

Primary subcommands:

- `init`
- `logs`
- `dev`
- `doctor`
- `status`
- `run`
- `credentials`
- `completions`
- `version`
- `servers`
- `secrets`
- `storages`
- `backups`
- `releases`
- `upgrade`
- `deploy`
- `delete`
- `scale`
- `generate`
- `uninstall`

Use `cargo run -p tako-cli --bin tako -- --help` for current flags and subcommand help.

Operational behavior highlights:

- `tako upgrade` upgrades only the local CLI. On macOS it preserves the signed `Tako.app` + `tako` symlink layout used by the hosted installer. Every build is a rolling `latest` build while Tako's protocol is v0.
- `tako status` prints one global snapshot and exits.
- `tako servers upgrade <name>` verifies the signed `tako-server-sha256s.txt` release manifest and passes the archive name and SHA-256 to a root-owned helper. The helper downloads only official releases and verifies the archive before installing it. Upgrade mode, service reload, readiness checks, and rollback keep the previous binary available until the replacement is ready. Service-user upgrades reject `TAKO_DOWNLOAD_BASE_URL`; install custom builds as administrator. GitHub-backed CLI update checks use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.
- Installer-managed hosts configure scoped passwordless sudo helpers for the `tako` SSH user, so upgrade/reload maintenance flows run non-interactively by default.
- Status output shows separate lines for concurrently running builds of the same app.
- App heading lines show `app (environment) state`; build/version is shown on the nested `build:` line.
- `tako deploy` packages source files from the app's source root (git root when available; otherwise app directory), filtered by `.gitignore`.
- `tako deploy` always excludes `.git/`, `.tako/`, `.env*`, `node_modules/`, and `target/` from source bundles.
- `tako deploy` resolves preset from top-level `preset` when set, otherwise falls back to adapter base preset from top-level `runtime` (when set) or adapter detection. Native artifact deploys can set `start` and skip runtime/preset resolution. Unpinned official aliases are refreshed from `master` on deploy and fall back to cached content if refresh fails. `tako dev` prefers cached or embedded preset data and only fetches when nothing local is available.
- For JS runtimes, `tako dev` and deploy build stage 1 use the runtime lane's script runner by default (`bun run dev/build`, `npm run dev/build`), so external tools like Vite+ can live behind those scripts.
- `tako deploy` builds native artifacts locally before upload. Set top-level `start = ["./app"]` to run a prebuilt native artifact without server-side runtime installation. When top-level `container = "Dockerfile"` is set, deploy packages source and the server builds/runs the image with Podman.
- Non-dry-run `tako deploy` acquires a project-local `.tako/deploy.lock` and fails fast if another local deploy is already running for the same project.
- On macOS, `tako dev` uses a dedicated `127.77.0.1` loopback alias plus a launchd-managed dev proxy (`tako-dev-proxy`) so `https://{app}.test/` works on default ports without binding the main network interfaces.
- `tako dev --tunnel` starts with a temporary public tunnel URL, and pressing `t` in the interactive dev UI toggles it. The tunnel URL is shown in `tako dev list` while active.
- Container release builds use the app directory as the Podman build context. The configured container file and `.dockerignore` own production build inputs.
- `tako deploy` caches target artifacts in `.tako/artifacts` and reuses verified cache hits when build inputs are unchanged; invalid cache entries are rebuilt automatically.
- Local runtime version resolution runs `<tool> --version` directly, falling back to `latest`.
- `tako deploy` merges build assets (preset assets + top-level `assets`) into app `public/` after target build, in listed order.
- `tako deploy` writes `app.json` in the deployed app directory and `tako-server` uses it to resolve the runtime start command.
- `tako releases list` shows release/build history for the current app and environment with commit metadata when available.
- `tako releases rollback <release-id>` rolls target servers back to a previous release id using the normal rolling-update path.
- `tako backups now/list/status/download/restore` manages encrypted private app data backups configured with `[envs.<env>].backup`; backup keys live encrypted in `.tako/secrets.json`.
- `tako servers add` expects a Tailscale MagicDNS name or Tailscale IP, checks the management endpoint and `tako@host` SSH access, enrolls the authenticated SSH key for signed remote management, verifies private management HTTP, then stores detected target metadata (`arch`, `libc`) in each `[[servers]]` entry in `~/.tako/config.toml`. If the server is running but inaccessible, interactive setup offers to repair access; if its state is unknown, it offers to install or repair it. After confirmation, Tako asks for an administrator with `root` pre-filled. Use `--install` to skip the confirmation or `admin@host` to select an administrator without a prompt. Encrypted local SSH keys prompt interactively; pass `--ssh-passphrase` for one-line commands.
- `tako deploy` requires valid target metadata for each selected server and does not probe targets during deploy.
- Production environments use Let’s Encrypt certificates by default. Run `tako credentials set ssl.cloudflare --env <env>` for wildcard routes that need Cloudflare DNS-01, or set `ssl = "cloudflare"` and store the same credential to use Cloudflare Origin CA certificates. Wildcard DNS-01 needs a Cloudflare user or account API token with Zone Read and DNS Write for the matching zone, and any token IP restriction must include each target server's egress IP.
- New apps start with one desired instance. Scaling to zero enables on-demand cold starts after the warm instance becomes idle.
- Official CLI installs send an anonymous event per command (version, OS, arch, command name) so we can count unique users and see which commands run. Set `TAKO_TELEMETRY=0` to opt out. CI, `--ci`, and local Cargo builds are off unless `TAKO_TELEMETRY=1`. `tako-server` does not send usage stats.

## Run and Test

From repository root:

```bash
cargo run -p tako-cli --bin tako -- --help
cargo run -p tako-cli --bin tako-dev-server -- --help
cargo run -p tako-cli --bin tako-dev-proxy -- --help
cargo test -p tako-cli
```

Run a focused command from source:

```bash
cargo run -p tako-cli --bin tako -- deploy --help
```

## Config Requirements

- `tako.toml` is required for `dev`, `deploy`, `logs`, and `secrets` workflows.
- App-scoped commands default to `./tako.toml`; `-c/--config CONFIG` selects another config file and uses its parent directory as project context. Omitting the `.toml` suffix is supported and recommended for brevity.
- Top-level `name` in `tako.toml` is optional; when omitted, app identity falls back to sanitized project directory name.
- Setting `name` explicitly is recommended for stable identity and uniqueness per server; renaming identity later creates a new app path and requires manual cleanup of old deployments.
- Non-development environments must define `route` or `routes`; development defaults to `{app}.test`.
- `[envs.<name>].ssl` is optional and defaults to `letsencrypt`; Cloudflare SSL and Let’s Encrypt wildcard routes require encrypted credentials from `tako credentials set ssl.cloudflare`. Deploy checks required Cloudflare credentials from each target server during remote prepare.
- Environments with `<app_root>/channels/` or `<app_root>/workflows/` can deploy to one server with local SQLite runtime state. Multi-server channels require the `postgres_url` credential. Multi-server workflows require `postgres_url` unless every JavaScript workflow sets `local: true`; Go workflow deployments always require `postgres_url`. Local workflows use per-server queues and cron.

## Related Docs

- `../website/src/pages/docs/quickstart.astro` (first-run local + remote setup)
- `website/src/pages/docs/development.md` (local dev workflow)
- `website/src/pages/docs/deployment.md` (remote deploy workflow)
