---
title: "How to Use Cloudflare Tunnel with Vite Local HTTPS Dev"
date: "2026-05-06T07:42"
description: "Expose a Vite app from tako dev through a stable Cloudflare Tunnel hostname without opening Vite's allowedHosts to the world."
image: 9f0f8a4dfb9c
---

Sometimes `.test` is exactly right. You want a local HTTPS hostname, secure cookies, service workers, OAuth callbacks, and no port juggling, so you run [`tako dev`](/docs/development) and open `https://my-app.test/`.

Sometimes the app has to leave your laptop.

Maybe a webhook provider needs to call you back. Maybe a teammate needs to try a branch before you deploy it. Maybe an OAuth provider insists on a real public domain. You can solve that with [Cloudflare Tunnel](https://developers.cloudflare.com/tunnel/setup/): `cloudflared` keeps an outbound connection to Cloudflare, and Cloudflare forwards a public hostname back to a service on your machine.

The trap is that Vite quite reasonably does not want to answer for every hostname on the internet. The tempting fix is `server.allowedHosts = true`, but the [Vite docs](https://vite.dev/config/server-options.html#server-allowedhosts) call that out as unsafe because it opens the dev server to DNS rebinding attacks. The better fix is to make the tunnel hostname a real development route in Tako, then let `tako.sh/vite` add only that hostname to Vite's allowed list.

## The Shape We Want

The browser should hit a real public HTTPS URL. Cloudflare should tunnel that request to the local Tako dev proxy. Tako should route by `Host` header, terminate local HTTPS, and forward to Vite on a loopback port. Vite should accept the request because the hostname is one of the configured dev routes, not because every host is allowed.

```d2
direction: right

browser: Browser {
  shape: rectangle
}

cloudflare: Cloudflare Tunnel {
  shape: cloud
}

tako: "tako dev proxy" {
  style.fill: "#E88783"
}

vite: "Vite dev server" {
  shape: hexagon
}

routes: "development routes" {
  style.fill: "#FFF9F4"
}

browser -> cloudflare: "https://dev.example.com"
cloudflare -> tako: "https://127.0.0.1:47831\nHost: dev.example.com"
tako -> routes: "match host"
tako -> vite: "loopback request"
routes -> vite: "allowed host"
```

That last arrow is the whole point. The tunneled hostname is explicit application config, so the Vite dev server stays picky.

| Layer              | Hostname it sees  | What accepts it                                                       |
| ------------------ | ----------------- | --------------------------------------------------------------------- |
| Browser            | `dev.example.com` | Cloudflare's public certificate and DNS route                         |
| `cloudflared`      | `dev.example.com` | The tunnel ingress rule                                               |
| Tako dev proxy     | `dev.example.com` | `[envs.development].routes` in [`tako.toml`](/docs/tako-toml)         |
| Vite dev server    | `dev.example.com` | `tako.sh/vite` adds the route host to `server.allowedHosts`           |
| Local fallback URL | `my-app.test`     | Tako-managed local DNS and HTTPS from [`tako dev`](/docs/development) |

This is different from binding Vite to `0.0.0.0` or telling it to trust every host. Vite still listens on loopback. The public edge talks to Tako, not straight to Vite.

## Configure Tako and Vite

Start with the Vite plugin. In `vite.config.ts`, add `tako()` alongside your framework plugins:

```ts
import { defineConfig } from "vite";
import { tako } from "tako.sh/vite";

export default defineConfig({
  plugins: [tako()],
});
```

During `vite dev`, the plugin adds `.test`, `.tako.test`, and the configured Tako dev route hostnames to Vite's `server.allowedHosts`. Under `tako dev`, it also binds Vite to `127.0.0.1` and reports the chosen port back to the dev daemon, which is why Tako does not need to scrape Vite's stdout for a URL.

Now give Tako the public hostname. Routes are host patterns, not URLs, so leave off `https://`:

```toml
name = "my-app"
preset = "vite"

[envs.development]
routes = ["my-app.test", "dev.example.com"]
```

The `.test` route is local and managed by Tako. The `dev.example.com` route is external: Tako will route it if traffic reaches the dev proxy, but it will not create DNS for it, advertise it in LAN mode, or rewrite it to `.local`. That split is intentional. [`LAN mode`](/blog/lan-mode-hand-your-app-to-a-phone) is for devices on your Wi-Fi; Cloudflare Tunnel is for traffic from the public internet.

You can also list only the external route:

```toml
[envs.development]
route = "dev.example.com"
```

When development routes contain no managed `.test` or `.tako.test` route, Tako keeps the default `my-app.test` route alongside the external hostname. We like the explicit two-route version in tutorials because it makes the shape visible, but the shorter version works.

Run the app:

```bash
tako dev
```

At this point `https://my-app.test/` should work locally. If it does not, fix that first. The [development docs](/docs/development), [CLI reference](/docs/cli), and [`tako.toml` reference](/docs/tako-toml) cover the local daemon, TLS trust, and route syntax.

## Configure Cloudflare Tunnel

Use a named tunnel with a stable hostname. Quick tunnels are handy for experiments, but their random hostnames are awkward here because the hostname needs to be in `tako.toml` before `tako dev` starts.

Create the tunnel and publish the DNS route:

```bash
cloudflared tunnel login
cloudflared tunnel create tako-dev
cloudflared tunnel route dns tako-dev dev.example.com
```

Then create a local `cloudflared` config file. The important values are the service URL, the host header, and the local TLS settings:

```yaml
tunnel: <tunnel-id>
credentials-file: /Users/you/.cloudflared/<tunnel-id>.json

ingress:
  - hostname: dev.example.com
    service: https://127.0.0.1:47831
    originRequest:
      httpHostHeader: dev.example.com
      matchSNItoHost: true
      noTLSVerify: true
  - service: http_status:404
```

Tako's HTTPS dev daemon listens on `127.0.0.1:47831`. On macOS and Linux, `tako dev` also arranges portless local URLs on `:443`, but the tunnel does not need that outer helper. It can talk directly to the daemon's fixed HTTPS port.

`httpHostHeader` makes the request arrive at Tako as `dev.example.com`, which is what the route matcher needs. `matchSNItoHost` makes the TLS handshake use the incoming hostname as SNI; that lines up with Tako's local certificate selection. `noTLSVerify` is there because the origin certificate is signed by your local Tako development CA, not by a CA Cloudflare already trusts. Keep that setting scoped to this local dev tunnel. For a real deployed origin, use normal TLS verification.

Cloudflare's [configuration file docs](https://developers.cloudflare.com/tunnel/advanced/local-management/configuration-file/) also show how ingress rules are matched and why the final catch-all rule is required. The [origin parameters docs](https://developers.cloudflare.com/tunnel/advanced/origin-parameters/) cover `httpHostHeader`, `matchSNItoHost`, and `noTLSVerify` in more detail.

Run the tunnel in another terminal:

```bash
cloudflared tunnel --config ~/.cloudflared/tako-dev.yml run tako-dev
```

Now open:

```text
https://dev.example.com/
```

The public URL should hit the same app you see at `https://my-app.test/`, but without changing Vite to accept arbitrary hosts.

## Debug the Hop That Fails

Tunnel setups are three small systems in a trench coat: Cloudflare DNS, `cloudflared`, and your local dev proxy. When something is off, the error usually tells you which hop to inspect.

| Symptom                              | Likely cause                                                                           |
| ------------------------------------ | -------------------------------------------------------------------------------------- |
| Cloudflare `502`                     | `tako dev` is not running, or `service` points at the wrong local port                 |
| Tako `421 Misdirected Request`       | `dev.example.com` is missing from `[envs.development]`, or the Host header is wrong    |
| Vite "Blocked request"               | `tako.sh/vite` is missing, or the app was started outside `tako dev`                   |
| `my-app.test` works, tunnel does not | Check `cloudflared tunnel ingress validate` and the DNS route                          |
| Tunnel works, HMR is odd             | Make sure the tunnel proxies WebSocket traffic; Cloudflare Tunnel does for HTTP routes |

For a direct local probe, this is useful:

```bash
curl -k -H "Host: dev.example.com" https://127.0.0.1:47831/
```

If that returns your app, Tako is configured correctly and the problem is on the Cloudflare side. If that returns `421`, Tako is receiving the request but does not have the route. If Vite blocks it, the request made it all the way through the tunnel and proxy, but the app's Vite config is missing the Tako plugin.

This setup is for development, not deployment. When you are ready to ship, use [`tako deploy`](/docs/deployment) and give the production environment its own public route. The neat part is that the local path and the production path share the same basic idea: routes are real config, the proxy routes by hostname, and the app does not need to know whether the request started three inches away or somewhere on the internet.
