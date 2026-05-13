import { ArrowClockwiseIcon, WarningDiamondIcon } from "@phosphor-icons/react";
import type { ErrorComponentProps } from "@tanstack/react-router";
import { useRouter } from "@tanstack/react-router";
import { Button } from "@/components/ui/button";
import { TopAppBar } from "./top-app-bar";

export function AppErrorPage({ reset }: ErrorComponentProps) {
  const router = useRouter();

  return (
    <div className="flex min-h-screen flex-col bg-background text-foreground antialiased">
      <TopAppBar baseName={null} />
      <main className="flex flex-1 items-center justify-center px-6 py-16">
        <section className="w-full max-w-xl">
          <div
            className="
              mb-4 inline-flex items-center gap-2 font-mono text-[11px]
              tracking-widest text-destructive uppercase
            "
          >
            <WarningDiamondIcon className="size-4" aria-hidden="true" />
            Mission interrupted
          </div>
          <h1
            className="
              mb-4 text-3xl/tight font-bold tracking-tight
              md:text-4xl
            "
          >
            This base could not load.
          </h1>
          <p className="mb-8 max-w-lg text-sm/relaxed text-muted-foreground">
            The request failed before the command deck could render. Try again in a moment.
          </p>
          <Button
            type="button"
            size="lg"
            onClick={() => {
              reset();
              void router.invalidate();
            }}
            className="font-mono tracking-wider uppercase"
          >
            Retry
            <ArrowClockwiseIcon data-icon="inline-end" />
          </Button>
        </section>
      </main>
    </div>
  );
}
