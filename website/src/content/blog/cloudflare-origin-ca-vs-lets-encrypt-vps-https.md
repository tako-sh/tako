---
title: "Cloudflare Origin CA vs Let’s Encrypt for Self-Hosted HTTPS on a VPS"
seoTitle: "Cloudflare Origin CA vs Let’s Encrypt"
date: "2026-05-24T14:34"
description: "Compare Let’s Encrypt HTTP-01, Cloudflare DNS-01 wildcard certificates, and Cloudflare Origin CA for proxied Tako apps."
image: ab9632db7a90
---

HTTPS is not one decision. It is two decisions pretending to be one.

First: who should issue the certificate on your origin server? Second: how will browser traffic actually reach that origin? If browsers connect straight to your VPS, you want a publicly trusted certificate. If every request goes through Cloudflare first, a certificate trusted by Cloudflare can be exactly right. If your app owns tenant subdomains, the interesting part is not the CA at all. It is the validation method.

Tako now has all three paths in the same [`tako.toml`](/docs/tako-toml/) model: exact-host Let’s Encrypt by default, Let’s Encrypt wildcard certificates through Cloudflare DNS-01, and Cloudflare Origin CA for Cloudflare-proxied apps. The trick is picking the certificate path that matches the traffic path.

## The three HTTPS paths

