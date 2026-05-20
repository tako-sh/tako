import { WaveformIcon } from "@phosphor-icons/react";
import { motion } from "motion/react";
import { cn } from "@/lib/utils";
import { Info } from "./info";
import { formatTimestamp, shortRequestId, type MissionLogEvent } from "./types";

type Props = {
  events: MissionLogEvent[];
  connected: boolean;
  className?: string;
};

export function MissionLog({ events, connected, className }: Props) {
  return (
    <aside
      className={cn(
        `
        relative flex min-h-80 w-full shrink-0 flex-col border-t
        border-border/50 bg-muted/30
        lg:h-full lg:min-h-0 lg:w-96 lg:border-t-0 lg:border-l lg:bg-card
      `,
        className,
      )}
    >
      <header className="flex items-center justify-between px-4 pt-5 pb-3">
        <h2
          className="
            flex items-center gap-2 font-mono text-[11px] font-bold
            tracking-widest text-muted-foreground uppercase
          "
        >
          <WaveformIcon className="size-3.5" aria-hidden="true" />
          Mission_Log
        </h2>
        <div className="flex items-center gap-2">
          <span
            className="
              font-mono text-[11px] tracking-widest text-muted-foreground
              uppercase
            "
          >
            {connected ? "live" : "offline"}
          </span>
          <span
            className={`
              inline-block size-1.5 rounded-full
              ${connected ? "data-pulse bg-primary" : `bg-muted-foreground`}
            `}
          />
        </div>
      </header>

      <div className="mx-4 mb-2 rounded-md bg-muted/40 p-3">
        <Info
          label="channels"
          description="Every workflow step writes to this feed. A single channel fans events out to all clients watching this base."
          sourcePath="src/channels/mission-log.ts"
        />
      </div>

      <div
        className="
          flex-1 space-y-2 overflow-y-auto p-4 font-mono text-[11px]
          leading-relaxed
        "
      >
        {events.length === 0 ? (
          <p className="mt-6 text-center text-muted-foreground/60 italic">
            — feed will appear here —
          </p>
        ) : (
          events.map((event) => <LogEntry key={event.id} event={event} />)
        )}
      </div>
    </aside>
  );
}

function LogEntry({ event }: { event: MissionLogEvent }) {
  const isError = event.level === "error";
  const isWarn = event.level === "warn";
  const isSystem = event.source === "System";

  const sourceTagClass = isError
    ? "text-destructive font-bold"
    : isWarn
      ? "text-[--color-tertiary] font-bold"
      : "text-primary";

  const wrapperClass = isError
    ? "flex gap-2 bg-destructive/10 px-2 py-1.5 -mx-2 rounded"
    : isWarn
      ? "flex gap-2 bg-[--color-tertiary]/10 px-2 py-1.5 -mx-2 rounded"
      : "flex gap-2";

  const reqId = event.requestId ? `REQ-${shortRequestId(event.requestId)}` : null;

  return (
    <motion.div
      className={wrapperClass}
      initial={{ opacity: 0, y: -6 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.2, ease: "easeOut" }}
    >
      <span
        className={`
          shrink-0
          ${isError ? "text-destructive" : `text-muted-foreground/60`}
        `}
      >
        [{formatTimestamp(event.timestamp)}]
      </span>
      <div className="flex-1 wrap-break-word">
        {isSystem && <span className={sourceTagClass}>[System] </span>}
        {reqId && <span className="text-muted-foreground/70">{reqId} </span>}
        {event.message}
      </div>
    </motion.div>
  );
}
