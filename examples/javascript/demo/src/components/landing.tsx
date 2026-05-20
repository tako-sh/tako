import {
  ArrowRightIcon,
  ArrowSquareOutIcon,
  CodeIcon,
  StackIcon,
  NetworkIcon,
  RadioIcon,
} from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import type { PlanetBase } from "@/lib/bases";
import { baseHref, type ParsedHost } from "@/lib/host";
import { GITHUB_BASE } from "./info";
import { TopAppBar } from "./top-app-bar";

const TAKO_URL = "https://tako.sh";

type Props = {
  rootHost: string;
  rootOrigin: string;
  bases: PlanetBase[];
};

export function Landing({ rootHost, rootOrigin, bases }: Props) {
  const [baseName, setBaseName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const parsedHost: ParsedHost = { rootHost, rootOrigin };

  function handleSubmit(event: React.SyntheticEvent<HTMLFormElement>) {
    event.preventDefault();
    const slug = normalizeBaseName(baseName);
    if (!slug) {
      setError("Enter a base name using letters, numbers, or hyphens");
      return;
    }
    redirectToBase(parsedHost, slug);
  }

  return (
    <div className="flex min-h-screen flex-col antialiased">
      <TopAppBar baseName={null} />
      <main
        className="
          relative flex flex-1 flex-col items-center px-6 py-16
          md:py-24
        "
      >
        <BackgroundGrid />

        <div
          className="
            relative z-10 w-full max-w-5xl
          "
        >
          <div className="min-w-0">
            <div
              className="
                mb-6 inline-flex items-center gap-2 font-mono text-[11px]
                tracking-[0.2em] text-primary/90 uppercase
              "
            >
              <span className="inline-block size-1 rounded-full bg-primary" />
              Mission Control · Command Deck
            </div>
            <h1
              className="
                mb-4 text-4xl leading-[1.05] font-bold tracking-tight
                md:text-5xl
              "
            >
              Planetary <span className="text-primary">Supply Desk</span>
            </h1>
            <p className="mb-10 max-w-xl text-base/relaxed text-muted-foreground">
              Dispatch supplies to an off-world base. Each base gets its own wildcard mission route:
              pick one, submit a supply request, and watch a durable workflow run with a live
              mission log on the side.
            </p>

            <form onSubmit={handleSubmit} className="mb-10 max-w-3xl">
              <FieldGroup>
                <Field data-invalid={error ? true : undefined}>
                  <FieldLabel htmlFor="base-name">Enter a base name</FieldLabel>
                  <div
                    className="
                      flex max-w-2xl flex-col gap-3
                      sm:flex-row
                    "
                  >
                    <div
                      className="
                        flex min-w-0 flex-1 items-center rounded-md border
                        border-input bg-card/35 transition-colors
                        has-focus-visible:border-ring has-focus-visible:ring-3
                        has-focus-visible:ring-ring/50
                      "
                    >
                      <span
                        className="
                          hidden h-11 shrink-0 items-center border-r border-border/60
                          px-3 font-mono text-xs text-muted-foreground
                          sm:flex
                        "
                      >
                        https://
                      </span>
                      <span
                        className="
                          flex h-11 shrink-0 items-center border-r border-border/60
                          px-3 font-mono text-sm text-muted-foreground
                          sm:hidden
                        "
                      >
                        https://
                      </span>
                      <Input
                        id="base-name"
                        autoFocus
                        placeholder="valles-hub"
                        aria-invalid={error ? true : undefined}
                        value={baseName}
                        onChange={(event) => {
                          setBaseName(event.target.value);
                          setError(null);
                        }}
                        className="
                          h-11 min-w-0 flex-1 border-0 bg-transparent px-3 font-mono
                          text-base shadow-none
                          placeholder:text-muted-foreground/40
                          focus-visible:border-transparent focus-visible:ring-0
                        "
                      />
                      <span
                        className="
                          flex h-11 shrink-0 items-center border-l border-border/60
                          px-3 font-mono text-xs text-muted-foreground
                        "
                      >
                        .{rootHost}
                      </span>
                    </div>
                    <Button
                      type="submit"
                      size="lg"
                      className="h-11 px-6 font-mono tracking-wider uppercase sm:shrink-0"
                    >
                      Enter base
                      <ArrowRightIcon data-icon="inline-end" />
                    </Button>
                  </div>
                  {error ? (
                    <FieldDescription
                      className="
                        font-mono text-xs tracking-wider text-destructive
                        uppercase
                      "
                    >
                      {error}
                    </FieldDescription>
                  ) : null}
                </Field>
              </FieldGroup>
            </form>

            <div
              className="
                mb-12
              "
            >
              <div className="mb-4">
                <h2 className="font-heading text-2xl font-bold tracking-tight">
                  One request, four platform primitives
                </h2>
                <p className="mt-1 max-w-2xl text-sm/relaxed text-muted-foreground">
                  The route is wildcard DNS. The app behavior is ordinary code running on Tako.
                </p>
              </div>
              <ol
                className="
                  grid grid-cols-1 gap-3
                  lg:grid-cols-4
                "
              >
                <Feature
                  step="01"
                  icon={<NetworkIcon className="size-4" aria-hidden="true" />}
                  label="Durable workflows"
                  body="One supply request fans out into five resumable steps. If the server restarts mid-launch, the workflow resumes where it stopped."
                  sourcePath="src/workflows/order-shipment.ts"
                />
                <Feature
                  step="02"
                  icon={<RadioIcon className="size-4" aria-hidden="true" />}
                  label="Live channels"
                  body="Each workflow step publishes to a channel, so connected clients see progress without polling."
                  sourcePath="src/channels/mission-log.ts"
                />
                <Feature
                  step="03"
                  icon={<CodeIcon className="size-4" aria-hidden="true" />}
                  label="Image service"
                  body="Server code signs AVIF image URLs. Tako resizes, caches, and serves them from /_tako/image."
                  sourcePath="src/routes/index.tsx"
                />
                <Feature
                  step="04"
                  icon={<StackIcon className="size-4" aria-hidden="true" />}
                  label="Scheduled cleanup"
                  body="A daily workflow prunes old demo records, keeping the public app tidy without a separate cron service."
                  sourcePath="src/workflows/cleanup.ts"
                />
              </ol>
            </div>

            <section className="mb-16">
              <h2
                className="
                  mb-3 font-mono text-[11px] tracking-widest text-muted-foreground
                  uppercase
                "
              >
                Planet bases
              </h2>
              <div
                className="
                  grid grid-cols-1 gap-3
                  sm:grid-cols-2 xl:grid-cols-3
                "
              >
                {bases.map((base, index) => (
                  <BaseButton
                    key={base.slug}
                    base={base}
                    loading={index === 0 ? "eager" : "lazy"}
                    parsedHost={parsedHost}
                  />
                ))}
              </div>
            </section>

            <footer
              className="
                flex flex-wrap items-center gap-6 pt-8 font-mono text-[11px]
                tracking-widest text-muted-foreground uppercase
              "
            >
              <a
                href={TAKO_URL}
                target="_blank"
                rel="noopener noreferrer"
                className="
                  transition-colors
                  hover:text-primary
                "
              >
                tako.sh
              </a>
              <a
                href={`${TAKO_URL}/docs`}
                target="_blank"
                rel="noopener noreferrer"
                className="
                  transition-colors
                  hover:text-primary
                "
              >
                docs
              </a>
              <a
                href={GITHUB_BASE}
                target="_blank"
                rel="noopener noreferrer"
                className="
                  inline-flex items-center gap-1.5 transition-colors
                  hover:text-primary
                "
              >
                <CodeIcon className="size-3" aria-hidden="true" />
                source
              </a>
              <span className="ml-auto text-muted-foreground/60">built with tako.sh</span>
            </footer>
          </div>
        </div>
      </main>
    </div>
  );
}

function BaseButton({
  base,
  loading,
  parsedHost,
}: {
  base: PlanetBase;
  loading: "eager" | "lazy";
  parsedHost: ParsedHost;
}) {
  return (
    <button
      type="button"
      onClick={() => redirectToBase(parsedHost, base.slug)}
      className="
        group relative aspect-video overflow-hidden border border-border
        bg-muted text-left transition-colors outline-none
        hover:border-primary/60 focus-visible:border-primary
        focus-visible:ring-3 focus-visible:ring-ring/50
      "
    >
      <img
        src={base.image.card}
        alt=""
        loading={loading}
        className="
          absolute inset-0 size-full object-cover opacity-80 transition duration-300
          group-hover:scale-[1.03] group-hover:opacity-100
        "
      />
      <span className="absolute inset-0 bg-linear-to-t from-background via-background/20 to-transparent" />
      <span className="absolute inset-x-0 bottom-0 block p-3">
        <span
          className="
            block font-mono text-[10px] tracking-[0.18em]
            text-primary uppercase
          "
        >
          {base.world}
        </span>
        <span className="block truncate font-heading text-base font-bold">{base.name}</span>
      </span>
    </button>
  );
}

function Feature({
  step,
  icon,
  label,
  body,
  sourcePath,
}: {
  step: string;
  icon: ReactNode;
  label: string;
  body: string;
  sourcePath: string;
}) {
  const sourceUrl = `${GITHUB_BASE}/${sourcePath}`;
  const filename = sourcePath.split("/").pop() ?? sourcePath;

  return (
    <li
      className="
        border border-border bg-card/35 p-4
        transition-colors hover:border-primary/50
      "
    >
      <div className="mb-3 flex items-start justify-between gap-3">
        <div
          className="
            flex items-center gap-2 font-mono text-[11px]
            tracking-widest text-primary uppercase
          "
        >
          <span
            className="
              inline-flex size-6 items-center justify-center rounded-sm
              bg-primary/10
            "
          >
            {icon}
          </span>
          {label}
        </div>
        <span className="font-mono text-[10px] tracking-widest text-muted-foreground/55">
          {step}
        </span>
      </div>
      <p className="text-xs/relaxed text-muted-foreground">{body}</p>
      <a
        href={sourceUrl}
        target="_blank"
        rel="noopener noreferrer"
        className="
          mt-3 inline-flex items-center gap-1 font-mono text-[10px]
          tracking-widest text-primary/90 uppercase
          hover:text-primary
        "
      >
        {filename}
        <ArrowSquareOutIcon className="size-3" aria-hidden="true" />
      </a>
    </li>
  );
}

function BackgroundGrid() {
  return (
    <div
      aria-hidden="true"
      className="pointer-events-none absolute inset-0 opacity-[0.04]"
      style={{
        backgroundImage:
          "radial-gradient(circle at 50% 50%, var(--color-primary) 1px, transparent 1px)",
        backgroundSize: "24px 24px",
      }}
    />
  );
}

function normalizeBaseName(raw: string): string | null {
  const cleaned = raw
    .trim()
    .toLowerCase()
    .replace(/\s+/g, "-")
    .replace(/[^a-z0-9-]/g, "")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "");
  if (!cleaned || cleaned.length > 64) return null;
  return cleaned;
}

function redirectToBase(parsedHost: ParsedHost, slug: string): void {
  if (typeof window === "undefined") return;
  window.location.href = baseHref(parsedHost, slug);
}
