---
title: "The Open Source ngrok Alternative for Local HTTPS Dev Tunnels"
date: "2026-06-21T15:02"
description: "Use tako dev --tunnel for public HTTPS dev URLs with stable hostnames, signed identity, reconnects, and no separate tunnel daemon."
image: 675b059cfc16
---

There are two moments when local development suddenly needs a public URL.

The first is practical: a webhook provider, OAuth callback, teammate, or phone on another network needs to reach the app on your laptop. The second is emotional: you realize the clean `https://my-app.test/` setup you had five minutes ago is about to become three terminals, a random tunnel URL, and a sticky note about callback URLs.

That second moment is why open-source [Tako](https://github.com/tako-sh/tako) has [`tako dev --tunnel`](/docs/development/): a public HTTPS URL for a running app without a separate tunnel daemon.

```bash
tako dev --tunnel
```

Or, if `tako dev` is already running, press `t`.

Tako creates a public URL like:

```text
https://my-app-k7q4z2.tako.website/
```

The URL is public, HTTPS, and stable for the same app name plus local Tako Identity. When the tunnel connection drops, Tako keeps the URL reserved, shows it as reconnecting, and retries automatically. You do not need a second process, a reserved subdomain, or a config file just to remember where your app is running.

## The Local Tunnel Problem

Tools like [ngrok](https://ngrok.com/), [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/), [localtunnel](https://github.com/localtunnel/localtunnel), [zrok](https://github.com/openziti/zrok), and [bore](https://github.com/ekzhang/bore) are useful. They all solve a real networking problem: your laptop is behind NAT, the internet cannot dial it directly, so an outbound connection has to meet a relay somewhere public.

The awkward part is that most tunnel tools sit beside your dev environment instead of inside it. Your app runs on one local port. Your framework has its own allowed-host rules. Your tunnel tool forwards to whatever port you remembered to type. If the public hostname changes, callbacks need updates. If the tunnel process dies, your app may still be fine locally, but everyone else sees a dead URL.

That shape is okay for one-off demos. It gets annoying when public dev URLs become part of your daily loop.

Tako already owns the local dev surface: the [dev daemon](/blog/the-dev-daemon-tako-dev-is-just-a-client/) starts the app, assigns the loopback port, terminates local HTTPS, tracks registered routes, streams logs, and keeps `https://my-app.test/` alive. Tunnel mode plugs into that same system instead of asking you to bolt another one on.

```d2
direction: right

browser: "teammate / webhook" {
  shape: rectangle
}

tunnel: "tako.website tunnel service" {
  shape: cloud
  style.fill: "#9BC4B6"
}

daemon: "tako-dev-server" {
  style.fill: "#E88783"
}

proxy: "local HTTPS proxy" {
  style.fill: "#FFF9F4"
}

app: "your app" {
  shape: hexagon
}

identity: "Tako Identity" {
  shape: cylinder
}

browser -> tunnel: "https://my-app-id.tako.website"
daemon -> tunnel: "signed outbound WebSocket"
identity -> daemon: "sign challenge"
tunnel -> daemon: "HTTP / WebSocket frames"
daemon -> proxy: "Host: my-app.test"
proxy -> app: "loopback port"
```

The public side speaks to `tako.website`. The local side stays routed through the same dev proxy that already serves your `.test` URL.

## What Makes Tako Different

The big difference is not that Tako has invented tunneling. It is that the tunnel knows about your app.

When you enable tunnel mode, the dev server loads or creates a local Tako Identity, asks the tunnel service for a challenge, signs that challenge, and opens an outbound WebSocket session. The public hostname includes the app name and an id derived from the app plus identity, so the same project on the same machine gets the same public URL when the identity is available.

On macOS, Tako tries to keep that identity in iCloud Keychain and falls back to local storage when synced Keychain access is unavailable. On other platforms, it uses local identity storage. Either way, the URL is tied to a local key, not a random string printed by a throwaway process.

That gives tunnel mode a few nice properties:

| Concern            | Separate tunnel CLI                            | `tako dev --tunnel`                                                |
| ------------------ | ---------------------------------------------- | ------------------------------------------------------------------ |
| Local app target   | Usually a port you pass by hand                | The app registered with the Tako dev daemon                        |
| Public URL         | Often random unless reserved/configured        | Stable for app name plus Tako Identity                             |
| HTTPS              | Public HTTPS from the tunnel provider          | Public HTTPS plus local HTTPS through Tako                         |
| Host routing       | You wire Host headers and framework allowlists | Tako routes through the existing dev route                         |
| Process model      | Extra daemon/process beside your app           | Built into the running `tako dev` session                          |
| Reconnect behavior | Depends on the tool and how it is supervised   | URL stays reserved while Tako reconnects                           |
| Local docs path    | Tool-specific setup                            | Same [`tako dev`](/docs/development/) and [`CLI`](/docs/cli/) flow |

The result feels smaller. You run the same command you were already running:

```bash
tako dev --tunnel
```

You can still use the local URL:

```text
https://my-app.test/
```

And now you also have the public one:

```text
https://my-app-k7q4z2.tako.website/
```

Both point at the same registered app. Both flow through the same route model described in the [`tako.toml` reference](/docs/tako-toml/). The public URL is just the internet-facing entrance.

## A Respectful Comparison

If you need a general-purpose tunnel, the standalone tools are still good tools. Ngrok has a mature hosted platform and domain controls. Cloudflare Tunnel is excellent when you already own the domain and want traffic on Cloudflare's network. zrok is a serious open-source sharing system built around zero-trust networking. localtunnel is simple for quick Node-flavored sharing. bore is tiny and TCP-shaped.

Tako is narrower on purpose: local HTTPS dev tunnels for apps already running under Tako.

| Tool              | Best fit                                                           | Tradeoff for local app dev                                                    |
| ----------------- | ------------------------------------------------------------------ | ----------------------------------------------------------------------------- |
| ngrok             | Polished hosted tunnels, traffic features, managed domains         | Another CLI and account surface outside your dev runtime                      |
| Cloudflare Tunnel | Stable custom hostnames on Cloudflare-controlled DNS               | Great but setup-heavy for quick branch previews                               |
| localtunnel       | Very quick open-source localhost sharing                           | Public hostname availability and HTTPS-to-local details are separate concerns |
| zrok              | Open-source sharing with private/public modes and zero-trust roots | More general than a framework-aware dev command                               |
| bore              | Small self-hostable TCP tunnels                                    | Lower-level than app-aware HTTPS routing                                      |
| Tako tunnel       | Public URL for a `tako dev` app                                    | Only useful when the app is running through Tako                              |

That last row is the point. We are not trying to replace every tunnel tool. We are trying to remove a whole category of tunnel setup from the path between "my app works locally" and "someone else can open it."

For webhook testing, this means you can give Stripe, GitHub, Customer.io, or any other callback sender a stable app-derived URL while your local app keeps running behind Tako. For OAuth, it means fewer "which callback URL is live today?" tabs. For teammate review, it means `tako dev --tunnel`, paste, keep coding.

## How It Behaves When Things Get Weird

Tunnel mode is deliberately boring during failure. That is a compliment.

If the tunnel connection drops, the URL does not disappear. Tako keeps tunnel mode enabled, marks the tunnel as reconnecting in the status panel, retries with bounded exponential backoff, and prints log lines when reconnecting starts and when the tunnel reconnects.

If you start another tunnel for the same app and identity, it replaces the previous active session for that URL. If one identity has too many active tunnel URLs at once, the service accepts the new one and closes the oldest active tunnel for that identity. The closed client turns tunnel mode off and tells you why.

If someone visits a tunnel URL while it is inactive or disconnected, they get a Tako-styled error page in the browser. API-ish clients that send `Accept: application/json` get JSON, and other clients get plain text.

That behavior matters because dev tunnels are usually used right when someone else is waiting. A webhook retry is hitting your endpoint. A teammate is refreshing a bug repro. An OAuth provider is redirecting back to you. The URL should be the stable thing, even if the laptop briefly changes Wi-Fi networks.

## Try It

Install Tako, run your app through [`tako dev`](/docs/development/), and turn on the tunnel:

```bash
brew install takoserver/tap/tako
cd your-project
tako dev --tunnel
```

Inside an existing interactive session, press `t` to toggle the tunnel. Run `tako dev list` to see registered apps and the current tunnel URL for any app with tunnel mode enabled.

For local-device testing on the same Wi-Fi, use [LAN mode](/blog/lan-mode-hand-your-app-to-a-phone/) instead. For a custom domain on Cloudflare, the manual [Cloudflare Tunnel setup](/blog/cloudflare-tunnel-vite-local-https-dev/) still works. For the common "I need a public HTTPS URL for this local Tako app right now" case, tunnel mode is the shorter path.

No port forwarding. No second tunnel daemon. No random URL musical chairs. Just your app, already running under Tako, with a public HTTPS door when you need one.
