import {
  ArrowRightIcon,
  ArrowSquareOutIcon,
  CodeIcon,
  StackIcon,
  NetworkIcon,
  RadioIcon,
} from "@phosphor-icons/react";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import type { PlanetBase } from "@/lib/bases";
import { GITHUB_BASE } from "./info";
import { TopAppBar } from "./top-app-bar";

const TAKO_URL = "https://tako.sh";

type Props = {
  rootHost: string;
  bases: PlanetBase[];
};

export function Landing({ rootHost, bases }: Props) {
  const [baseName, setBaseName] = useState("");
  const [error, setError] = useState<string | null>(null);

  function handleSubmit(event: React.SyntheticEvent<HTMLFormElement>) {
    event.preventDefault();
    const slug = normalizeBaseName(baseName);
    if (!slug) {
      setError("Enter a base name using letters, numbers, or hyphens");
      return;
    }
    redirectToBase(slug, rootHost);
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
              Dispatch supplies to an off-world base. Each base is an isolated Tako tenant — spin
              one up by name, submit a supply request, and watch a five-step durable workflow run
              with a live mission log on the side.
            </p>

            <form onSubmit={handleSubmit} className="mb-10">
              <FieldGroup>
                <Field data-invalid={error ? true : undefined}>
                  <FieldLabel htmlFor="base-name">Enter a base name</FieldLabel>
                  <div
                    className="
                      flex flex-col gap-3
                      md:flex-row
                    "
                  >
                    <div
                      className="
                        flex flex-1 items-center gap-1 rounded-lg border
                        border-input bg-transparent px-3 transition-colors
                        has-focus-visible:border-ring has-focus-visible:ring-3
                        has-focus-visible:ring-ring/50
                      "
                    >
                      <span
                        className="
                          shrink-0 font-mono text-sm text-muted-foreground
                        "
                      >
                        //
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
                          h-11 flex-1 border-0 bg-transparent px-1 font-mono
                          text-base shadow-none
                          placeholder:text-muted-foreground/35 placeholder:italic
                          focus-visible:border-transparent focus-visible:ring-0
                        "
                      />
                      <span
                        className="
                          hidden shrink-0 font-mono text-xs text-muted-foreground
                          sm:inline
                        "
                      >
                        .{rootHost}
                      </span>
                    </div>
                    <Button
                      type="submit"
                      size="lg"
                      className="h-11 px-6 font-mono tracking-wider uppercase"
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
                mb-12 grid grid-cols-1 gap-4
                md:grid-cols-2
              "
            >
              <Feature
                icon={<StackIcon className="size-4" aria-hidden="true" />}
                label="Multi-tenancy"
                body="Every subdomain is an isolated tenant. Tako routes wildcard hosts to one app and exposes the tenant via Host."
                sourcePath="tako.toml"
              />
              <Feature
                icon={<NetworkIcon className="size-4" aria-hidden="true" />}
                label="Durable workflows"
                body="One supply request fans out into five resumable steps. Server crashes mid-launch? The workflow picks up where it left off."
                sourcePath="src/workflows/order-shipment.ts"
              />
              <Feature
                icon={<RadioIcon className="size-4" aria-hidden="true" />}
                label="Live channels"
                body="Workflow steps publish to a channel. Every connected client sees the stream — no polling, no reconnect logic in app code."
                sourcePath="src/channels/mission-log.ts"
              />
              <Feature
                icon={<StackIcon className="size-4" aria-hidden="true" />}
                label="Image service"
                body="Server code signs AVIF image URLs. Tako resizes, caches, and serves them from /_tako/image."
                sourcePath="src/routes/index.tsx"
              />
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
                    rootHost={rootHost}
                    loading={index === 0 ? "eager" : "lazy"}
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
  rootHost,
  loading,
}: {
  base: PlanetBase;
  rootHost: string;
  loading: "eager" | "lazy";
}) {
  return (
    <button
      type="button"
      onClick={() => redirectToBase(base.slug, rootHost)}
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
  icon,
  label,
  body,
  sourcePath,
}: {
  icon: React.ReactNode;
  label: string;
  body: string;
  sourcePath: string;
}) {
  const sourceUrl = `${GITHUB_BASE}/${sourcePath}`;
  const filename = sourcePath.split("/").pop() ?? sourcePath;

  return (
    <Card size="sm">
      <CardHeader>
        <CardTitle
          className="
            flex items-center gap-2 font-mono text-[11px] tracking-widest
            text-primary uppercase
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
        </CardTitle>
      </CardHeader>
      <CardContent>
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
      </CardContent>
    </Card>
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
    .replace(/[^a-z0-9-]/g, "");
  if (!cleaned || cleaned.length > 63) return null;
  if (cleaned.startsWith("-") || cleaned.endsWith("-")) return null;
  return cleaned;
}

function redirectToBase(slug: string, rootHost: string): void {
  if (typeof window === "undefined") return;
  const port = window.location.port ? `:${window.location.port}` : "";
  const protocol = window.location.protocol;
  window.location.href = `${protocol}//${slug}.${rootHost}${port}/`;
}
