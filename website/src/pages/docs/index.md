---
layout: ../../layouts/DocsLayout.astro
title: Self-hosted app deployment docs - Tako Docs
heading: Intro
current: intro
description: "Docs for running apps on your own servers with Tako: local HTTPS development, production deploys, routing, TLS, logs, secrets, and more."
---

# Intro

Tako is an opinionated development and deployment tool for self-hosted apps. It focuses on fast defaults for local HTTPS development and remote deploys, with routes, secrets, and logs, without a giant matrix of knobs.

## The Why

Tako started from one simple question: why did deploying become so dramatic?

The mission is simple: bring back the old <span class="dynamic-phrase">upload and go</span> energy, but with modern safety rails.

- Ship changes quickly.
- See results fast.
- Keep your flow instead of fighting platform glue.

Tako is built to make local development smooth and production deploys boring (the good kind of boring).

## What Tako Does Well

- Rolling deploys with health-based traffic shifts, no babysitting required.
- Zero-downtime server updates — one command and tako handles the handoff.
- Built-in load balancer. Scales down to `0`, scales up as far as you need.
- Was it `3000`? `5000`? Or `8081`? With Tako, local setup is portless on `https://*.test` (<a href="https://www.rfc-editor.org/rfc/rfc6761#section-6.2" target="_blank" rel="noopener noreferrer">RFC 6761</a>).
- Remote production routes are HTTPS by default (HTTP redirects to HTTPS).
- Subdomains? Custom path routes? Done.
- Serves static files from your app's `public` folder.
- Serves libvips-backed public optimized image URLs and signed object storage URLs.
- Secrets and variables per environment. Scoped and ready.
- Runtime status and log inspection via CLI.

> Already enjoying Tako? Show it some love - drop a star on <a href="https://github.com/lilienblum/tako" target="_blank" rel="noopener noreferrer">GitHub</a>.

## Who Tako Is For

- Builders and entrepreneurs who want predictable pricing and predictable performance.
- Teams that want shipping to feel boring and reliable, not risky and ceremonial.
- Teams that are done with surprise invoices and random "how is this `$46,485.99`?" moments.
- People who want a runtime they control, without arbitrary platform limits.
- Folks running lots of low-traffic apps: instances can scale to `0` and start on demand.
- Yes, even "a ton of apps on a tiny VPS" territory, if most of them are idle most of the time.
- Anyone tired of bloated tools and config files that feel like a second full-time job.

## Tech

- Built with Rust to be fast, reliable, and memory-safe.
- Minimal resource footprint is a core principle.
- Built on [Pingora](https://blog.cloudflare.com/how-we-built-pingora-the-proxy-that-connects-cloudflare-to-the-internet/), Cloudflare's Rust proxy library (Apache-2.0) that powers Cloudflare and is known for high performance.

## Ok, So Where Do I Sign?

Easy. Start here:

- [Local setup](/docs/quickstart/#local-setup)
- [Remote setup](/docs/quickstart/#remote-setup)
