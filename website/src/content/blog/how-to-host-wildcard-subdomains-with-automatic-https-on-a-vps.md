---
title: "How to Host Wildcard Subdomains with Automatic HTTPS on a VPS"
date: "2026-05-20T14:38"
description: "Run tenant-style wildcard subdomains on a VPS with Tako routes, Cloudflare DNS-01 credentials, DNS-only records, and automatic HTTPS."
image: ca44ba6463d9
---

Wildcard subdomains are where a simple VPS setup usually starts to feel less simple.

`app.example.com` is easy. Point a DNS record at the box, let HTTP-01 prove domain ownership, and serve the app. `alice.app.example.com`, `bob.app.example.com`, and every future tenant below `*.app.example.com` are different. You do not know all the hostnames ahead of time, and Let's Encrypt will not issue a wildcard certificate with HTTP-01.

That is the job for DNS-01. Tako uses Cloudflare DNS-01 for wildcard route certificates, while keeping normal app traffic pointed directly at your VPS. Cloudflare proves domain control by creating short-lived TXT records. `tako-server` still terminates TLS itself, routes by hostname, and serves the app from your own server.

The result is the shape most tenant apps want:

| Hostname                   | Purpose                        | Certificate               |
| -------------------------- | ------------------------------ | ------------------------- |
| `app.example.com`          | dashboard, landing page, login | ordinary cert via HTTP-01 |
| `alice.app.example.com`    | tenant subdomain               | wildcard cert via DNS-01  |
| `bob.app.example.com`      | tenant subdomain               | same wildcard cert        |
| `anything.app.example.com` | future tenant                  | same wildcard cert        |

## Configure the Route Shape

Start with the app route. Routes live in [`tako.toml`](/docs/tako-toml) at the environment level, not in a shared reverse-proxy config. A wildcard host must start with `*.` and only covers subdomains below that suffix. It does not cover the apex hostname itself, so list both when you want both:

```toml
name = "dashboard"
runtime = "node"
preset = "nextjs"

[envs.production]
routes = ["app.example.com", "*.app.example.com"]
servers = ["prod"]
source_ip = "direct"
```

`source_ip = "direct"` is explicit here because the DNS records below will be DNS-only. The default `auto` mode would also fall back to the direct peer IP when traffic does not come from Cloudflare, but spelling it out makes the deployment intent visible.

If the wildcard app is the only public route, this also works:

```toml
[envs.production]
route = "*.app.example.com"
servers = ["prod"]
```

Most apps still keep the exact route for login, marketing, or an admin surface. Tako's route matcher chooses the most specific match first, so an exact route such as `app.example.com` or `admin.app.example.com` can coexist with a broader wildcard.

## Point DNS at the VPS

