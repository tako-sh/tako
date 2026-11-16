# Backlog

## E2E

- **Docker layer caching** — Server container builds (apt-get) are not cached between CI runs. `docker compose build` doesn't support BuildKit cache env vars. Would need `docker buildx bake` or `compose.yml` cache directives.

## Networks

- **WireGuard over the management HTTPS port** — For networks that block UDP, `tako ssh` could carry WireGuard datagrams over a WebSocket on the server's pinned management port, and the server could hand them to its local WireGuard socket. WireGuard keeps authenticating peers, and the server's firewall and `from=` SSH key restrictions still see the device's private address. The management path currently buffers each request and response through the TLS front end and primary relay, with no upgrade or streaming support, so this needs a new streaming route and end-to-end coverage. Do it only once users actually hit UDP-blocked networks; `tako ssh` and `tako doctor` now report that case.
- **Not planned: relaying through a public server for servers behind NAT.** Deploys, status, and logs reach each server over its own public HTTPS endpoint, so a server behind NAT is unmanageable regardless of SSH. Relaying SSH alone would not make such servers usable.
- **Not planned: network SSH to administrator accounts (`root`).** The agent withdraws all network SSH grants whenever it is unsure of membership, and network SSH stops with the agent, so it cannot replace break-glass access. It would also turn a stolen administrator laptop key into unrestricted root on every opted-in server. Keep separate provider-console or administrator SSH access for recovery.
