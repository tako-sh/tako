---
title: "An MCP Server for VPS Deployment: How Tako Connects Coding Agents to Your App"
date: "2026-10-25T04:27"
description: "Connect coding agents to your VPS app with Tako's bundled docs, workflow skills, and MCP tools for logs, diagnostics, and deployment."
image: 75dd3e175c3d
---

An agent can edit your app and still have no idea why the deployed route returns an error. It needs the right configuration, evidence from the running system, and instructions that match the tools installed on your machine. Otherwise, you become the clipboard between the editor and the terminal.

Tako now bundles [coding-agent support](/docs/agents/) into its CLI: documentation, workflow skills, and a local MCP server. Together, they give agents a way to investigate failures and operate deployments through the same Tako commands you use yourself. Here's how those pieces connect, and what a useful debugging session looks like.

## Set up the agent in the right project

From your app's directory, run:

```bash
tako agents
```

Tako detects Codex, Claude Code, Cursor, and VS Code Copilot from project or user configuration directories. It previews the project-local files and asks before writing. You can also select clients explicitly, or inspect the plan without changing anything:

```bash
tako agents --agent codex,claude-code --yes
tako agents --agent cursor --dry-run --json
```

Setup installs bundled skills, merges the Tako MCP entry into the selected clients' configuration, and adds a marked section to `AGENTS.md`. Claude Code also gets that guidance in `CLAUDE.md`. Unrelated settings are preserved. Review the generated files before committing them.

| Client | MCP configuration | Skill directory |
| --- | --- | --- |
| Codex | `.codex/config.toml` | `.agents/skills/` |
| Claude Code | `.mcp.json` | `.claude/skills/` |
| Cursor | `.cursor/mcp.json` | `.cursor/skills/` |
| VS Code Copilot | `.vscode/mcp.json` | `.github/skills/` |

Those paths are relative to the selected config's project directory. If your repository has several app configurations, select the intended one with `-c`. Setup records absolute paths to both the current executable and the selected configuration, so a different client working directory or `PATH` doesn't quietly choose another app.

Restart the coding agent or reload its MCP connections afterward. The client may require project trust or server enablement. Setup doesn't copy global agent settings or credentials; the MCP server uses the installed CLI's credentials and permissions.

The server itself is a local stdio process:

```bash
tako mcp -c /absolute/path/to/project/tako.toml
```

The coding client starts this process and exchanges MCP messages over standard input and output. Diagnostics go to stderr. There is no extra Tako login for MCP. Re-run `tako agents` after upgrading the CLI, or after moving the project or executable, to refresh the installed files and paths.

## Bundle the reference, workflow, and operations together

The three pieces answer different questions. Documentation explains configuration and APIs. Skills explain how to approach a task. MCP tools expose operations and return evidence from the configured project.

During the CLI build, Tako embeds the Markdown from `website/docs/` and its canonical SDK skills. An agent can read those documents without a network lookup. Their contents match the installed CLI build; upgrading Tako brings newer reference material.

`tako_docs` lists topics when called without arguments and reads a selected topic when given one. `tako_search_docs` performs a case-insensitive literal search and returns matching lines with topic names and line numbers. The agent can then read the full topic. For example, these are MCP `tools/call` parameter objects:

```json
{
  "name": "tako_search_docs",
  "arguments": { "query": "readiness" }
}
```

```json
{
  "name": "tako_docs",
  "arguments": { "topic": "docs/troubleshooting" }
}
```

