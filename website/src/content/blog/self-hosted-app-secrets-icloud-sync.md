---
title: "Encrypted App Secrets in Git, Keys in iCloud"
date: "2026-05-07T04:55"
description: "Tako lets you commit encrypted app secrets to git while decryption keys stay local, sync through iCloud Keychain, or move by export/import."
image: 16daaacdbe32
---

Secrets are easy until a second laptop shows up.

One machine has the production database URL. Another needs to deploy. CI needs the same key for release builds. A teammate joins and asks where the Stripe token lives. The usual answer is a `.env` file, a password manager note, a Slack message you promise to delete, or a vault service that is technically correct and operationally one more thing.

We already wrote about [why Tako does not inject secrets through `.env` files](/blog/secrets-without-env-files). That post covered the runtime side: AES-256-GCM at rest, fd 3 at spawn time, and typed accessors from `tako generate`.

This is the other half: sharing the keys that decrypt those secrets. Tako secrets now have stable per-environment key IDs, self-contained key export/import, passphrase-derived keys, and optional iCloud Keychain storage on macOS. The goal is boring on purpose: encrypted project state can live in git, while the key material follows the people and machines that are allowed to use it.

## The file is portable. The key is not.

When you run [`tako secrets set`](/docs/cli), Tako writes encrypted values to `.tako/secrets.json`. That file is meant to be tracked. `tako init` updates `.gitignore` so the app's `.tako/` directory stays ignored while `.tako/secrets.json` remains visible to git.

The file looks like this:

```json
{
  "production": {
    "key_id": "0123456789abcdef",
    "secrets": {
      "DATABASE_URL": "base64(nonce + ciphertext + tag)",
      "STRIPE_KEY": "base64(nonce + ciphertext + tag)"
    }
  }
}
```

Secret names are plaintext so `tako secrets ls` can show a useful table without decrypting values. Secret values are encrypted with AES-256-GCM. Each environment has a `key_id`, which is the small but important rebuild: every machine can agree that production uses key `0123456789abcdef`, without storing the key itself in the repo.

The key lives outside the project:

| Piece                | Where it lives                   | Safe to commit? | What it contains                               |
| -------------------- | -------------------------------- | --------------- | ---------------------------------------------- |
| `.tako/secrets.json` | Project repo                     | Yes             | Environment names, key IDs, encrypted values   |
| Local key file       | Tako data dir, `keys/{key_id}`   | No              | Raw environment key                            |
| iCloud Keychain item | macOS Keychain                   | No              | Raw environment key, synchronizable            |
| Exported key bundle  | Clipboard / out-of-band transfer | No              | Base64url JSON with `version`, `id`, and `key` |

That split is what makes the workflow practical. The encrypted file can move through git like any other project file. The key can move through a different channel, or not move at all if the machine should never decrypt production.

```d2
direction: right

repo: Git repo {
  shape: rectangle
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

secrets: ".tako/secrets.json" {
  style.fill: "#9BC4B6"
}

keyid: "key_id" {
  shape: circle
  style.fill: "#E88783"
}

store: "local file or iCloud Keychain" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

value: "decrypted values" {
  shape: hexagon
  style.fill: "#9BC4B6"
}

repo -> secrets: "git pull"
secrets -> keyid: "points at"
keyid -> store: "load key"
store -> value: "decrypt"
```

## Sharing is a key operation, not a file copy.

For a new teammate, the flow is intentionally small:

```bash
# Person who already has the key
tako secrets key export --env production

# New machine
tako secrets key import
```

`tako secrets key export` reads the cached key for the selected environment, requires macOS user authentication on macOS, and copies one self-contained key bundle to the clipboard. The bundle includes the key ID, so import does not need `--env` when you are importing an exported key. Tako can look at the current project's `.tako/secrets.json`, match the ID, and report the environment name if it finds one.

There is also a passphrase path:

```bash
tako secrets key import --passphrase --env production
```

That derives the environment key from the passphrase and the environment key ID. It is useful when a tiny team wants a memorized shared secret instead of passing a random key bundle around. If the environment does not have a key ID yet, Tako creates one before saving `.tako/secrets.json`.

Both paths validate before they trust the key. If the project already has encrypted secrets for that environment, Tako tries to decrypt them with the imported key. Wrong key, wrong passphrase, or corrupted payload: no silent success.

