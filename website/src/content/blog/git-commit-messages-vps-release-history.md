---
title: "Git Commit Messages in VPS Release History: Tako Deploy Descriptions"
date: "2026-11-02T03:17"
description: "Tako release descriptions bring Git commit subjects, dirty checkout markers, and custom labels to VPS deploy history and rollback decisions."
image: 1dadea0f2f71
imageAlt: "A coral octopus matches a release description card to an application package in a server archive."
---

A release ID tells you which build reached your VPS. Remembering what that build changed is another job. During a broken checkout flow, a list of hashes sends you back to Git exactly when you'd rather be reading the app's logs.

Tako now gives releases a description alongside their version. Clean checkouts use the latest commit subject. Uncommitted source gets an explicit snapshot description. You can supply your own label when the release needs more context. That context travels with the release and appears when you inspect its history.

Here's how to use it for ordinary deploys, staging experiments, and choosing a recovery target.

## Read commit-aware release descriptions

From an existing Tako app, inspect its production releases:

```bash
tako releases list --env production
```

Run this from the directory containing the app's `tako.toml`, or select its configuration with `-c`. The [operations guide](/docs/operations/) covers the surrounding status and log commands. The environment matters: staging and production have separate deployed app identities, so select the one you mean to inspect.

Each listed release has a version, deployment time, current marker when applicable, and a description line. That description defaults to the source context available when Tako packages the release:

| Source state | Default description | Cleanliness shown in history |
| --- | --- | --- |
| Clean Git checkout | Latest commit subject | `clean` |
| Git checkout with uncommitted changes | `Uncommitted changes based on: <commit subject>` | `dirty` |
| Source without an available Git commit | `Local source snapshot` | `unknown` when Git state is unavailable |

Suppose the latest commit subject is `fix: reconnect live dashboard`. A clean deployment's description line reads:

```text
fix: reconnect live dashboard [clean]
```

If you make local changes before deploying, the default becomes:

```text
Uncommitted changes based on: fix: reconnect live dashboard [dirty]
```

That wording carries useful information. The commit subject describes the base commit. The deployed source includes changes beyond it. When someone asks whether the server runs the exact committed fix, the dirty marker gives you a reason to inspect the snapshot more closely.

The release version remains the identifier used by deployment and rollback. The [deployment guide](/docs/deployment/#build-and-artifact-rules) explains its source-based naming: a clean checkout uses the commit hash, a dirty checkout adds a content hash, and source without a commit uses a `nogit_` version with a content hash. A readable description supplements that identifier.

This works for native and container releases. Both package the description and source metadata in their release manifest. You don't need to change the application handler, add a runtime logger, or teach each framework how to report its Git history. The metadata belongs to the release that Tako prepares.

For scripts and tooling, request structured history:

```bash
tako releases list --env production --json
```

The response includes the selected app and environment plus a `releases` array. Release entries expose `version`, `description`, `commit_message`, `git_dirty`, `current`, and `deployed_at_unix_secs`. Keep the version when recording a deployment reference; descriptions may be reused, and timestamps alone are an awkward way to choose code.

## Add a description when a commit subject needs context

The default is enough for a straightforward deploy. You already wrote a commit subject, so Tako uses it:

```bash
tako deploy --env production
```

When the deployment has an operational reason that the commit subject doesn't capture, pass `--description`:

```bash
tako deploy --env production --description "Fix reconnect handling"
```

For example, one commit might support a staging exercise before the production rollout. A short staging description can record the behavior you're asking someone to verify:

```bash
tako deploy --env staging --description "QA: reconnect after laptop sleep"
```

This assumes your app already declares a staging environment and maps it to the intended servers. Follow [environment configuration](/docs/deployment/#configure-an-environment) to set that up. A description labels the release; the `--env` flag selects its deployment target.

Custom descriptions replace the default description text. The underlying commit subject and dirty state remain separate metadata, and the terminal still shows the cleanliness marker. Deploying a dirty tree with the custom label above would display:

```text
QA: reconnect after laptop sleep [dirty]
```

The label doesn't turn an uncommitted snapshot into a clean checkout. It explains why you deployed it. If you want the release to correspond to committed source, commit the intended changes and deploy from that source state.

Description values are trimmed, and blank values are rejected. Write them like a short note for your future self: the affected behavior, the reason for the rollout, or the verification someone should perform. Since they appear in release history, keep credentials and private customer details out of them.

| Situation | Useful description |
| --- | --- |
| A production behavior fix | `Fix reconnect handling` |
| A focused staging check | `QA: reconnect after laptop sleep` |
| A deliberate local snapshot | `Test empty dashboard recovery` |

The implementation keeps description changes in the artifact cache identity too. Repackaging the same source version with a different description doesn't accidentally reuse an archive containing the previous label. This is covered by the [manifest and cache tests](https://github.com/tako-sh/tako/blob/master/tako/src/commands/deploy/cache.rs).

```d2
direction: right

source: "Source checkout\ncommit subject + dirty state" {
  style.fill: "#FFF9F4"
}
package: "Release manifest\nversion + description" {
  style.fill: "#9BC4B6"
}
history: "Release history\ninspect and select version" {
  style.fill: "#E88783"
}

source -> package: "tako deploy"
package -> history: "stored on server"
```

That small trail is the feature: source context becomes something you can read beside the deployed build.

## Use release history during recovery

Suppose the new dashboard passes startup checks but fails when a user reconnects. Begin with the deployed state and recent evidence:

```bash
tako status --json
tako releases list --env production --json
tako logs --env production --days 1 --json
```

Read the command results, including failures and log truncation. Match the failing behavior to the current release and examine the candidates. Descriptions make that search easier, but the previous entry isn't automatically a known good version. Choose one whose behavior and data compatibility you understand.

Once you've identified the actual release ID to restore, use it in the rollback command documented in the [CLI reference](/docs/cli/):

```bash
tako releases rollback <release-id> --env production
```

Replace `<release-id>` with the version from your history. Interactive production rollbacks ask for confirmation. After it completes, inspect history and status again, then repeat the application request that originally failed. A successful command needs the same practical follow-up as a successful deploy: check the behavior through the real route.

On multiple servers, the release listing merges versions across the mapped hosts. Its current marker can be set when a version is current on any responding server. It doesn't establish that every server runs the same version, or that every host answered successfully. Read warnings and inspect server state when reconciling a partial rollout.

Release history also has retention boundaries. After a successful deployment, Tako keeps the active release and prunes non-active releases older than 30 days or beyond 50 total releases. Files still referenced by Compose containers remain protected until those containers are replaced or removed. Treat the list as the releases available for operations, rather than a permanent deployment audit archive.

Finally, rollback restores release code. Database changes and persistent files need their own recovery plan; [backups](/docs/backups/) cover app data, and [release commands](/blog/the-release-command-database-migrations-during-deploy/) cover deploy-time migrations. Check schema compatibility before selecting older code.

The next time you deploy, start with the default description. Add a short label when the rollout has a reason worth remembering. Your release ID stays precise, and the history gains enough context to help when you need it.
