import { ArrowClockwiseIcon, PackageIcon } from "@phosphor-icons/react";
import { AnimatePresence, motion } from "motion/react";
import { Badge } from "@/components/ui/badge";
import { Card, CardAction, CardContent, CardHeader } from "@/components/ui/card";
import { Empty, EmptyContent, EmptyDescription } from "@/components/ui/empty";
import { WorkflowPipeline } from "./workflow-pipeline";
import { PIPELINE_STEPS, shortRequestId, totalRetries, type InFlightRequest } from "./types";

type Props = {
  requests: InFlightRequest[];
};

export function InFlightFeed({ requests }: Props) {
  const activeCount = requests.filter((request) => !request.isComplete).length;

  return (
    <section>
      <header className="mb-4 flex items-center justify-between">
        <h2
          className="
            flex items-center gap-2 text-xs font-bold tracking-[0.18em]
            uppercase
          "
        >
          <PackageIcon className="size-4 text-primary" aria-hidden="true" />
          Shipments
        </h2>
        <span
          className="
            font-mono text-[11px] tracking-widest text-muted-foreground
            uppercase
          "
        >
          In Flight: {activeCount.toString().padStart(2, "0")}
        </span>
      </header>

      {requests.length === 0 ? (
        <Empty>
          <EmptyContent>
            <EmptyDescription>
              No requests yet. Dispatch one above — you&apos;ll see each workflow step run here with
              live progress.
            </EmptyDescription>
          </EmptyContent>
        </Empty>
      ) : (
        <div className="space-y-3">
          <AnimatePresence initial={false}>
            {requests.map((request) => (
              <motion.div
                key={request.requestId}
                initial={{ opacity: 0, y: -4 }}
                animate={{ opacity: 1, y: 0 }}
                exit={{ opacity: 0 }}
                transition={{ duration: 0.18, ease: "easeOut" }}
              >
                <RequestCard request={request} />
              </motion.div>
            ))}
          </AnimatePresence>
        </div>
      )}
    </section>
  );
}

function RequestCard({ request }: { request: InFlightRequest }) {
  const { isComplete } = request;
  const retries = totalRetries(request.retries);
  const isQueued = !isComplete && PIPELINE_STEPS.every((step) => request.steps[step] === "pending");

  const idColor = isComplete ? "text-muted-foreground" : "text-primary";

  const badge = isComplete
    ? { label: "Delivered", className: "bg-primary/10 text-primary border-primary/20" }
    : isQueued
      ? {
          label: "Queued",
          className:
            "bg-[--color-tertiary]/15 text-[--color-tertiary] border-[--color-tertiary]/30",
        }
      : { label: "In flight", className: "bg-primary/15 text-primary border-primary/30" };

  return (
    <Card size="sm" className={isComplete ? "opacity-80" : ""}>
      <CardHeader>
        <div className="min-w-0">
          <div className="mb-1 flex items-center gap-2">
            <span
              className={`
                font-mono text-xs font-bold
                ${idColor}
              `}
            >
              REQ-{shortRequestId(request.requestId)}
            </span>
            {!isComplete && (
              <span
                className="
                  inline-block size-1.5 data-pulse rounded-full bg-primary
                "
              />
            )}
          </div>
          <h3 className="truncate text-sm font-semibold">{request.item}</h3>
        </div>
        <CardAction className="flex flex-row items-center gap-2">
          <AnimatePresence initial={false}>
            {retries > 0 && (
              <motion.div
                key="retries"
                initial={{ opacity: 0, scale: 0.9 }}
                animate={{ opacity: 1, scale: 1 }}
                exit={{ opacity: 0, scale: 0.9 }}
                transition={{ duration: 0.15, ease: "easeOut" }}
              >
                <Badge
                  variant="outline"
                  className="
                    border-[--color-tertiary]/30 bg-[--color-tertiary]/15
                    font-mono text-[10px] tracking-widest
                    text-[--color-tertiary] uppercase
                  "
                >
                  <ArrowClockwiseIcon data-icon="inline-start" />
                  {retries === 1 ? "1 retry" : `${retries} retries`}
                </Badge>
              </motion.div>
            )}
          </AnimatePresence>
          <motion.div
            key={badge.label}
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            transition={{ duration: 0.18 }}
          >
            <Badge
              variant="outline"
              className={`
                font-mono text-[10px] tracking-widest uppercase
                ${badge.className}
              `}
            >
              {badge.label}
            </Badge>
          </motion.div>
        </CardAction>
      </CardHeader>

      <CardContent>
        <WorkflowPipeline request={request} />
      </CardContent>
    </Card>
  );
}
