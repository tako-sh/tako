# E2E Tests

## CLI Output Tests

PTY-based tests that verify rendered terminal output (colors, formatting, spinners) by spawning the `tako` binary in a real pseudo-terminal via Bun's native PTY and parsing the screen with `@xterm/headless`.

```bash
just test::cli
```

Requires the `tako` binary to be built first (`cargo build -p tako-cli`).

Focused native-network tests live in `e2e/cli/`. Run `cargo test -p tako-cli --bin tako network::` for configuration, identity, ownership, resolver cleanup, SSH configuration, and transport unit tests.

The following scripts must run inside disposable privileged Linux containers, with the checkout mounted read-only. They reject execution outside a container. Install Python, iproute2, ping, wireguard-tools, wireguard-go, OpenSSH client/server, systemd-resolved, D-Bus, and the CLI's libvips runtime dependencies in the test image:

```bash
python3 /checkout/e2e/cli/embedded-wireguard.py /checkout/target/debug/tako
python3 /checkout/e2e/cli/private-dns.py /checkout/target/debug/tako
python3 /checkout/e2e/cli/rootless-ssh.py /checkout/target/debug/deps/tako-TEST_HASH
```

Use the test executable reported by `cargo test -p tako-cli --bin tako --no-run` for `tako-TEST_HASH`. The first script exercises two embedded tunnels in isolated namespaces; it substitutes the resolver adapter because D-Bus cannot see those namespaces. The second uses real systemd-resolved and a peer DNS server to check private names, split DNS, conflict rollback, and disconnect cleanup. The third drops privileges before exercising a 1 MiB TCP round trip, then checks concurrent OpenSSH channels and exclusion between active SSH sessions and system connection changes. The reference WireGuard executable is a test peer, not a Tako runtime dependency.

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
just e2e examples/compose/bun-redis
```

This runs the global e2e harness in `e2e/run.sh` against the fixture path.
The runner disables CLI telemetry so local and CI test runs do not send usage stats.
The harness generates an ephemeral SSH keypair per run inside a disposable Docker volume, starts real `tako-server` binaries on an Ubuntu test host, and starts AlmaLinux too when the current server binary's runtime libraries are available there. It never uses `~/.ssh`.
Server containers run privileged with private cgroup namespaces so the production cgroup limits can be exercised. Their entrypoints move init into a control subgroup and enable CPU, memory, and process controllers within that container only. No host cgroup filesystem is bind-mounted. The entrypoint installs the mounted build as root before SSH starts; the runner then uses the installed restricted sudo policy.

Each test host also runs its own Docker Engine for Compose releases. The executor's Docker socket is not mounted into the test hosts. `E2E_PROXY_CA` optionally supplies a CA bundle for build and runtime downloads; in managed cloud sessions it defaults to `CODEX_PROXY_CERT`. Build trust is mounted through BuildKit secrets, with certificate verification enabled.

The staged JavaScript fixtures receive the runner's proxy and CA settings as explicit production variables so isolated dependency installs can access the registry. The committed fixtures are unchanged.

The AlmaLinux image includes libvips and freezes DNF metadata after installing system dependencies. Startup still runs the real installer against those installed packages; app dependency installs and runtime downloads continue to use the network.

The Bun + Redis example checks first deployment, service status, logs, code updates, server restart, explicit rollback, and recovery after a failed HTTP health check. The Redis counter must increase across every transition. `just test e2e` includes this example after the native fixtures.

Additional Compose lifecycle tests can run directly on a development machine with Docker Compose 2.24.4 or newer:

```bash
cargo test -p tako-compose -- --ignored --test-threads=1
cargo test -p tako-server --bin tako-server first_compose_prepare_uses_incoming_secrets_without_committing_them -- --ignored
cargo test -p tako-cli --bin tako-dev-server compose_registration_restart_stop_and_shutdown_keep_redis_data -- --ignored
```

When running these tests inside a container, its temporary project paths and loopback must be shared with its Docker engine. Both tests use unique projects and remove their own test volumes afterward.

For `on_demand_integration` and `rolling_update_integration`, an installed disposable Linux host can set `TAKO_E2E_DISPOSABLE_HOST=1` and `TAKO_TEST_SERVER_WRAPPER=/workspace/e2e/docker/server/test-wrapper.sh`. Run the test executables as root with `--test-threads=1`, with Bun installed in a path accessible to app users. The wrapper installs the test binary with production capabilities, points the real isolation helper at each temporary data directory, and starts the service as `tako`. The fixture uploader reproduces the ownership of an SSH upload. The wrapper refuses to run outside a root Docker test container.

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

Set `E2E_USE_PREBUILT=1` to reuse existing binaries and all three test images without rebuilding them. Build these artifacts from the checkout under test first; this option deliberately skips freshness checks and fails if an artifact is missing. It is useful for repeated fixture runs or CI-provided artifacts.

`E2E_BUN_CACHE_DIR=/absolute/path/to/bun/cache` optionally shares a writable Bun package cache with the runner. Fixture installs prefer cached packages and reuse the repository lockfile, with bounded retries for missing downloads.

After deploy, it runs universal runtime checks:

- App health endpoint responds with valid JSON.
- App root responds with valid HTML or JSON.
- Static/public files (if present in release) are fetched over HTTP.
- Compiled static assets (if present or referenced by HTML) are fetched over HTTP.
- Fixtures with production secrets import the example passphrase and verify a
  secret-backed response.
- The `channels-workflows` fixture additionally opens a real SSE stream,
  verifies direct channel publish delivery, enqueues a workflow with a supplied
  run ID, verifies the workflow-published event arrives on the same stream, and
  checks that a repeated enqueue returns the same run ID.
