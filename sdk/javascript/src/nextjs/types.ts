import type { ChildProcess } from "node:child_process";

/** Minimal Next.js config shape touched by {@link import("./index").withTako}. */
export type NextConfigShape = Record<string, unknown> & {
  /** Path to the Next adapter module. */
  adapterPath?: string;
  /** Extra dev origins accepted by Next dev server. */
  allowedDevOrigins?: string[];
  /** Next image config. */
  images?: Record<string, unknown>;
  /** Next output mode. */
  output?: string;
};

/** Context passed to a Next adapter `modifyConfig` hook. */
export interface NextAdapterContext {
  /** Current Next phase. */
  phase: string;
  /** Next.js version. */
  nextVersion: string;
}

/** Context passed to a Next adapter `onBuildComplete` hook. */
export interface NextBuildCompleteContext {
  /** Next routing manifest data. */
  routing: Record<string, unknown>;
  /** Next build output metadata. */
  outputs: Record<string, unknown>;
  /** Project root directory. */
  projectDir: string;
  /** Repository root directory. */
  repoRoot: string;
  /** Next dist directory. */
  distDir: string;
  /** Effective Next config. */
  config: NextConfigShape;
  /** Next.js version. */
  nextVersion: string;
  /** Next build id. */
  buildId: string;
}

/** Next adapter object exported to Next.js. */
export interface NextAdapterShape {
  /** Adapter name reported to Next.js. */
  name: string;
  /** Optional hook to mutate the Next config before build. */
  modifyConfig?: (
    config: NextConfigShape,
    ctx: NextAdapterContext,
  ) => Promise<NextConfigShape> | NextConfigShape;
  /** Optional hook run after the Next build completes. */
  onBuildComplete?: (ctx: NextBuildCompleteContext) => Promise<void> | void;
}

/** Options for {@link import("./fetch-handler").createNextjsFetchHandler}. */
export interface NextjsFetchHandlerOptions {
  /** @defaultValue "127.0.0.1" */
  hostname?: string;
  /** @defaultValue 30_000 */
  startupTimeoutMs?: number;
  /** @defaultValue [] */
  argv?: string[];
  /** @defaultValue directory of the serverPath argument */
  cwd?: string | URL;
  /** Test-only hooks that bypass process spawning and fetch. */
  unstable_testing?: {
    /** Test hook that returns the upstream port. */
    ensureServer?: () => Promise<number>;
    /** Test hook used instead of global `fetch`. */
    fetchImplementation?: typeof fetch;
  };
}

/** State for one managed Next.js child process. */
export interface ManagedNextjsServer {
  /** Spawned child process, or null before start/after shutdown. */
  child: ChildProcess | null;
  /** In-flight readiness promise. */
  ready: Promise<number> | null;
  /** Extra argv passed to `server.js`. */
  argv: string[];
  /** Working directory for the child process. */
  cwd: string;
  /** Hostname the child server binds to. */
  hostname: string;
  /** Startup timeout in ms. */
  startupTimeoutMs: number;
}

/** File layout produced by staging a Next.js build for Tako. */
export interface NextjsBuildManifest {
  /** Root `.next` directory. */
  distRoot: string;
  /** Generated Tako fetch entrypoint. */
  takoEntrypoint: string;
  /** Next standalone output directory. */
  standaloneDir: string;
  /** Next standalone `server.js` path. */
  standaloneServer: string;
  /** Next static assets directory. */
  staticDir: string;
  /** Project public assets directory. */
  publicDir: string;
  /** Static assets path inside standalone output. */
  standaloneStaticDir: string;
  /** Public assets path inside standalone output. */
  standalonePublicDir: string;
}
