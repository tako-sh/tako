# Tako native networks

- [x] Read poteto-mode Principles and repository instructions.
- [x] Phase A: Frame. Record the current implementation, risks, and acceptance criteria.
- [x] Phase B: Design the workflow. Compare membership designs and define module boundaries.
- [x] Phase C: Implement the agreed modules.
  - [x] Validate replicated metadata, permissions, and irrevocable device revocation.
  - [x] Add network identity, profiles, and per-device enrollment.
  - [x] Manage native WireGuard servers and client connections.
  - [x] Add scoped sandbox management over HTTPS.
  - [x] Add private service discovery and routing.
  - [x] Deliver channel messages across servers and recover replay gaps internally.
  - [x] Generate typed HTTP service contracts.
  - [x] Implement drain/removal and update installation and documentation.
  - [x] Record small commits on feat/native-networks and run focused offline checks.
- [x] Phase D: Keep the audit trail. Log decisions and verification limits.
- [ ] Phase E: Finish verification and publish one PR. Offline checks and independent source review are complete; live testing and publication remain blocked.

## Acceptance criteria

Two native servers join one stable network without an external VPN account. A new laptop can request, receive approval, import, connect, and SSH with its own keys. Revoked identities cannot rejoin or administer services. App/environment secrets stay separate from network identity. A scoped sandbox can deploy without WireGuard or SSH. Apps discover private services. Channels deliver across servers and recover missing replay through SDK control flow. PostgreSQL remains available for strong channel/workflow durability. Typed HTTP contracts work without a hosted registry. Planned removal drains work and reports unreachable peers honestly. CLI and end-to-end checks pass and an independent reviewer examines the changes.

## Work allocation

The parent owns core network identity, CLI profiles, integration, and test setup. Independent exploration covers server installation/authentication and channel/SDK contracts. Implementation delegates use separate worktrees with bounded write sets. Verification uses the actual CLI, isolated server processes/containers, and affected SDK suites. No production servers or user firewall are changed during development.

## Review method

Use inherited models with explicit high reasoning for delegates. Runtime policy permits model overrides only when the user asks, so cross-family diversity is unavailable. Record independent review separately from self-review. No additional approval checkpoint is needed because the user authorized the entire implementation in one PR with small commits.

## Current integration evidence

Implementation is integrated on the local branch. No push, pull request, merge, installation, or deployment has occurred.

The full CLI suite passed: 73 tests, 7 existing skips. The integrated CLI network suite passed 30 tests. The focused server suite passed 264 tests with one ignored test and four explicit exclusions for socket-dependent checks. It covers agent lifecycle and authority, discovery projections, management HTTP over in-memory transports, channel delivery/replay, request ownership, shared drain, persistence, promotion guards, and bounded preparation. Final CLI/server all-target compile checks and all 35 Compose unit tests passed. The final source passed the same 264 focused server cases. Full evidence and exclusions are recorded in native-network-verification.txt.

Independent source reviews covered the root helper, native management authorization/uploads, private routing, membership runtime, channel recovery, and process drain. Confirmed findings were fixed and regression tests added. These reviews used separate agents with the inherited model; no claim of cross-model review is made.

Live WireGuard/nftables/systemd/DNS traffic, real pinned-TLS deployment, PostgreSQL integration, and two-server/reload/promotion scenarios remain unverified. The required Docker end-to-end suite was attempted and blocked by EPERM on its Unix socket. Website source/type/link checks passed; the static build and rendered verifier were blocked by an esbuild write restriction and missing dist output. Go checks were unavailable because the toolchain was not usable.

Previous permission-gated Docker/publication attempts were aborted. Do not repeat those approval requests or equate offline tests with live verification. User authorization remains in force; the remaining limitation is execution capability, not missing task approval.
