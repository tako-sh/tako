<a href="https://tako.sh" target="_blank" rel="noopener"><img src="assets/readme-banner.webp" alt="Tako - ship apps to your own servers" width="100%" /></a>

[![npm: tako.sh](https://img.shields.io/npm/v/tako.sh?label=npm%3A%20tako.sh&color=9BC4B6)](https://www.npmjs.com/package/tako.sh)
[![skills.sh](https://skills.sh/b/tako-sh/tako)](https://skills.sh/tako-sh/tako)

## What is Tako?

Ship apps to your own servers without turning deployment into a part-time job.

Tako gives you the "upload files, refresh, done" feeling with modern guardrails: rolling deploys, load balancing, HTTPS, secrets, and logs out of the box.

Tako is not just a deployment tool. The vision is a self-hosted application platform: the backend for your backend.

Deployment is the starting point, not the finish line. Over time, Tako should provide the core primitives teams end up rebuilding in every stack: durable channels, workflows, and other platform capabilities built into one tool instead of stitched together from many.

## Install

Install the CLI:

```bash
curl -fsSL https://tako.sh/install.sh | sh
```

Verify:

```bash
tako --version
```

Start local development from your app directory:

```bash
bun add tako.sh   # or: npm install tako.sh
tako dev
```

Set up a deployment host:

```bash
# Connect the host and your workstation to Tailscale first.
# The host installer bootstraps only; servers add starts and verifies it.
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
tako servers add my-server
# Or install/repair over SSH while adding:
tako servers add root@my-server
# Custom public ports:
tako servers add root@my-server --http-port 8080 --https-port 8443
```

Deploy your app:

```bash
tako init    # prompts for app name + route, offers wildcard HTTPS setup, writes tako.toml
tako servers add my-server
# Optional: Let’s Encrypt wildcard certificates via Cloudflare DNS-01; traffic can stay DNS-only/direct.
# Use a User API token with Zone Read and DNS Write for the matching Cloudflare zone.
tako credentials set ssl.cloudflare --env production --expires-on "in 90 days"
# Optional: Cloudflare Origin CA certificates for Cloudflare-proxied apps.
# Set ssl = "cloudflare" in tako.toml, then store the provider token:
tako credentials set ssl.cloudflare --env production --expires-on "in 90 days"
tako deploy
```

## Quick links

- [Quickstart](https://tako.sh/docs/quickstart) — install to live in minutes
- [How Tako Works](https://tako.sh/docs/how-tako-works) — architecture and mental model
- [tako.toml Reference](https://tako.sh/docs/tako-toml) — every config option
- [CLI Reference](https://tako.sh/docs/cli) — all commands and flags
- [Framework Guides](https://tako.sh/docs/framework-guides) — adapter examples
- [Local Development](https://tako.sh/docs/development) — HTTPS, DNS, environment variables
- [Deployment](https://tako.sh/docs/deployment) — deploy flow, rolling updates, rollbacks
- [Troubleshooting](https://tako.sh/docs/troubleshooting) — common issues and fixes
- [Examples](https://github.com/tako-sh/tako/tree/main/examples)
- [SDK](https://www.npmjs.com/package/tako.sh)
- [Benchmarks](BENCHMARKS.md) — current TLDR and raw-data links

## Agent Skills

Install Tako skills for Codex, Claude Code, Cursor, and other skills.sh-compatible agents:

```bash
npx skills add tako-sh/tako
```

Install a specific skill with `--skill tako`, `--skill tako-sdk-js`, or `--skill tako-sdk-go`.

## License

MIT — see [LICENSE](LICENSE).
