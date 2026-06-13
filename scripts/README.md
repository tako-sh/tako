# Scripts

Repository scripts used by installers, CI checks, and local development workflows.

## Scripts

- `install-tako.sh`: POSIX installer for local `tako`, `tako-dev-server`, and `tako-dev-proxy`. It verifies the release archive SHA-256 checksum before extraction. On macOS it verifies `Tako.app` and helper signatures, installs `Tako.app`, symlinks `tako` to the signed CLI inside the app bundle, and installs libvips with Homebrew when available.
- `install-libvips-runtime.sh`: CI helper that installs the libvips runtime used by downloaded Tako binaries on macOS and apt-based Linux runners.
- `package-tako-app.sh`: Packages the Rust `tako` binary as `Tako.app` for macOS signing and iCloud Keychain entitlements.
- `bump-rust-sdk-version.ts`: Bumps the published Rust SDK crate version in `sdk/rust/Cargo.toml` and `Cargo.lock`. Prefer `just sdk-rust patch|minor|major`. The release workflow publishes the Rust SDK only when this Cargo package version changes; `sdk-rust-latest` is moved by CI only after a successful crates.io publish.
- `install-tako-server.sh`: POSIX installer for `tako-server` on Linux hosts.
  - Both installers download assets from the rolling `latest` release (override with `TAKO_RELEASE_TAG`).
  - GitHub-hosted downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.
  - Hosted installers require HTTPS download overrides by default; set `TAKO_ALLOW_INSECURE_DOWNLOAD_BASE=1` only for local test mirrors.
  - Supports systemd and OpenRC service definitions.
  - Starts the service by default via `TAKO_RESTART_SERVICE=1`: refreshes binary/users/helpers, installs the service definition, enables `tako-server`, and starts or reloads it. Set `TAKO_RESTART_SERVICE=0` for bootstrap-only image builds or refreshes that should not touch the running service.
  - `tako servers add --install` still bootstraps first, then starts the service with the requested listener ports.
  - Detects the host's Tailscale IP with `tailscale ip -4` and configures remote management HTTP on port `9844` when starting the service. Set `TAKO_MANAGEMENT_HOST` to the server's Tailscale IP to override detection. Service start fails if no Tailscale IP is available.
  - Detects host architecture (`x86_64`/`aarch64`) and libc (`glibc`/`musl`) to download the matching server artifact.
  - Applies `setcap cap_net_bind_service,cap_setuid,cap_setgid,cap_kill=+ep` to `/usr/local/bin/tako-server` for non-root `:80/:443` binds, app-user switching, and stopping app processes; non-systemd/OpenRC installs fail if the capability cannot be granted.
  - Creates both `tako` (server) and `tako-app` (app process) users.
  - Prepares `/opt/tako` and `/var/run/tako` ownership without recursively traversing existing app releases.
  - Installs restricted maintenance helpers (`/usr/local/bin/tako-server-install-refresh`, `/usr/local/bin/tako-server-service`) and a scoped sudoers policy so the `tako` SSH user can run upgrade/reload commands non-interactively.
  - Enrolls `TAKO_SSH_PUBKEY` for both `tako` SSH login and signed remote management.
  - If `TAKO_SSH_PUBKEY` is unset, prompts for a public key from the terminal (`/dev/tty`) when available, including common piped installs; invalid key lines are re-prompted. If key input cannot be read, installer tries the invoking sudo user's `~/.ssh/authorized_keys` first, then warns/skips if no valid key is found.
  - Installs service definitions based on host init system:
    - systemd unit with `Type=notify`, `ExecReload=/bin/kill -HUP $MAINPID`, high file-descriptor limits, and capability bounding for bind, app-user switching, and app-process stop capabilities.
    - OpenRC init script with high file-descriptor limits, `reload` support, and `retry="TERM/1800/KILL/5"` graceful-stop semantics.
  - Installs required runtime dependencies (including Unix-socket-capable `nc` with `-U` support, sqlite runtime libraries, libvips image codec packages, Linux namespace networking tools `ip`/`iptables`/`sysctl`, and `proto`) via the host package manager when available.
  - Falls back to the official `proto` installer if not already present.
- `check_critical_coverage.sh`: coverage gate for selected critical source files.

## Typical Usage

Run from repository root:

```bash
sh scripts/install-tako.sh
sh scripts/install-tako-server.sh
bash scripts/check_critical_coverage.sh
```

The install scripts are exposed via website redirect endpoints:

- `/install.sh`
- `/install-server.sh`
- `/server-install.sh`
