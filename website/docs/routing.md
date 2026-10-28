---
title: Routing and TLS
description: "Configure Tako domains and path routes per environment, choose automatic TLS or Cloudflare Origin CA, and control trusted client-IP handling."
---

Each deployment environment declares the hostnames and paths that route to its app. `tako-server` terminates TLS and routes requests to deployed assets or healthy app instances. Local `.test` routes use the development proxy instead; see [Local development](/development/#routes).

## Declare production routes

```toml
[envs.production]
routes = ["app.example.com", "example.com/api/*"]
servers = ["la"]
```

Use either `route = "app.example.com"` or `routes = [...]`, not both. Deployable environments need at least one route. Supported forms are an exact host, wildcard host, host-plus-path, and wildcard-host-plus-path. Path-only routes are invalid. More specific routes win; conflicting routes are rejected at deploy.

Point public DNS or your external proxy at the selected servers. Declaring a route does not create public DNS records. Tako reserves matched-app `/_tako/*` paths for its image, storage, and realtime endpoints; do not use that prefix for application routes.

## Choose certificates

| Configuration                              | Certificates                         | Required setup                                   |
| ------------------------------------------ | ------------------------------------ | ------------------------------------------------ |
| Exact route, default `ssl = "letsencrypt"` | Let's Encrypt through HTTP-01        | Public HTTP port 80 reaches the server.          |
| Let's Encrypt with `ssl.cloudflare` set    | DNS-01 for exact and wildcard routes | Cloudflare credential for the zone.              |
| Wildcard route with Let's Encrypt          | DNS-01                               | `ssl.cloudflare` is required.                    |
| `ssl = "cloudflare"`                       | Cloudflare Origin CA                 | `ssl.cloudflare` and Cloudflare-proxied traffic. |

```bash
tako credentials set ssl.cloudflare --env production
```

The credential is encrypted and managed by Tako; do not put it in `tako.toml` or expose it to app code. See [Secrets](/secrets/#provider-and-storage-credentials) for the distinction between app and provider credentials.

Cloudflare Origin CA certificates are not trusted by direct browser connections to the origin. Use the certificate mode that matches how traffic reaches the host. Public HTTP/HTTPS listener ports are configured during [server setup](/servers/). Non-default HTTPS ports appear in deploy summaries and HTTP redirect targets.

## Derive client IPs

Set `source_ip` on the environment:

```toml
[envs.production]
route = "app.example.com"
servers = ["la"]
source_ip = "auto"
```

`auto` detects Cloudflare when possible, then configured trusted proxies, then direct peers. `direct`, `cloudflare-proxy`, and `trusted-proxy` choose stricter modes. This policy affects which client IP the proxy trusts; choose it for the actual network path rather than accepting arbitrary forwarded headers.

## Verify

Deploy the route configuration, inspect `tako status --json`, and request each public hostname and path. Check the certificate chain, HTTP-to-HTTPS redirect, and a representative app response. For wildcard routes, test a concrete subdomain. Inspect finite app logs for certificate or routing failures; see [Troubleshooting](/troubleshooting/#tls-or-certificate-problems).