For CI, the shape is the same. Import the production key into the runner's Tako data directory before the step that needs to decrypt or sync secrets. The repo contains encrypted values; the runner secret store contains the exported key bundle or passphrase. Tako keeps the two concerns separate.

## On macOS, keys can follow your Macs.

Local key files are still the default because they work everywhere. On macOS, interactive key creation and key import now ask:

```text
Use iCloud Keychain?
```

Say yes, and Tako stores the environment key in a synchronizable Keychain item. Pull the repo on another Mac signed into the same iCloud account, and the key can be read from Keychain instead of from `keys/{key_id}`.

The technical detail is that the macOS CLI is now packaged as a real app bundle, not just a loose executable. The release packaging step copies the Rust binary into `Tako.app/Contents/MacOS/tako`, writes an app `Info.plist`, and signs the bundle with Keychain access-group entitlements. The installer puts `Tako.app` in your Applications directory and symlinks `tako` to the signed executable inside the bundle. From the terminal, it still feels like a normal CLI command; to macOS, it is a signed app allowed to use the protected, synchronizable Keychain.

At runtime, Tako asks Keychain for a protected generic password item named by the environment key ID and marks it synchronizable. There is no helper daemon, background socket, or side channel. The same process that runs `tako secrets set` or `tako secrets key import` writes the key.

If the entitlement is unavailable, Tako fails before changing project state:

```text
iCloud Keychain requires the signed Tako app. Reinstall Tako and try again.
```

That failure mode is deliberate. If you asked for iCloud storage, Tako should not quietly fall back to a local key file and leave you thinking the key will sync.

This gives macOS users a nicer day-to-day loop without making Keychain mandatory. Linux servers, CI runners, and teammates on other platforms can keep using local key files and key import.

## Rotation does not need a redeploy.

Key sharing gets the secret onto the right machines. Sync gets it onto the right servers.

```bash
tako secrets set STRIPE_KEY --env production --sync
```

or:

```bash
tako secrets set STRIPE_KEY --env production
tako secrets sync --env production
```

[`tako secrets sync`](/docs/cli) treats the local `.tako/secrets.json` file as the source of truth. For each target environment, Tako decrypts locally using the cached key from iCloud Keychain or `keys/{key_id}`, then sends an `update_secrets` command to `tako-server`.

The server does not write a remote `.env` file. It stores secrets encrypted in SQLite using a per-device key. Fresh app instances and workflow workers receive secrets through the same fd 3 bootstrap envelope described in [the deployment docs](/docs/deployment). HTTP instances roll. Workflow workers drain and restart. New processes see the new value; old processes finish what they were already doing.

Deploys use the same model. During [`tako deploy`](/blog/what-happens-when-you-run-tako-deploy), the CLI asks each server for the app's current secrets hash. If the hash matches, it skips the secrets payload entirely. If the server is new or stale, the deploy includes decrypted secrets and the server stores the update.

That is the whole shape:

| Task           | Command                                            | Result                                                |
| -------------- | -------------------------------------------------- | ----------------------------------------------------- |
| Add a secret   | `tako secrets set API_KEY --env production`        | Encrypts into `.tako/secrets.json`                    |
| Share access   | `tako secrets key export --env production`         | Copies a key bundle for that environment              |
| Join a machine | `tako secrets key import`                          | Caches the imported key locally or in iCloud Keychain |
| Rotate live    | `tako secrets set API_KEY --env production --sync` | Updates servers and rolls fresh processes             |
| Audit names    | `tako secrets ls`                                  | Shows presence across environments, never values      |

## Small enough to use, strict enough to trust.

This is still not a replacement for every vault. Big organizations have approval flows, audit systems, HSMs, break-glass policies, and compliance boxes to check. Those tools are real for a reason.

Tako is aiming at the very common middle: teams deploying apps to their own servers who want something better than `.env`, but do not want secret management to become the largest system in the room. A tracked encrypted file, stable environment key IDs, explicit key import/export, passphrases when you want them, iCloud Keychain when your Mac can use it, and fd 3 when the process starts.

Secrets are one of the platform pieces Tako keeps close to the app: routing, TLS, deploys, logs, local dev, workflows, and now a key-sharing path that does not require pretending a plaintext file in `.gitignore` is infrastructure. Start with the [CLI reference](/docs/cli), skim [how Tako works](/docs/how-tako-works), or read the original [Secrets Without `.env` Files](/blog/secrets-without-env-files) if you want the runtime half of the story.
