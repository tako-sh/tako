# E2E Tests

## CLI Output Tests

PTY-based tests that verify rendered terminal output (colors, formatting, spinners) by spawning the `tako` binary in a real pseudo-terminal via Bun's native PTY and parsing the screen with `@xterm/headless`.

```bash
just test::cli
```

Requires the `tako` binary to be built first (`cargo build -p tako-cli`).

## Native Image Dependency

Run `mise bootstrap` to install the repository toolchain, native libvips dependencies, and package dependencies. Full E2E also requires a running Docker engine with Compose support.

Homebrew's `vips` formula includes the codec libraries Tako needs for JPEG, PNG, WebP, and AVIF transforms. Debian/Ubuntu split the AVIF encoder into `libheif-plugin-aomenc`, so install that alongside `libvips-dev`.

Deploy E2E builds Linux binaries inside a pinned glibc builder image, so their libvips dependencies do not depend on the host operating system.

## Docker E2E Fixtures

From repo root:

```bash
just e2e e2e/fixtures/javascript/bun
just e2e e2e/fixtures/javascript/nextjs
just e2e e2e/fixtures/javascript/tanstack-start
just e2e examples/go/basic
```

This runs the global e2e harness in `e2e/run.sh` against the fixture path.
The harness generates an ephemeral SSH keypair per run inside a disposable Docker volume, starts real `tako-server` binaries on an Ubuntu test host, and starts AlmaLinux too when the current server binary's runtime libraries are available there. It never uses `~/.ssh`.
Server containers run privileged with private cgroup namespaces so the production cgroup limits can be exercised. Their entrypoints move init into a control subgroup and enable CPU, memory, and process controllers within that container only. No host cgroup filesystem is bind-mounted. The entrypoint installs the mounted build as root before SSH starts; the runner then uses the installed restricted sudo policy.

On disposable GitHub Actions runners, `e2e/prepare-ci-host.sh` permits DAC reads in the host's `unix-chkpwd` AppArmor profile. That host profile also attaches to AlmaLinux's PAM helper inside privileged containers, where the mode-000 shadow file needs this permission. The fixture check verifies sudo before and after the correction; production PAM policy, shadow permissions, and server capabilities are unchanged.

For a focused installed-isolation check after building the glibc server:

```bash
docker run --rm --privileged --cgroupns=private \
  --volume "$PWD:/workspace:ro" tako-e2e-server-ubuntu \
  sh /workspace/e2e/docker/server/verify-isolation.sh
```

The additional ignored `tako-spawn` test `service_child_joins_cgroup_before_dropping_all_authority` requires root and `TAKO_TEST_CGROUP` pointing at a writable test cgroup in a private namespace. It checks migration from a non-root service identity with only SETUID/SETGID capabilities, followed by execution as the app identity without capabilities.

To also verify Bun dependency hardlinks, mount a Linux Bun binary matching the container architecture:

```bash
docker run --rm --privileged --cgroupns=private \
  --volume "$PWD:/workspace:ro" \
  --volume /absolute/path/to/linux-bun:/proof/bun:ro \
  tako-e2e-server-ubuntu \
  sh /workspace/e2e/docker/server/verify-release-dependencies.sh
```

This checks install, reprovisioning, dependency replacement, service access, and rejection of hardlinked manifests and service-owned files.

Cargo registry and Git caches use the Docker volumes `tako-e2e-cargo-registry` and `tako-e2e-cargo-git`. Build outputs use `target/e2e-linux-glibc`; runnable binaries are copied to `.e2e-bin` (override with `E2E_BIN_DIR`).

After deploy, it runs universal runtime checks:

- App health endpoint responds with valid JSON.
- App root responds with valid HTML or JSON.
- Static/public files (if present in release) are fetched over HTTP.
- Compiled static assets (if present or referenced by HTML) are fetched over HTTP.
- Fixtures with production secrets import the example passphrase and verify a
  secret-backed response.
- The `channels-workflows` fixture additionally opens a real SSE stream,
  verifies direct channel publish delivery, enqueues a workflow, and verifies
  the workflow-published event arrives on the same stream.
