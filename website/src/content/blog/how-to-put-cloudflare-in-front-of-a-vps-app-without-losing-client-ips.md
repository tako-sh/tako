---
title: "How to Put Cloudflare in Front of a VPS App Without Losing Client IPs"
date: "2026-06-10T14:28"
description: "Use Cloudflare in front of a Tako VPS app while preserving real visitor IPs for logs, rate limits, redirects, and upstream headers."
image: b6291ea694ec
---

Putting Cloudflare in front of a VPS app is easy until the app needs to know who is actually visiting.

Your server sees a connection from Cloudflare. Your app wants the browser's IP address for logs, abuse limits, session security, fraud checks, geolocation, or "why did this one user get rate limited?" debugging. The hard part is not finding an IP-looking header. The hard part is trusting the right one only when the request really came through the proxy you meant to trust.

Tako handles that at the route layer with [`source_ip`](/docs/tako-toml/). You choose the traffic shape once in `tako.toml`, and `tako-server` turns it into the client IP used for per-IP limits, upstream `X-Forwarded-For`, redirects, and request diagnostics.

## The proxy header problem

Cloudflare sends visitor identity to origins in headers. The important one is [`CF-Connecting-IP`](https://developers.cloudflare.com/fundamentals/reference/http-headers/#cf-connecting-ip): it contains the client IP address Cloudflare saw for the request it is forwarding to your origin. Cloudflare also sends [`X-Forwarded-For`](https://developers.cloudflare.com/fundamentals/reference/http-headers/#x-forwarded-for), but that header can be a chain because every proxy along the way may append to it.

That difference matters. A single IP header is easier to reason about. A chain is useful, but only when you know which proxies were allowed to write to it. If a browser connects directly to your VPS and sends its own `X-Forwarded-For: 1.2.3.4`, your app should not believe it just because it looks official.

The trust boundary is the peer connection. If the direct peer is Cloudflare, `CF-Connecting-IP` is meaningful. If the direct peer is a random client on the internet, the same header is just user input with a fancy name.

```d2
direction: right

browser: "Browser\n203.0.113.15" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

cloudflare: "Cloudflare edge" {
  shape: cloud
  style.fill: "#9BC4B6"
}

tako: "VPS\nTako server" {
  style.fill: "#E88783"
}

app: "Your app" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

browser -> cloudflare: "HTTPS request"
cloudflare -> tako: "CF-Connecting-IP: 203.0.113.15"
tako -> app: "X-Forwarded-For: 203.0.113.15"
```

Tako's default mode is designed for that exact situation. When `source_ip` is omitted, it behaves as `auto`: if the peer IP belongs to Cloudflare and the request has a valid `CF-Connecting-IP`, Tako uses that value. Otherwise it can use configured trusted-proxy headers, and if neither proxy path applies, it falls back to the direct peer IP.

That makes the boring path work without a proxy cookbook. It also means a route can move from DNS-only to Cloudflare-proxied without changing app code. Your app still sees the selected visitor IP through the normal request headers.

## Pick the mode that matches the traffic

The most useful rule is simple: configure `source_ip` for the first hop your VPS should trust.

| Mode               | What Tako trusts                                                                                | Direct non-matching requests | Best fit                                               |
| ------------------ | ----------------------------------------------------------------------------------------------- | ---------------------------- | ------------------------------------------------------ |
| omitted or `auto`  | Cloudflare IPs with `CF-Connecting-IP`, then configured trusted proxy headers, then direct peer | Accepted as direct traffic   | Mixed or migrating setups                              |
| `direct`           | Only the TCP peer IP                                                                            | Accepted as direct traffic   | DNS-only records straight to the VPS                   |
| `cloudflare-proxy` | Cloudflare IPs with `CF-Connecting-IP`                                                          | Rejected with `403`          | Cloudflare is the intended public front door           |
| `trusted-proxy`    | Loopback or configured trusted CIDRs with `X-Forwarded-For` or `Forwarded`                      | Rejected with `403`          | nginx, HAProxy, Caddy, Traefik, or another front proxy |

For a normal Cloudflare-proxied app, make that intent visible:

```toml
name = "web"
runtime = "node"
preset = "nextjs"

[envs.production]
route = "www.example.com"
servers = ["la"]
source_ip = "cloudflare-proxy"
```

Then create an `A`, `AAAA`, or `CNAME` record in Cloudflare with proxying enabled for the hostname. The orange-cloud proxy path is now part of the deployment contract. Requests that arrive straight at the VPS with forged Cloudflare headers are not treated as Cloudflare traffic; in strict Cloudflare mode, they are rejected.

If the app is intentionally direct-to-VPS, use `direct`:

```toml
[envs.production]
routes = ["api.example.com", "*.api.example.com"]
servers = ["la"]
source_ip = "direct"
```

That is a good match for DNS-only Cloudflare records, including wildcard subdomain setups where Cloudflare is your DNS provider but not your reverse proxy. We covered the DNS-only wildcard flow in [How to Host Wildcard Subdomains with Automatic HTTPS on a VPS](/blog/how-to-host-wildcard-subdomains-with-automatic-https-on-a-vps/). In that shape, the browser connects to Tako directly, so the real client IP is already the peer IP.

Use `auto` when you want Tako to adapt. It is the generated default because many apps start direct, then put Cloudflare in front later for WAF, caching, DDoS protection, or global routing. Tako keeps Cloudflare IP ranges in memory, starts from bundled fallback ranges, overlays a last-known-good cache from disk, and refreshes the list while running when routes need Cloudflare detection.

Use `trusted-proxy` when Cloudflare is not the immediate peer but some other proxy is. For example, you might put Caddy or HAProxy in front of Tako on the same machine, or terminate a private network path before the request reaches the Tako proxy. In that mode, Tako only accepts forwarded client IP headers from loopback or from server-level trusted CIDRs. It reads `X-Forwarded-For` or the standardized `Forwarded` header, then rejects requests that do not come from a trusted proxy boundary.

## What your app receives

Once Tako resolves the client IP, it normalizes what the app sees.

For proxied upstream requests, `tako-server` sets `X-Forwarded-Proto` to the browser-facing scheme and forwards `X-Request-ID` for tracing. If a client IP was accepted, it sets `X-Forwarded-For` to that selected IP. If no client IP is accepted, it removes `X-Forwarded-For`. It also removes the incoming `Forwarded` header before the request reaches your app, so your framework does not accidentally parse a chain Tako did not choose.

That gives app code one boring contract:

```ts
export default {
  async fetch(request: Request) {
    const ip = request.headers.get("x-forwarded-for");
    const requestId = request.headers.get("x-request-id");

    return Response.json({ ip, requestId });
  },
};
```

The same resolved IP is used by Tako's browser-facing per-IP request limit. That limit is enforced before your app handles the request, so putting Cloudflare in front should not collapse every visitor into "the Cloudflare IP" from Tako's point of view. It also shows up in app-scoped proxy diagnostics available through [`tako logs`](/docs/cli/), alongside the request ID, route match, status, handler path, total latency, cold-start wait time, and upstream response latency.

Forwarded HTTPS metadata gets the same treatment. Tako only honors `X-Forwarded-Proto` and `Forwarded: proto=https` from trusted peers: loopback, Cloudflare, or configured trusted proxy CIDRs. Direct clients cannot skip redirects by spoofing those headers.

That is the shape we want from a deploy tool. App code can stay ordinary. The infrastructure layer decides which peer is trusted, which header is canonical, and which headers are scrubbed before they reach the runtime.

## A practical Cloudflare checklist

For Cloudflare in front of a VPS app, start with the traffic path:

| Step                                   | Choice                                      | Tako setting                                                      |
| -------------------------------------- | ------------------------------------------- | ----------------------------------------------------------------- |
| Cloudflare is only DNS                 | DNS-only records to the VPS                 | `source_ip = "direct"` or default `auto`                          |
| Cloudflare is the public reverse proxy | Proxied records to the VPS                  | `source_ip = "cloudflare-proxy"`                                  |
| Cloudflare Origin CA is used           | Cloudflare must stay in the request path    | `ssl = "cloudflare"` and usually `source_ip = "cloudflare-proxy"` |
| Another proxy sits in front of Tako    | Configure trusted CIDRs at the server layer | `source_ip = "trusted-proxy"`                                     |

Certificate choice is related, but not the same decision. If users can reach the VPS directly, use a public certificate path such as Let's Encrypt. If Cloudflare is intentionally the only browser-facing entry point, Cloudflare Origin CA can be the right origin certificate. The tradeoffs are covered in [Cloudflare Origin CA vs Let's Encrypt for Self-Hosted HTTPS on a VPS](/blog/cloudflare-origin-ca-vs-lets-encrypt-vps-https/), and the deployment flow lives in the [deployment docs](/docs/deployment/).

After deploy, test both paths:

```bash
curl -I https://www.example.com
curl -I --resolve www.example.com:443:203.0.113.10 https://www.example.com
```

The first command goes through normal DNS. The second pins the hostname to the origin IP so you can see whether direct origin access still works. If the route is `cloudflare-proxy`, a direct request should not be treated as trusted Cloudflare traffic. If the route is `direct`, the origin path should work because that is the point.

Cloudflare is very good at being the front door. Tako's job is to make sure your VPS remembers who walked through it.
