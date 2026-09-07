# Docker Assets

Internal Docker tooling for debugging Tako.

## Files

- `testbed.Dockerfile`: internal debug container (Debian Bookworm + sshd) used for deploy/install debugging.
  - Bootstraps server-side dependencies through `scripts/install-tako-server.sh` so debug image deps stay aligned with installer behavior.
- `install-authorized-key.sh`: helper script used by debug container boot flow.

`testbed.Dockerfile` is internal-only and not intended as a production application image.

## Common Commands

From repository root:

```bash
just testbed::create
just testbed::install
```
