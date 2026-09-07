# Scripts

Repository scripts used by installers, CI checks, and local development workflows.

## Scripts

- `install-tako.sh`: POSIX installer for local `tako`, `tako-dev-server`, and `tako-dev-proxy`. It verifies the release archive SHA-256 checksum before extraction. Official macOS CLI builds support Apple Silicon only. On macOS it verifies `Tako.app` and helper signatures, installs `Tako.app`, symlinks `tako` to the signed CLI inside the app bundle, and installs libvips with Homebrew when available.
- `install-libvips-runtime.sh`: CI helper that installs the libvips runtime used by downloaded Tako binaries on macOS and apt-based Linux runners.
- `package-tako-app.sh`: Packages the Rust `tako` binary as `Tako.app` for macOS signing and iCloud Keychain entitlements.
- `bump-rust-sdk-version.ts`: Bumps the published Rust SDK crate version in `sdk/rust/Cargo.toml` and `Cargo.lock`. Prefer `just sdk-rust patch|minor|major`. The release workflow publishes the Rust SDK only when this Cargo package version changes; `sdk-rust-latest` is moved by CI only after a successful crates.io publish.
- `install-tako-server.sh`: POSIX installer for `tako-server` on Linux hosts.
  - Both installers download assets from the rolling `latest` release (override with `TAKO_RELEASE_TAG`).
  - GitHub-hosted downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.
  - Hosted installers require HTTPS download overrides by default; set `TAKO_ALLOW_INSECURE_DOWNLOAD_BASE=1` only for local test mirrors.
  - Requires glibc and systemd on Linux servers.
  - Starts the service by default via `TAKO_RESTART_SERVICE=1`: refreshes binary/users/helpers, installs the service definition, enables `tako-server`, and starts or restarts it. Restart replaces children that may have been launched under an older isolation policy. Set `TAKO_RESTART_SERVICE=0` for bootstrap-only image builds; after upgrading an existing installation this way, restart the service before relying on the new isolation policy.
  - `tako servers add --install` still bootstraps first, then starts the service with the requested listener ports.
  - Detects the host's Tailscale IP with `tailscale ip -4` and configures remote management HTTP on port `9844` when starting the service. Set `TAKO_MANAGEMENT_HOST` to the server's Tailscale IP to override detection. Service start fails if no Tailscale IP is available.
  - Detects host architecture (`x86_64`/`aarch64`) and verifies glibc before downloading the matching server artifact.
  - Applies `setcap cap_net_bind_service,cap_setuid,cap_setgid,cap_kill=+ep` to `/usr/local/bin/tako-server` for non-root `:80/:443` binds, app-user switching, and stopping app processes; bootstrap installs without active systemd fail if the capability cannot be granted.
  - Creates `tako` (server), `tako-images` (image decoding), and the shared `tako-app` traversal group. App/environment pairs receive separate hashed Unix users and groups at provisioning time.
  - Prepares `/opt/tako` and `/var/run/tako` ownership without recursively traversing existing app releases.
  - Installs restricted maintenance helpers (`/usr/local/bin/tako-server-install-refresh`, `/usr/local/bin/tako-server-service`) and `/usr/local/bin/tako-provision-app`. Sudo permits these fixed operations, with no general root shell. Root-owned `/etc/tako/isolation.conf` binds provisioning to the installed data directory and service identity. App arguments cannot select paths, commands, or Unix IDs.
  - Requires cgroup v2 with writable CPU, memory, and process controllers. Each app gets a 2 GiB memory limit, no swap, a two-CPU quota, and a 512-process limit. Provisioning fails if those limits cannot be installed. Image workers use a separate identity, no supplementary groups, a cleared environment, and address-space/file/process limits.
  - App and image children clear all effective, permitted, inheritable, and ambient capabilities before execution, enable `no_new_privs`, and apply resource limits. The service itself permits its restricted sudo helpers; its additional bounding-set capabilities are not ambient and are used only by the privileged helpers.
  - Enrolls `TAKO_SSH_PUBKEY` for both `tako` SSH login and signed remote management.
  - If `TAKO_SSH_PUBKEY` is unset, prompts for a public key from the terminal (`/dev/tty`) when available, including common piped installs; invalid key lines are re-prompted. If key input cannot be read, installer tries the invoking sudo user's `~/.ssh/authorized_keys` first, then warns/skips if no valid key is found.
  - Installs a systemd unit with `Type=notify`, `ExecReload=/bin/kill -HUP $MAINPID`, high file-descriptor limits, and capability bounding for bind, app-user switching, and app-process stop capabilities.
  - Installs required runtime dependencies (including Unix-socket-capable `nc` with `-U` support, sqlite runtime libraries, libvips image codec packages, Linux namespace networking tools `ip`/`iptables`/`sysctl`, and `proto`) via the host package manager when available.
  - On RHEL-family dnf hosts (AlmaLinux/Rocky/RHEL/CentOS Stream), no repo — including EPEL — ever carries a vips/libvips package, so the installer falls back to [Remi's RPM repository](https://rpms.remirepo.net/) instead of retrying a package that will never exist there.
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