In Cloudflare, create [wildcard DNS records](https://developers.cloudflare.com/dns/manage-dns-records/reference/wildcard-dns-records/) in the `example.com` zone that point at the public IP address of the Tako server. Use [DNS-only records](https://developers.cloudflare.com/dns/proxy-status/), not proxied records:

| Type   | Name    | Target                         | Proxy status |
| ------ | ------- | ------------------------------ | ------------ |
| `A`    | `app`   | your VPS IPv4 address          | DNS only     |
| `A`    | `*.app` | your VPS IPv4 address          | DNS only     |
| `AAAA` | `app`   | your VPS IPv6 address, if used | DNS only     |
| `AAAA` | `*.app` | your VPS IPv6 address, if used | DNS only     |

Cloudflare's dashboard may show `*.app` as `*.app.example.com`; either way, the record belongs to the `example.com` zone. The important bit is the gray-cloud DNS-only mode. Cloudflare can still be your authoritative DNS provider and can still create DNS-01 challenge records through the API. Browser traffic does not need to pass through Cloudflare's reverse proxy.

That distinction matters. If you orange-cloud the wildcard record, Cloudflare sits between browsers and your VPS and may terminate TLS at its edge. That can be useful for other setups, but this tutorial is about Tako owning HTTPS on the server. For direct wildcard subdomains, DNS-only records keep the connection path straightforward: browser to VPS, SNI to Tako, wildcard certificate selected by `tako-server`.

```d2
direction: right

browser: Browser
dns: "Cloudflare DNS\nDNS-only records" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}
vps: "VPS\nTako server" {
  style.fill: "#E88783"
}
routes: "routes\napp + *.app" {
  style.fill: "#9BC4B6"
}
app: "your app\ntenant lookup" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

browser -> dns: "alice.app.example.com"
dns -> vps: "A / AAAA answer"
vps -> routes: "SNI + Host match"
routes -> app: "tenant subdomain"
```

Before deploying, make sure the server is installed and registered as usual. The [deployment docs](/docs/deployment) cover the full server setup, but the short version is:

```bash
curl -fsSL https://tako.sh/install.sh | sh
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
tako servers add prod.example-tailnet.ts.net --name prod
```

## Give Tako DNS-01 Credentials

Wildcard certificates need DNS-01 because the certificate authority has to verify control over the wildcard name. Tako currently supports Cloudflare for that challenge. You create a [Cloudflare API token](https://developers.cloudflare.com/fundamentals/api/get-started/create-token/), scope it to the zone, and let Tako store it as an encrypted environment credential.

The token needs enough access to find the zone and create/delete TXT records:

| Cloudflare permission | Why Tako needs it                                  |
| --------------------- | -------------------------------------------------- |
| Zone: Zone: Read      | Find the matching zone for `*.app.example.com`.    |
| Zone: DNS: Edit       | Create and clean up `_acme-challenge` TXT records. |

Scope the token to the specific zone when you can. For this example, include only `example.com`. You do not need to grant account-wide access, and you do not need a Cloudflare Tunnel token or proxy setting for this flow.

Set up the credential once for the production environment:

```bash
tako credentials set ssl.cloudflare --env production --expires-on "in 90 days"
```

If you omit `--expires-on`, Tako treats the token as having no known expiry. If you set an expiry, deploy will fail after that date and warn during the final 30 days before it. The token is encrypted in `.tako/secrets.json` under the environment's provider credentials; no DNS provider block is written to `tako.toml`.

You can also pass the token non-interactively:

```bash
printf '%s\n' "$CLOUDFLARE_API_TOKEN" | tako credentials set ssl.cloudflare \
  --env production \
  --expires-on "2026-08-18"
```

Use that form in automation only when your shell history and CI logs are under control. The interactive prompt is the safer default for local setup.

## What Happens During Deploy

Now deploy normally:

```bash
tako deploy --env production
```

Before build work starts, the CLI validates the routes and secrets. If any Let’s Encrypt route starts with `*.`, the selected environment must have credential `ssl.cloudflare`. Missing or expired credentials stop the deploy early with a message pointing back to `tako credentials set ssl.cloudflare --env production`.

When validation passes, the CLI decrypts the Cloudflare token locally and includes it in the SSL binding for deploys that actually contain Let’s Encrypt wildcard routes. The management request is signed, and `tako-server` stores the SSL binding encrypted in its SQLite state for that deployed app. Exact-host Let’s Encrypt apps do not receive or retain provider credentials.

The certificate flow is short-lived:

1. `tako-server` sees `*.app.example.com` in the route list.
2. It asks Let's Encrypt for a wildcard certificate.
3. The ACME server returns a DNS-01 challenge value.
4. Tako uses Cloudflare's API to create a TXT record at `_acme-challenge.app.example.com`.
5. After propagation, Tako marks the challenge ready.
6. The certificate is issued and stored under the server's cert directory.
7. Tako attempts to delete the temporary TXT record.

The app does not need to know any of this. It only receives requests. During TLS, `tako-server` uses SNI to look up an exact certificate first, then falls back to a wildcard certificate. During routing, the proxy matches the `Host` header against the route table and forwards the request to the app's loopback instance.

That gives you the useful part of wildcard hosting without a hand-written Nginx config:

| Concern                | Where it lives                          |
| ---------------------- | --------------------------------------- |
| Tenant host pattern    | `routes = ["*.app.example.com"]`        |
| Public DNS             | Cloudflare DNS-only A/AAAA records      |
| DNS-01 API token       | encrypted SSL credential                |
| Wildcard cert issuance | `tako-server` ACME flow                 |
| TLS selection          | SNI exact match, then wildcard fallback |
| Tenant behavior        | your app reads the `Host` header        |

If issuance fails, start with the [troubleshooting docs](/docs/troubleshooting). The usual problems are simple: the wildcard route was deployed without provider credentials, the token cannot read the zone or edit DNS records, the DNS record is pointed at the wrong server, or the app expects `app.example.com` to match the wildcard route. It will not; add the exact route too.

This is the part of self-hosting that should feel boring. One wildcard DNS record points at the box. One encrypted token lets Tako prove domain ownership. One wildcard route sends every tenant hostname to the app. The rest is just your code deciding what `alice` means.
