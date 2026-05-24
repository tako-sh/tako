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

Use `cargo run -p tako --bin tako -- --help` for current flags and subcommand help.

Operational behavior highlights:

- `tako upgrade` upgrades only the local CLI. On macOS it preserves the signed `Tako.app` + `tako` symlink layout used by the hosted installer. Every build is a rolling `latest` build while Tako's protocol is v0.
- `tako servers status` prints one global snapshot and exits.
- `tako servers upgrade <name>` verifies the signed `tako-server-sha256s.txt` release manifest, enforces the matching archive SHA-256 on the host, installs `/usr/local/bin/tako-server`, enters server upgrade mode, triggers service-manager reload (`systemctl reload tako-server` on systemd or `rc-service tako-server reload` on OpenRC) using root privileges (root login or sudo-capable user), waits for readiness, then exits upgrade mode. Reload uses temporary process overlap until the replacement server is ready, and Tako keeps the previous on-disk binary until then so it can restore it if readiness fails. Custom `TAKO_DOWNLOAD_BASE_URL` overrides must use `https://` unless `TAKO_ALLOW_INSECURE_DOWNLOAD_BASE=1` is set for local testing. GitHub-backed update checks and downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.
- Installer-managed hosts configure scoped passwordless sudo helpers for the `tako` SSH user, so upgrade/reload maintenance flows run non-interactively by default.
- Status output shows separate lines for concurrently running builds of the same app.
- App heading lines show `app (environment) state`; build/version is shown on the nested `build:` line.
- `tako deploy` packages source files from the app's source root (git root when available; otherwise app directory), filtered by `.gitignore`.
- `tako deploy` always excludes `.git/`, `.tako/`, `.env*`, `node_modules/`, and `target/` from source bundles.
- `tako deploy` resolves preset from top-level `preset` when set, otherwise falls back to adapter base preset from top-level `runtime` (when set) or adapter detection (`unknown` falls back to `bun`); unpinned official aliases are refreshed from `master` on deploy and fall back to cached content if refresh fails. `tako dev` prefers cached or embedded preset data and only fetches when nothing local is available.
- For JS runtimes, `tako dev` and deploy build stage 1 use the runtime lane's script runner by default (`bun run dev/build`, `npm run dev/build`), so external tools like Vite+ can live behind those scripts.
- `tako deploy` builds per-target artifacts locally before upload, using Docker only when preset `[build].container` resolves to `true`; built-in JS base presets (`bun`, `node`) default to local build mode (`container = false`) unless explicitly overridden.
- Non-dry-run `tako deploy` acquires a project-local `.tako/deploy.lock` and fails fast if another local deploy is already running for the same project.
- On macOS, `tako dev` uses a dedicated `127.77.0.1` loopback alias plus a launchd-managed dev proxy (`tako-dev-proxy`) so `https://{app}.test/` works on default ports without binding the main network interfaces.
- Container builds stay ephemeral; dependency downloads are reused via per-target Docker cache volumes keyed by target label and builder image.
- Containerized deploy builds default to `ghcr.io/lilienblum/tako-builder-musl:v1` for `*-musl` targets and `ghcr.io/lilienblum/tako-builder-glibc:v1` for `*-glibc` targets.
- `tako deploy` caches target artifacts in `.tako/artifacts` and reuses verified cache hits when build inputs are unchanged; invalid cache entries are rebuilt automatically.
- Local runtime version resolution runs `<tool> --version` directly, falling back to `latest`.
- `tako deploy` merges build assets (preset assets + `build.assets`) into app `public/` after target build, in listed order.
- `tako deploy` writes `app.json` in the deployed app directory and `tako-server` uses it to resolve the runtime start command.
- `tako releases list` shows release/build history for the current app and environment with commit metadata when available.
- `tako releases rollback <release-id>` rolls target servers back to a previous release id using the normal rolling-update path.
- `tako backups now/list/status/download/restore` manages encrypted private app data backups configured with `[envs.<env>].backup`; backup keys live encrypted in `.tako/secrets.json`.
- `tako servers add` expects a Tailscale MagicDNS name or Tailscale IP, verifies `tako@host` SSH recovery access, enrolls the authenticated SSH key for signed remote management, verifies private management HTTP, then stores detected target metadata (`arch`, `libc`) in each `[[servers]]` entry in `~/.tako/config.toml`. Use `--install` to install or repair `tako-server` over SSH before adding. Encrypted local SSH keys prompt interactively; pass `--ssh-passphrase` for one-line commands.
- `tako deploy` requires valid target metadata for each selected server and does not probe targets during deploy.
- Production environments use Let’s Encrypt certificates by default. Run `tako credentials set ssl.cloudflare --env <env>` for wildcard routes that need Cloudflare DNS-01, or set `ssl = "cloudflare"` and store the same credential to use Cloudflare Origin CA certificates. Deploy verifies required Cloudflare tokens are active before build/upload; Let’s Encrypt wildcard routes also verify zone read access.
- New apps start with desired instance count `0`, and `tako deploy` still validates startup by briefly starting one warm instance; deploy fails if startup health checks fail.

## Run and Test

From repository root:

```bash
cargo run -p tako --bin tako -- --help
cargo run -p tako --bin tako-dev-server -- --help
cargo run -p tako --bin tako-dev-proxy -- --help
cargo test -p tako
```

Run a focused command from source:

```bash
cargo run -p tako --bin tako -- deploy --help
```

## Config Requirements

- `tako.toml` is required for `dev`, `deploy`, `logs`, and `secrets` workflows.
- App-scoped commands default to `./tako.toml`; `-c/--config CONFIG` selects another config file and uses its parent directory as project context. Omitting the `.toml` suffix is supported and recommended for brevity.
- Top-level `name` in `tako.toml` is optional; when omitted, app identity falls back to sanitized project directory name.
- Setting `name` explicitly is recommended for stable identity and uniqueness per server; renaming identity later creates a new app path and requires manual cleanup of old deployments.
- Non-development environments must define `route` or `routes`; development defaults to `{app}.test`.
- `[envs.<name>].ssl` is optional and defaults to `letsencrypt`; Cloudflare SSL and Let’s Encrypt wildcard routes require encrypted credentials from `tako credentials set ssl.cloudflare`. Deploy verifies required Cloudflare tokens before build/upload.

## Related Docs

- `website/src/pages/docs/quickstart.md` (first-run local + remote setup)
- `website/src/pages/docs/development.md` (local dev workflow)
- `website/src/pages/docs/deployment.md` (remote deploy workflow)
