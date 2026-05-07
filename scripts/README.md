# Scripts

Repository scripts used by installers, CI checks, and local development workflows.

## Scripts

- `install-tako.sh`: POSIX installer for local `tako`, `tako-dev-server`, and `tako-dev-proxy`. On macOS it verifies `Tako.app` and helper signatures, installs `Tako.app`, and symlinks `tako` to the signed CLI inside the app bundle.
- `package-tako-app.sh`: Packages the Rust `tako` binary as `Tako.app` for macOS signing and iCloud Keychain entitlements.
- `install-tako-server.sh`: POSIX installer for `tako-server` on Linux hosts.
  - Both installers download assets from the rolling `latest` release (override with `TAKO_RELEASE_TAG`).
  - GitHub-hosted downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.
  - Hosted installers require HTTPS download overrides by default; set `TAKO_ALLOW_INSECURE_DOWNLOAD_BASE=1` only for local test mirrors.
  - Supports systemd and OpenRC for normal install/start.
  - Supports install-refresh mode via `TAKO_RESTART_SERVICE=0` (refreshes binary/users without restarting service; service definition is updated only when a supported manager is active), used in build/container workflows before init/service managers are running.
  - Detects host architecture (`x86_64`/`aarch64`) and libc (`glibc`/`musl`) to download the matching server artifact.
  - Applies `setcap cap_net_bind_service,cap_setuid,cap_setgid=+ep` to `/usr/local/bin/tako-server` for non-root `:80/:443` binds and app-user switching; non-systemd/OpenRC installs fail if the capability cannot be granted.
  - Creates both `tako` (server) and `tako-app` (app process) users.
  - Installs restricted maintenance helpers (`/usr/local/bin/tako-server-install-refresh`, `/usr/local/bin/tako-server-service`) and a scoped sudoers policy so the `tako` SSH user can run upgrade/reload commands non-interactively.
  - If `TAKO_SSH_PUBKEY` is unset, prompts for a public key from the terminal (`/dev/tty`) when available, including common piped installs; invalid key lines are re-prompted. If key input cannot be read, installer tries the invoking sudo user's `~/.ssh/authorized_keys` first, then warns/skips if no valid key is found.
  - Installs service definitions based on host init system:
    - systemd unit with `Type=notify`, `ExecReload=/bin/kill -HUP $MAINPID`, and capability bounding for bind and app-user switching capabilities.
    - OpenRC init script with `reload` support and `retry="TERM/1800/KILL/5"` graceful-stop semantics.
  - Installs required runtime dependencies (including Unix-socket-capable `nc` with `-U` support, sqlite runtime libraries, Linux namespace networking tools `ip`/`iptables`/`sysctl`, and `proto`) via the host package manager when available.
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
