---
title: "Secure Code Execution for AI Agents"
date: "2026-04-16T10:00"
description: "AI agents that run code need two security layers: a V8 sandbox for untrusted execution, and secure secret injection so credentials stay out of the isolate."
image: 82add43ee4cf
---

AI agents that execute code are everywhere now — code interpreters, data analysis pipelines, automated fix-it bots, SQL generators. Running arbitrary code on a server sounds alarming, and it should: you need isolation. But there's a second problem most tutorials skip: the agent itself needs to be secure. Its API keys and database credentials are just as much at risk as the code it runs.

Those are two different problems, solved at two different layers.

## The execution layer: isolate the code

[secureexec](https://secureexec.dev/) is an npm package that runs Node.js code inside V8 isolates — the same isolation technology behind Cloudflare Workers and Chrome tabs. No Docker. No VMs. One `npm install`.

The core properties:

| Property             | What it means                                                                                |
| -------------------- | -------------------------------------------------------------------------------------------- |
| **Deny-by-default**  | Filesystem, network, child processes, and env vars are all blocked unless explicitly granted |
| **Resource limits**  | CPU time budgets and memory caps prevent runaway code                                        |
| **Fast cold starts** | ~17.9ms per isolate, ~3.4MB per instance (their numbers, from benchmarks on their site)      |
| **Real Node APIs**   | `fs`, `http`, `child_process` are bridged to host capabilities — not stubbed                 |

Wiring it up as a tool in the Vercel AI SDK:

```ts
import { tool } from "ai";
import { NodeRuntime, createNodeDriver, createNodeRuntimeDriverFactory } from "secure-exec";
import { z } from "zod";

const runtime = new NodeRuntime({
  systemDriver: createNodeDriver({
    /* grant specific permissions here */
  }),
  runtimeDriverFactory: createNodeRuntimeDriverFactory(),
  memoryLimit: 64,
  cpuTimeLimitMs: 5000,
});

const executeCode = tool({
  description: "Run JavaScript in a secure sandbox",
  parameters: z.object({ code: z.string() }),
  execute: async ({ code }) => runtime.run(code),
});
```

The agent generates code, secureexec runs it inside an isolate, and the result comes back. The host filesystem, network, and credentials are untouched — whatever the model produces can't reach them.

## The deployment layer: keep secrets out of the isolate

Here's the part secureexec can't help with on its own: what the _agent process_ has access to before the isolate ever starts.

A typical agent backend holds LLM provider keys, database URLs, webhook secrets. If those live in a `.env` file, an agent executing code on your server can simply `fs.readFileSync('.env')`. Environment variables aren't much better — they show up in `/proc/<pid>/environ` and propagate to child processes. One well-crafted prompt, one compromised tool call, and every secret the agent has is exposed.

Tako handles this at the deployment layer. Secrets are encrypted at rest with AES-256-GCM and never touch disk as plaintext on the server. When `tako-server` spawns your agent, it injects secrets through a file descriptor, not through env vars:

```d2
direction: right

server: tako-server {style.fill: "#E88783"; style.font-size: 14}
pipe: fd 3 {shape: circle; style.font-size: 14}
agent: Agent Process {shape: hexagon; style.font-size: 14}
isolate: V8 Isolate {style.fill: "#9BC4B6"; style.font-size: 14}

server -> pipe: "write secrets JSON"
pipe -> agent: "read at startup, close fd"
agent -> isolate: "run untrusted code (no secrets)"
```

The SDK reads fd 3 once at startup and closes it. By the time your code creates an isolate, the file descriptor is gone. The sandbox has nothing to steal. Your agent code imports a typed `secrets` bag from the generated `tako.gen.ts`:

```ts
import { secrets } from "../tako.gen";

// Available to your agent's own code:
const llm = new Anthropic({ apiKey: secrets.ANTHROPIC_KEY });

// What happens if sandboxed code tries to log it:
console.log(secrets.ANTHROPIC_KEY); // "[REDACTED]"
JSON.stringify(secrets); // "[REDACTED]"
```

The SDK wraps secrets in a Proxy that redacts on `toString()` and `toJSON()`, so even accidental logging doesn't leak values. And since secrets never land in `process.env`, they're not visible to code running inside the isolate at all. See the [secrets docs](/docs) for the full fd 3 flow.

## What the two layers cover

| Layer                  | Tool         | Threat blocked                                               |
| ---------------------- | ------------ | ------------------------------------------------------------ |
| **Code sandbox**       | secureexec   | Agent-generated code accessing host filesystem, network, env |
| **Secret injection**   | Tako fd 3    | Env var and file exfiltration by untrusted code              |
| **Encryption at rest** | Tako secrets | Plaintext on disk, accidental commits                        |
| **Redacting proxy**    | Tako SDK     | Accidental secret logging                                    |

Set your secrets with [`tako secrets set`](/docs/cli), deploy with `tako deploy`, and pass your Vercel AI SDK tool — or LangChain tool, or any other framework — the secureexec-backed executor. No managed sandbox service. No external vault. No Docker.

Check the [deployment guide](/docs/deployment) for how rolling updates work with agent processes, and the [development docs](/docs/development) for how secrets flow locally with `tako dev`. The [CLI reference](/docs/cli) has the full `tako secrets` command set.

Two layers, two packages, one server.