The same content is available as resources such as `tako://docs/tako-toml` and `tako://skills/tako-sdk-js`. The general `tako` skill guides development, debugging, and deployment; the JavaScript, Go, and Rust skills supply language-specific SDK guidance. The live [documentation index](https://tako.sh/llms.txt) remains available when the agent needs an online entry point.

Operational tool schemas come from the CLI's command definitions. Command paths become names such as `tako_deploy` and `tako_releases_rollback`; flags and positionals become named arguments. The translation validates arguments against the CLI parser, including required values and conflicts. That keeps the MCP interface tied to the command implementation.

Most operational calls launch the installed executable with the selected configuration, project working directory, `--ci`, and `--json`. Arguments are passed as process arguments without shell interpolation. Results include `ok`, `exit_code`, `stdout`, `stderr`, and a `truncated` flag; JSON stdout is parsed when possible. Documentation and local-log snapshots have dedicated handlers.

```d2
direction: right

agent: "Coding agent" {style.fill: "#9BC4B6"}
mcp: "Local tako mcp" {style.fill: "#E88783"}
docs: "Bundled docs\nand skills" {style.fill: "#FFF9F4"}
cli: "CLI operations\nSelected project" {style.fill: "#9BC4B6"}
runtime: "Local development\nor VPS deployment" {style.fill: "#FFF9F4"}

agent -> mcp: "stdio requests"
mcp -> docs: "read / search"
mcp -> cli: "validated arguments"
cli -> runtime: "inspect / operate"
```

Calls are finite. Command execution defaults to 120 seconds, with `timeout_seconds` configurable up to 1800 for longer builds or deployments. Each output stream is capped at 1 MiB, and truncation is reported. This gives the agent a result it can reason about instead of leaving it attached to an endless process.

## Follow a failure from logs to a verified deployment

Suppose a page works on your machine but fails in the preview environment. A useful task names the request, the environment, and the intended scope:

> Investigate why the preview checkout page fails. Inspect its status, release, and logs, fix the cause, test the change, deploy to preview, and verify the same page afterward.

The agent should first read the project's [`tako.toml`](/docs/tako-toml/) and establish which environment and servers it is investigating. The MCP configuration selects an app configuration; it doesn't remove the need to choose an environment for operations that accept one.

For production or preview failures, `tako_status`, `tako_releases_list`, and `tako_logs` provide deployment evidence. A finite production log request can look like:

```json
{
  "name": "tako_logs",
  "arguments": { "env": "production", "days": 1 }
}
```

For the preview task, use `"preview"` instead. The point is to distinguish build failures, readiness failures, routing problems, and application errors before editing code. The [troubleshooting guide](/docs/troubleshooting/) gives the agent a reference for interpreting that evidence.

Local investigation uses a different set of tools. `tako_doctor` checks local setup; `tako_dev_list` identifies running apps. `tako_dev` starts development in the background and returns a ready record while the dev daemon owns the app process. The agent can then exercise the returned Tako URL with its browser or HTTP tools, following the [development workflow](/docs/development/).

`tako_dev_logs` returns a finite snapshot, defaulting to 100 recent entries. Its `next_after` cursor becomes the next request's `after`, allowing the agent to read new evidence after reproducing the bug. Check `truncated` before assuming the history is complete. Continuous log tailing belongs in a terminal; `tako_dev_stop` stops the development session when appropriate.

Once the fix is tested, the agent can inspect a deployment dry run. For an existing preview environment, this call previews the deployment:

```json
{
  "name": "tako_deploy",
  "arguments": { "env": "preview", "dry_run": true, "yes": true }
}
```

The actual deployment uses the same tool without `dry_run`. It follows the normal [deployment flow](/docs/deployment/). Afterward, inspect status and logs and repeat the failing request. MCP supplies operational access; successful command execution alone doesn't prove the checkout page works.

One failure case deserves special care: a timeout or cancellation terminates local execution, but remote work already submitted may continue. Inspect status before retrying a deployment or another mutation. Otherwise, an agent can mistake “I stopped waiting” for “nothing happened.”

Tool annotations distinguish inspection from operations that may change state. Client approval policies still apply, and `yes` skips a CLI confirmation rather than establishing user authorization. `tako_run` executes local code with app secrets, even when selecting a production environment; it isn't a remote shell. Secret setters accept `stdin`, but secrets should stay out of reports, and log lines remain untrusted data.

Installation, upgrades, interactive initialization, and agent setup stay in the terminal. For the working session itself, the [Coding agents guide](/docs/agents/) and [MCP implementation](https://github.com/tako-sh/tako/tree/master/tako/src/commands/mcp) show the available surface. Run `tako agents`, then give your agent a concrete failure to follow. It can read the reference, inspect the running app, and verify what its change actually did.
