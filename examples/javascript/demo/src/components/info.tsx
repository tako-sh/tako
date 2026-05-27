import { ArrowSquareOutIcon } from "@phosphor-icons/react";

export const GITHUB_BASE = "https://github.com/tako-sh/tako/tree/master/examples/javascript/demo";

type Props = {
  label: string;
  description: string;
  sourcePath: string;
};

export function Info({ label, description, sourcePath }: Props) {
  const sourceUrl = `${GITHUB_BASE}/${sourcePath}`;
  const filename = sourcePath.split("/").pop() ?? sourcePath;

  return (
    <div className="flex flex-col gap-1.5">
      <div
        className="
          font-mono text-[11px] tracking-widest text-primary/80 uppercase
        "
      >
        {label}
      </div>
      <p className="max-w-xs text-xs/snug text-muted-foreground">{description}</p>
      <a
        href={sourceUrl}
        target="_blank"
        rel="noopener noreferrer"
        className="
          inline-flex items-center gap-1 font-mono text-[11px] tracking-wider
          text-primary/90 uppercase
          hover:text-primary
        "
      >
        {filename}
        <ArrowSquareOutIcon className="size-3" aria-hidden="true" />
      </a>
    </div>
  );
}
