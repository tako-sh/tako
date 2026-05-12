/**
 * Filesystem discovery for the configured workflows directory.
 *
 * Each `<name>.(ts|tsx|js|mjs|mts)` file becomes a workflow named `<name>`.
 * The default export must be either:
 *   - A `WorkflowDefinition` produced by `defineWorkflow(name, opts)` —
 *     handler and opts are read directly from the object.
 *   - A plain function — registered with empty opts.
 *
 * Nested directories are not scanned in v1 — flat structure only.
 */

import { readdir, stat } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { join, parse } from "node:path";
import { isWorkflowDefinition, isWorkflowExport } from "./define";
import type { WorkflowRuntimeOpts } from "./types";
import type { WorkflowHandler } from "./worker";

const VALID_EXTS = new Set([".ts", ".tsx", ".js", ".mjs", ".mts"]);

export interface DiscoveredWorkflow {
  name: string;
  handler: WorkflowHandler;
  opts: WorkflowRuntimeOpts;
}

export interface WorkflowDiscoveryOptions {
  /**
   * Only return workflows assigned to this worker group. Workflows without
   * `opts.worker` belong to the default group. Omit to return all workflows.
   */
  worker?: string;
}

const DEFAULT_WORKER_GROUP = "default";

export async function discoverWorkflows(
  dir: string,
  opts: WorkflowDiscoveryOptions = {},
): Promise<DiscoveredWorkflow[]> {
  const exists = await dirExists(dir);
  if (!exists) return [];

  const entries = await readdir(dir);
  const found: DiscoveredWorkflow[] = [];
  for (const entry of entries) {
    const parsed = parse(entry);
    if (!VALID_EXTS.has(parsed.ext)) continue;
    if (parsed.name.startsWith(".") || parsed.name.startsWith("_")) continue;

    const url = pathToFileURL(join(dir, entry)).href;
    const mod = (await import(/* @vite-ignore */ url)) as Record<string, unknown>;
    const defaultExport = mod["default"];

    if (isWorkflowExport(defaultExport)) {
      const def = defaultExport.definition;
      if (def.name !== parsed.name) {
        throw new Error(
          `workflow file '${parsed.name}' exports defineWorkflow('${def.name}', ...); the name must match the file basename`,
        );
      }
      pushIfWorkerMatches(found, { name: def.name, handler: def.handler, opts: def.opts }, opts);
    } else if (isWorkflowDefinition(defaultExport)) {
      pushIfWorkerMatches(
        found,
        {
          name: defaultExport.name,
          handler: defaultExport.handler,
          opts: defaultExport.opts,
        },
        opts,
      );
    } else if (typeof defaultExport === "function") {
      pushIfWorkerMatches(
        found,
        {
          name: parsed.name,
          handler: defaultExport as WorkflowHandler,
          opts: {},
        },
        opts,
      );
    } else {
      throw new Error(
        `workflow '${parsed.name}' (${entry}) must default-export a defineWorkflow() result or a plain function`,
      );
    }
  }
  return found;
}

function pushIfWorkerMatches(
  found: DiscoveredWorkflow[],
  workflow: DiscoveredWorkflow,
  opts: WorkflowDiscoveryOptions,
): void {
  if (opts.worker === undefined || workflowWorkerGroup(workflow.opts) === opts.worker) {
    found.push(workflow);
  }
}

function workflowWorkerGroup(opts: WorkflowRuntimeOpts): string {
  return opts.worker ?? DEFAULT_WORKER_GROUP;
}

async function dirExists(dir: string): Promise<boolean> {
  try {
    const s = await stat(dir);
    return s.isDirectory();
  } catch {
    return false;
  }
}
