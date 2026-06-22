import { ArrowClockwiseIcon, CheckIcon, XIcon } from "@phosphor-icons/react";
import { motion } from "motion/react";
import {
  PIPELINE_STEPS,
  PIPELINE_STEP_LABELS,
  type InFlightRequest,
  type PipelineStep,
  type PipelineStepState,
} from "./types";

const QUEUED_STAGE = "queued";
const VISUAL_PIPELINE_STEPS = [QUEUED_STAGE, ...PIPELINE_STEPS] as const;

type Props = {
  request: InFlightRequest;
};

export function WorkflowPipeline({ request }: Props) {
  const { isComplete, steps: stepStates, retries } = request;
  const visualStates = buildVisualStates({ isComplete, stepStates });

  return (
    <div className="relative pt-2">
      <div
        className="
          absolute inset-x-5.5 top-3.75 z-0 h-0.5 bg-surface-container-lowest
          sm:inset-x-8
        "
      />
      <TrackFill visualStates={visualStates} />
      <div className="relative z-10 flex justify-between">
        {VISUAL_PIPELINE_STEPS.map((step) => (
          <StepPill
            key={step}
            step={step}
            state={visualStates[step]}
            retries={step === QUEUED_STAGE ? 0 : retries[step]}
            isComplete={isComplete}
          />
        ))}
      </div>
    </div>
  );
}

type VisualPipelineStep = (typeof VISUAL_PIPELINE_STEPS)[number];

function buildVisualStates({
  isComplete,
  stepStates,
}: {
  isComplete: boolean;
  stepStates: Record<PipelineStep, PipelineStepState>;
}): Record<VisualPipelineStep, PipelineStepState> {
  const hasStarted = PIPELINE_STEPS.some((step) => stepStates[step] !== "pending");
  return Object.fromEntries(
    VISUAL_PIPELINE_STEPS.map((step) => [
      step,
      step === QUEUED_STAGE ? (hasStarted || isComplete ? "done" : "running") : stepStates[step],
    ]),
  ) as Record<VisualPipelineStep, PipelineStepState>;
}

function TrackFill({
  visualStates,
}: {
  visualStates: Record<VisualPipelineStep, PipelineStepState>;
}) {
  const lastActiveIndex = VISUAL_PIPELINE_STEPS.reduce(
    (acc, step, i) => (visualStates[step] !== "pending" ? i : acc),
    0,
  );
  const gaps = VISUAL_PIPELINE_STEPS.length - 1;
  const width = Math.max(0, (lastActiveIndex / gaps) * 100);

  return (
    <div
      className="
        absolute inset-x-[22px] top-[15px] z-0 h-0.5
        sm:inset-x-8
      "
    >
      <motion.div
        className="h-full origin-left bg-primary will-change-transform"
        initial={false}
        animate={{ scaleX: width / 100 }}
        transition={{ type: "spring", stiffness: 180, damping: 26, mass: 0.6 }}
      />
    </div>
  );
}

function StepPill({
  step,
  state,
  retries,
  isComplete,
}: {
  step: VisualPipelineStep;
  state: PipelineStepState;
  retries: number;
  isComplete: boolean;
}) {
  const label = step === QUEUED_STAGE ? "Queued" : PIPELINE_STEP_LABELS[step];
  const dotClass = dotClassFor({ state, isComplete });
  const labelClass = labelClassFor({ state });

  return (
    <div className="flex flex-col items-center gap-2">
      <div className={dotClass}>
        {state === "done" && (
          <CheckIcon
            size={10}
            weight="bold"
            className={
              isComplete
                ? "text-primary-container"
                : `
              text-on-primary-container
            `
            }
            aria-hidden="true"
          />
        )}
        {state === "running" && (
          <div
            className="
          size-1.5 data-pulse rounded-full bg-primary
        "
          />
        )}
        {state === "failed" && (
          <XIcon size={10} weight="bold" className="text-error" aria-hidden="true" />
        )}
      </div>
      <span
        className={`
          w-11 text-center font-mono text-[10px] leading-tight uppercase
          sm:w-16 sm:text-[11px]
          ${labelClass}
        `}
      >
        {label}
      </span>
      {retries > 0 && (
        <span
          className="
            inline-flex items-center gap-0.5 font-mono text-[9px]
            tracking-widest text-[--color-tertiary] uppercase
          "
        >
          <ArrowClockwiseIcon className="size-2.5" aria-hidden="true" />×{retries}
        </span>
      )}
    </div>
  );
}

function dotClassFor({
  state,
  isComplete,
}: {
  state: PipelineStepState;
  isComplete: boolean;
}): string {
  const base = "w-4 h-4 rounded-full border-2 border-surface flex items-center justify-center";
  if (state === "done") {
    const fill = isComplete ? "bg-on-primary-container" : "bg-primary-container";
    return `${base} ${fill}`;
  }
  if (state === "running") {
    return "w-4 h-4 rounded-full bg-surface border-2 border-primary flex items-center justify-center";
  }
  if (state === "failed") {
    return "w-4 h-4 rounded-full bg-surface border-2 border-error flex items-center justify-center";
  }
  return "w-4 h-4 rounded-full bg-surface-container-lowest border-2 border-outline-variant/30 flex items-center justify-center";
}

function labelClassFor({ state }: { state: PipelineStepState }): string {
  if (state === "running") {
    return "text-primary font-bold";
  }
  if (state === "failed") {
    return "text-error";
  }
  if (state === "pending") {
    return "text-outline";
  }
  return "text-on-surface-variant";
}
