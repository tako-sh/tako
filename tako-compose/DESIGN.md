# Compose service updates

Tako owns the deployment transaction and HTTP routing. Docker Compose continues
to validate the project and create regular containers, networks, and volumes.
Development continues to use ordinary `compose up`; production uses the service
update planner below.

## Invariants

- The project, networks, and named volumes belong to the app/environment, never
  to an HTTP instance or release. Retiring an HTTP instance cannot stop a stack.
- An immutable service generation is identified by the resolved container
  configuration, dependency conditions, and image runtime contents. Build paths,
  release tags, replica count, and update policy are not container configuration.
  Secrets participate in the fingerprint but are never written to generated files or deployment records.
- Each replica has a stable slot. An unchanged healthy container keeps its ID,
  endpoint, and data across deployment and server restart.
- The selected stateless HTTP service defaults to start-first. Other services
  and services with writable persistent volumes default to stop-first.
  `deploy.update_config.order` may explicitly allow or prevent overlap.
- New containers must pass Docker readiness and the selected service must pass
  Tako's HTTP readiness and stability checks before public traffic changes.
- The old HTTP instance drains only after the replacement is in routing.
- Rollback stops newly introduced containers and restarts stopped predecessors
  using their original dependency graph and readiness conditions;
  it never restores database contents. An unchanged dependency stays running.
- A durable pending marker precedes container changes. The active release,
  routes, and encrypted credentials commit in one SQLite transaction. Recovery
  reconciles the committed release, then removes abandoned generations.
- Cleanup uses exact owned container IDs and labels, never an interpolated
  failed Compose file. Referenced release files remain protected from pruning.

## Container creation

Image preparation uses temporary tags unique to each attempt, including when
the Compose file declares an image name for a build. It pins the inspected image
runtime contents before discarding those tags, so concurrent preparations cannot
exchange images. Creation pins the exact inspected image ID. The first successful
preparation of a release is published atomically and cannot be replaced by a
retry. Later preparations reuse it; changed sources or images require a new
release. The override contains only generated names and stays outside build
contexts, preserving identical-build caching and service identity.

Production renders a single resolved service under an internal generation name,
with its original network aliases and stable resource names. It passes this
model to `compose create` and `compose start` through stdin. Compose start also
injects environment-backed secrets and configs. This preserves normal Compose
restart/resource/health settings and allows the old and new generations to
coexist. Resolved models stay in memory; literal dollar signs must survive the second Compose parse.

Container labels record only app ownership, original service name, replica slot,
configuration fingerprint, dependency conditions, and source release directory.
They allow discovery after process termination without relying on an in-memory
process handle.

The planner evaluates dependency ordering and readiness conditions before
starting consumers. Jobs required to complete successfully are treated as jobs,
not long-running HTTP instances. The initial HTTP ingress has one replica;
workers may declare replicas and explicit start-first updates.

## Failure boundaries

| Interruption | Recovery |
| --- | --- |
| Before pending marker | No containers changed |
| After container creation, before readiness | Reconcile committed release; remove candidates |
| After stop-first predecessor stops | Restart matching committed generation before cleanup |
| After readiness, before durable commit | Previous release remains authoritative |
| After durable commit, before route switch/cleanup | New release is authoritative; adopt its healthy containers |
| First deployment interrupted before commit | Remove that pending project's containers; preserve volumes |

The availability contract covers Tako's HTTP ingress during compatible
start-first updates. Changes to dependencies, exclusive data access, incompatible
migrations, direct internal DNS clients, and connections exceeding the drain
deadline require separate application handling.

## Verification

Test planning independently from Docker: image/config identity, secret changes,
dependency cycles/conditions, replicas, update policy, and unsupported sharing.
Use real Docker tests for unchanged IDs, restart policies, aliases, volumes,
literal environment values, stop-first recovery, interrupted generations, and
per-container cleanup. Exercise the installed server with continuous HTTP
requests during successful and failed Bun updates, Redis identity/persistence,
secret rotation, rollback, and server restart. Run the required native deployment
and CLI regression suites after integrating the lifecycle changes.
