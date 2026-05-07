---
title: "LAN Mode: Hand Your App to a Phone in Three Seconds"
date: "2026-04-13T06:33"
description: "Press l in tako dev and your app is reachable from any phone or tablet on your Wi-Fi as myapp.local — real HTTPS, no ngrok, no port forwarding."
image: 75e8f994d6cd
---

You're building a responsive layout. You want to see how it looks on your actual phone, not Chrome's device emulator. The usual options are all bad: type your laptop's IP and a port number into your phone's browser, fight with self-signed cert warnings, or fire up an ngrok tunnel that broadcasts your half-built app to the public internet just so it can come back to a device three feet away.

There's a better way. Press `l` in [`tako dev`](/docs/development).

## What happens when you press `l`

Tako detects your LAN IP, binds the dev proxy to `0.0.0.0:443`, and publishes an mDNS record for every registered `.test` route as a matching `.local` hostname. Your phone — already a fluent mDNS client thanks to Bonjour — resolves `myapp.local` and connects over real HTTPS.

It also prints a QR code in your terminal pointing at `http://<lan-ip>/ca.pem`. Scan it on your phone, install the Tako development CA as a configuration profile, and the green padlock shows up on every `.local` hostname you serve, forever. One setup, every project.

```bash
$ tako dev
  ✓ dev daemon running
  ✓ routes ready

  https://myapp.test/

  l LAN mode · r restart · b background · ctrl+c stop

# press l

  ✓ LAN mode enabled

  https://myapp.local/   ← reachable from any device on your Wi-Fi

  ▄▄▄▄▄▄▄ ▄ ▄▄ ▄ ▄▄▄▄▄▄▄
  █ ███ █ █▀▀█▄ █ ███ █     scan to trust the dev CA
  █▄▄▄▄▄█ ▄▀█ ▄ █▄▄▄▄▄█     http://192.168.1.42/ca.pem
```

Press `l` again to turn it off. mDNS records vanish, the proxy goes back to loopback-only, and your laptop is no longer announcing anything to the network.

## Why not ngrok?

Ngrok and similar tunnels solve a different problem — making your laptop reachable from the public internet. For real-device testing on the same Wi-Fi network, that's the wrong tool. You don't need a public URL, a third-party relay, or a rotating subdomain that breaks every OAuth callback you configure. You need your phone, three feet away, to talk to your laptop directly.

| Approach              | Setup                             | URL on the phone           | HTTPS               | Public exposure |
| --------------------- | --------------------------------- | -------------------------- | ------------------- | --------------- |
| IP + port             | Find IP, type it, allow firewall  | `http://192.168.1.42:3000` | No                  | LAN only        |
| ngrok / Cloudflare    | Account, daemon, auth token       | `https://abcd.ngrok.app`   | Yes                 | Public internet |
| `mkcert` + manual DNS | Edit `/etc/hosts` on every device | `https://myapp.test`       | If you trust the CA | LAN only        |
| **Tako LAN mode**     | Press `l`                         | `https://myapp.local`      | Yes (one-tap CA)    | LAN only        |

## How it works under the hood

```d2
direction: right

phone: Phone {shape: rectangle; style.fill: "#9BC4B6"}
mdns: mDNS / Bonjour {shape: circle; style.fill: "#FFF9F4"; style.stroke: "#2F2A44"}
proxy: Tako Dev Proxy {style.fill: "#E88783"}
app: Your App {shape: hexagon; style.fill: "#FFF9F4"; style.stroke: "#2F2A44"}

phone -> mdns: "who has myapp.local?"
mdns -> phone: "192.168.1.42"
phone -> proxy: "GET https://myapp.local/"
proxy -> app: routed by Host header
```

When you toggle LAN mode, the dev server spawns one mDNS publisher per registered route — `dns-sd` on macOS, `avahi-publish-address` on Linux. Each publisher advertises one concrete hostname. The dev proxy starts listening on `0.0.0.0:443` and routes by `Host` header, the same way [the production Tako proxy](/blog/pingora-vs-caddy-vs-traefik) does.

One footnote: mDNS only knows how to advertise concrete records, so wildcard routes like `*.app.test` don't translate. Tako warns you about this and suggests adding an explicit subdomain. Everything else — subpaths, multiple apps, multiple terminals — just works.

## Try it

If you've got `tako` installed, you already have LAN mode. Run `tako dev` in any project, press `l`, scan the QR code on your phone, and open `https://<your-app>.local` in mobile Safari. Your responsive breakpoints, your touch targets, your iOS-only Safari quirks — all live, all over real HTTPS, all in about three seconds.

New here? `brew install takoserver/tap/tako` and check out the [development docs](/docs/development) or the [CLI reference](/docs/cli) for everything `tako dev` can do beyond this one keystroke.
