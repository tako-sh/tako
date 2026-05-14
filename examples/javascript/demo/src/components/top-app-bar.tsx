import { BookOpenIcon, CodeIcon, ArrowSquareOutIcon } from "@phosphor-icons/react";
import { buttonVariants } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { GITHUB_BASE } from "./info";

type Props = {
  baseName: string | null;
  homeHref?: string;
};

const TAKO_URL = "https://tako.sh";
const DOCS_URL = `${TAKO_URL}/docs`;

const linkClass = cn(
  buttonVariants({ variant: "ghost", size: "sm" }),
  "font-mono text-[11px] tracking-widest uppercase",
);

export function TopAppBar({ baseName, homeHref = "/" }: Props) {
  return (
    <header
      className="
        z-50 flex h-14 w-full shrink-0 items-center justify-between bg-card px-4
        shadow-[inset_0_-1px_0_0_var(--border)]
        sm:px-6
      "
    >
      <div
        className="
          flex min-w-0 flex-1 items-center gap-3
          sm:gap-6
        "
      >
        <a
          href={homeHref}
          className="
            shrink-0 font-heading text-base font-bold tracking-tight
            text-primary transition-opacity
            hover:opacity-90
            sm:text-lg
          "
        >
          <span
            className="
              hidden
              sm:inline
            "
          >
            PLANETARY_SUPPLY_DESK
          </span>
          <span className="sm:hidden">SUPPLY</span>
        </a>
        {baseName ? (
          <div
            className="
              flex items-center gap-2 truncate font-mono text-[11px]
              tracking-widest text-muted-foreground uppercase
            "
          >
            <span className="text-muted-foreground/60">//</span>
            <span className="truncate text-foreground">{baseName}</span>
          </div>
        ) : null}
      </div>
      <div className="flex shrink-0 items-center gap-1">
        <a
          href={DOCS_URL}
          target="_blank"
          rel="noopener noreferrer"
          aria-label="Tako docs"
          className={linkClass}
        >
          <BookOpenIcon data-icon="inline-start" />
          <span
            className="
              hidden
              sm:inline
            "
          >
            Docs
          </span>
        </a>
        <a
          href={GITHUB_BASE}
          target="_blank"
          rel="noopener noreferrer"
          aria-label="Source on GitHub"
          className={linkClass}
        >
          <CodeIcon data-icon="inline-start" />
          <span
            className="
              hidden
              sm:inline
            "
          >
            Source
          </span>
        </a>
        <a
          href={TAKO_URL}
          target="_blank"
          rel="noopener noreferrer"
          className={cn(
            linkClass,
            `
              text-primary
              hover:text-primary
            `,
          )}
        >
          tako.sh
          <ArrowSquareOutIcon data-icon="inline-end" />
        </a>
      </div>
    </header>
  );
}

export { TAKO_URL };