For normal public routes, Tako uses Let’s Encrypt. Let’s Encrypt is a public certificate authority, its certificates are trusted by browsers, and its [HTTP-01 challenge](https://letsencrypt.org/docs/challenge-types/#http-01-challenge) proves control by serving a token from your web server on port 80. That is the boring default, which is exactly what you want for `app.example.com`, `api.example.com`, or `www.example.com`.

Wildcard routes are different. Let’s Encrypt explicitly requires [DNS-01 for wildcard certificates](https://letsencrypt.org/docs/faq/#does-lets-encrypt-issue-wildcard-certificates). DNS-01 proves you control DNS by creating a TXT record under `_acme-challenge.<domain>`, so Tako needs a DNS provider credential. Today, that provider is Cloudflare DNS. The app traffic can still go straight to your VPS; Cloudflare is only helping answer the certificate challenge.

Cloudflare Origin CA is the other shape. Cloudflare says Origin CA certificates are for origins that only receive traffic from [proxied records](https://developers.cloudflare.com/dns/proxy-status/) and are compatible with [Full (strict)](https://developers.cloudflare.com/ssl/origin-configuration/ssl-modes/full-strict/) mode. The certificate is trusted by Cloudflare, not by normal browsers connecting directly to your server. That makes it a strong fit when Cloudflare is intentionally the front door.

| Route shape                              | DNS/proxy shape         | Tako SSL setting                 | Credential needed | Best fit                                              |
| ---------------------------------------- | ----------------------- | -------------------------------- | ----------------- | ----------------------------------------------------- |
| `app.example.com`                        | DNS-only or proxied     | omitted or `ssl = "letsencrypt"` | None              | Public browser-trusted HTTPS on an exact hostname     |
| `*.app.example.com`                      | Usually DNS-only to VPS | omitted or `ssl = "letsencrypt"` | `ssl.cloudflare`  | Tenant subdomains with browser-trusted wildcard certs |
| `app.example.com` or `*.app.example.com` | Cloudflare proxied      | `ssl = "cloudflare"`             | `ssl.cloudflare`  | Origin TLS when Cloudflare is the only public path    |

Here is the mental model:

```d2
direction: right

browser: Browser

direct_dns: "DNS-only record" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

cloudflare: "Cloudflare proxy" {
  shape: cloud
  style.fill: "#F6A25F"
}

vps: "VPS\nTako server" {
  style.fill: "#E88783"
}

app: "your app" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

dns01: "DNS-01 TXT record\nfor wildcard validation" {
  style.fill: "#9BC4B6"
}

browser -> direct_dns: "exact host or wildcard"
direct_dns -> vps: "public TLS\nLet's Encrypt"
browser -> cloudflare: "proxied hostname"
cloudflare -> vps: "origin TLS\nOrigin CA or public CA"
vps -> app: "route match"
dns01 -> vps: "issue wildcard cert"
```

The same VPS can use more than one path across different apps. A direct tenant app can use DNS-only wildcard records and Let’s Encrypt DNS-01. A marketing site behind Cloudflare WAF can use Origin CA. A small API can use exact-host Let’s Encrypt and never think about provider credentials.

## Configuring the choice in Tako

Exact-host Let’s Encrypt is the default. This config asks Tako to issue certificates for the listed route names, route by host, and serve the app from the server named `la`:

```toml
name = "api"
runtime = "bun"

[envs.production]
routes = ["api.example.com", "www.api.example.com"]
servers = ["la"]
```

There is no provider credential because the origin can prove each exact hostname through HTTP-01. The server still needs public HTTP reachability for issuance. The [deployment docs](/docs/deployment/) cover the server install flow and public proxy ports; the short version is that Tako owns `:80` for ACME challenges and `:443` for normal HTTPS.

For tenant-style wildcard routes, keep Let’s Encrypt and add the Cloudflare credential:

```toml
name = "dashboard"
runtime = "node"
preset = "nextjs"

[envs.production]
routes = ["app.example.com", "*.app.example.com"]
servers = ["la"]
source_ip = "direct"
```

Then set the provider credential once for that environment:

```bash
tako credentials set ssl.cloudflare --env production
```

Tako stores that token as an encrypted provider credential, not as an app secret. It is not exposed to your process and is not part of generated secret types. During deploy, Tako validates that the credential exists, has not expired when expiry metadata is known, and can read the matching Cloudflare zone. The server then uses the token to create short-lived DNS-01 TXT records for issuance and renewal.

Use this when you want public, browser-trusted certificates and direct traffic to the VPS. It is also the right path when you need a wildcard route but do not want Cloudflare sitting in the request path. Our [wildcard subdomain guide](/blog/how-to-host-wildcard-subdomains-with-automatic-https-on-a-vps/) walks through the DNS-only record shape in more detail.

For Cloudflare-proxied apps, make the proxy path explicit:

```toml
name = "web"
runtime = "node"
preset = "nextjs"

[envs.production]
route = "www.example.com"
servers = ["la"]
ssl = "cloudflare"
source_ip = "cloudflare-proxy"
```

And use the same credential name:

```bash
tako credentials set ssl.cloudflare --env production
```

This time the token is used for Cloudflare Origin CA, not DNS-01. Tako asks Cloudflare for an origin certificate, stores it on the server, and renews it like other managed certificates. `source_ip = "cloudflare-proxy"` tells Tako to require requests from Cloudflare IP ranges and use `CF-Connecting-IP` as the client IP. That matters for logs, rate limits, and any app behavior that depends on the real visitor address.

The catch is important: do not use Origin CA for a route that users may reach directly. Cloudflare's own docs warn that disabling proxying or pausing Cloudflare can expose visitors to untrusted certificate errors, because Origin CA certificates only cover the Cloudflare-to-origin hop. If you want the route to work both through Cloudflare and directly to the VPS, use Let’s Encrypt instead.

## The practical rule

Choose based on the first public hop, not based on which provider happens to host your DNS.

| If this is true                                                                             | Use                                           | Why                                                                               |
| ------------------------------------------------------------------------------------------- | --------------------------------------------- | --------------------------------------------------------------------------------- |
| Browsers connect directly to `app.example.com`                                              | Exact-host Let’s Encrypt                      | Publicly trusted, no provider credential, simple renewal                          |
| Browsers connect directly to tenant subdomains like `alice.app.example.com`                 | Let’s Encrypt wildcard with Cloudflare DNS-01 | Publicly trusted wildcard cert; Cloudflare only proves DNS control                |
| Browsers always hit Cloudflare first and your origin should not be a public direct endpoint | Cloudflare Origin CA                          | Works cleanly with Full (strict), keeps Cloudflare as the intended trust boundary |
| You are unsure whether direct origin access needs to work                                   | Let’s Encrypt                                 | A public CA certificate leaves the fewest surprises                               |

That last row is the safe default. Let’s Encrypt certificates are public DV certificates, valid for normal browsers and operating systems, and [renewed on a short lifetime](https://letsencrypt.org/docs/faq/#what-is-the-lifetime-for-lets-encrypt-certificates-for-how-long-are-they-valid). Origin CA is not worse. It is narrower. Narrow is great when the system shape is narrow too: Cloudflare edge in front, origin hidden behind it, Full (strict) enabled, and direct browser traffic intentionally unsupported.

The credential story is also deliberately small. There is one provider credential name today, `ssl.cloudflare`, stored per environment with:

```bash
tako credentials set ssl.cloudflare --env production --expires-on "in 90 days"
```

Deploy fails before build work starts if a required certificate credential is missing, expired, disabled, or invalid for the selected flow. If the credential expires within 30 days, deploy warns before you get surprised by a renewal later. The [CLI reference](/docs/cli/) covers credential commands, and [troubleshooting](/docs/troubleshooting/) is the place to start when certificate issuance fails.

So the decision tree is short:

1. Exact hostname and direct public traffic? Do nothing special. Let’s Encrypt is already the default.
2. Wildcard hostname and direct public traffic? Keep Let’s Encrypt, set `ssl.cloudflare`, and use DNS-only records.
3. Cloudflare is the only public front door? Set `ssl = "cloudflare"`, keep the DNS record proxied, and usually set `source_ip = "cloudflare-proxy"`.

Tako’s job is to make that choice live beside the app route instead of in a separate proxy cookbook. Routes decide where traffic goes. SNI decides which certificate is served. The SSL provider setting decides how that certificate is issued. Once those three agree, self-hosted HTTPS gets pleasantly boring again.
