---
title: "Local Dev with Real HTTPS, Real DNS, and Zero Config"
date: "2026-04-04T14:24"
description: "Stop fighting localhost:3000 and self-signed cert warnings. Tako dev gives you real HTTPS, real domains, and a local proxy — automatically."
image: f8930f578676
---

You open your laptop, run one command, and your app is live at `https://myapp.test/`. Real HTTPS. Real domain. No port number. No browser warning. No config file you had to write.

That's `tako dev`.

## The localhost problem

Most local development looks like this: your app runs on `localhost:3000`, maybe `localhost:3001` for a second service. You bookmark a handful of port numbers. HTTPS? Either you skip it entirely, or you spend an afternoon with `mkcert`, nginx configs, and `/etc/hosts` entries that you'll forget to clean up.

This matters more than it seems. OAuth providers reject non-HTTPS redirect URIs. Secure cookies don't work without HTTPS. Service workers require it. And if your local setup doesn't match production, you're debugging environment differences instead of building features.

Most deployment tools don't even try to solve this. Kamal, Dokku, Coolify, Fly.io — they're focused on getting your code to a server. Local dev is your problem.

## How `tako dev` works

When you run [`tako dev`](/docs/development/), Tako sets up three things automatically:

| Layer     | What it does                                   | How                                                                   |
| --------- | ---------------------------------------------- | --------------------------------------------------------------------- |
| **DNS**   | Resolves `*.test` to your machine              | Local DNS server on `127.0.0.1:53535`, registered via system resolver |
| **HTTPS** | Real TLS certificates, trusted by your browser | Local CA generated once, installed in your system trust store         |
| **Proxy** | Routes `https://{app}.test/` to your app       | Pingora-based proxy on a dedicated loopback address (`127.77.0.1`)    |

The `.test` TLD is [reserved by RFC 6761](https://www.rfc-editor.org/rfc/rfc6761#section-6.2) — it will never resolve to a real domain, so there's no risk of collision. The first time you run it, Tako asks for your password once to install the DNS resolver and trust the CA. After that, it's automatic.

```bash
$ tako dev
  ✓ dev daemon running
  ✓ local CA trusted
  ✓ routes ready

  https://myapp.test/

  r restart · b background · ctrl+c stop
```

Your app gets a proper domain with a green padlock. No port numbers, no `--host 0.0.0.0`, no reverse proxy configs.

```d2
direction: right

browser: Browser {
  url: "https://myapp.test/"
}

dns: Local DNS {
  shape: circle
  style.font-size: 13
}

proxy: Loopback Proxy {
  style.font-size: 13
}

tls: TLS Termination {
  style.font-size: 13
}

router: Route Matcher {
  style.font-size: 13
}

app: Your App {
  shape: hexagon
}

browser -> dns: "A query\n*.test"
dns -> browser: "127.77.0.1"
browser -> proxy: ":443"
proxy -> tls
tls -> router
router -> app
```

## What makes this different

**It's a persistent daemon.** The dev server runs in the background with SQLite-backed state. You can `tako dev` in one project, background it with `b`, start another, and both are accessible at their own `.test` domains simultaneously. Come back tomorrow and your routes are still registered — the daemon wakes apps on incoming requests after they idle out.

**It's the same architecture as production.** Your app uses the same [Tako SDK](/docs/) entrypoint, the same environment variable merging, the same health check protocol. The proxy in dev is the same Pingora-based proxy that runs on your server. If it works at `https://myapp.test/`, it'll work when you [`tako deploy`](/docs/deployment/).

**It's actually zero config.** Tako detects your runtime from your lockfile, resolves your entrypoint from `package.json` or [presets](/docs/presets/), generates certs, sets up DNS, and starts the proxy. The only thing in your [`tako.toml`](/docs/tako-toml/) might be environment variables — and even those are optional.

## Multiple apps, multiple domains

Running a frontend and an API? Each gets its own domain:

```toml
# frontend/tako.toml
[envs.development]
route = "app.test"

# api/tako.toml
[envs.development]
route = "api.test"
```

Open two terminals, `tako dev` in each. Your frontend calls `https://api.test/` with real HTTPS, real CORS headers, real cookies — exactly like production.

## Try it

```bash
brew install takoserver/tap/tako   # or cargo install tako
cd your-project
tako dev
```

Your app is at `https://your-project.test/`. Check out the [development docs](/docs/development/) for the full picture, or the [CLI reference](/docs/cli/) for all the flags.

No nginx. No Caddyfile. No docker-compose. Just your code and a URL that works.
