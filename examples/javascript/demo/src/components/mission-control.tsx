import {
  DropIcon,
  FirstAidKitIcon,
  PlantIcon,
  WarningIcon,
  WrenchIcon,
} from "@phosphor-icons/react";
import { memo } from "react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Card, CardContent } from "@/components/ui/card";
import { Info } from "./info";
import { InFlightFeed } from "./InFlightFeed";
import { MissionLog } from "./mission-log";
import { RequestForm } from "./request-form";
import { TopAppBar } from "./top-app-bar";
import { formatBaseName, type InFlightRequest, type MissionLogEvent } from "./types";

const RESOURCES: { label: string; icon: React.ReactNode }[] = [
  { label: "O2 Canisters", icon: <DropIcon className="size-4" aria-hidden="true" /> },
  { label: "Rover Parts", icon: <WrenchIcon className="size-4" aria-hidden="true" /> },
  { label: "Medical Pack", icon: <FirstAidKitIcon className="size-4" aria-hidden="true" /> },
  { label: "Hydroponics", icon: <PlantIcon className="size-4" aria-hidden="true" /> },
];

type Props = {
  tenantSlug: string;
  rootOrigin: string;
  inFlight: InFlightRequest[];
  events: MissionLogEvent[];
  submitting: boolean;
  connected: boolean;
  submitError: string | null;
  onSubmit: (payload: { item: string }) => void;
};

export function MissionControl({
  tenantSlug,
  rootOrigin,
  inFlight,
  events,
  submitting,
  connected,
  submitError,
  onSubmit,
}: Props) {
  const baseName = formatBaseName(tenantSlug);

  return (
    <div
      className="
        flex min-h-dvh flex-col antialiased
        lg:h-dvh lg:overflow-hidden
      "
    >
      <TopAppBar baseName={baseName} homeHref={`${rootOrigin}/`} />
      <div
        className="
          flex flex-1
          lg:overflow-hidden
        "
      >
        <main
          className="
            relative flex flex-1 flex-col
            lg:flex-row lg:overflow-hidden
          "
        >
          <div
            className="
              relative flex-1 space-y-6 p-5
              md:p-8 lg:overflow-y-auto
            "
          >
            <TenantHeader baseName={baseName} requests={inFlight} />
            {submitError ? (
              <Alert variant="destructive">
                <WarningIcon />
                <AlertDescription>{submitError}</AlertDescription>
              </Alert>
            ) : null}
            <RequestForm tenantSlug={tenantSlug} submitting={submitting} onSubmit={onSubmit} />
            <InFlightFeed requests={inFlight} />
          </div>
          <MissionLog events={events} connected={connected} />
        </main>
      </div>
    </div>
  );
}

const TenantHeader = memo(function TenantHeader({
  baseName,
  requests,
}: {
  baseName: string;
  requests: InFlightRequest[];
}) {
  const resources = RESOURCES.map(({ label, icon }) => ({
    label,
    icon,
    count: requests.filter((request) => request.isComplete && request.item === label).length,
  }));

  return (
    <Card>
      <CardContent className="flex flex-col gap-4">
        <div className="flex items-start justify-between gap-5">
          <div className="min-w-0">
            <h2
              className="
                mb-1 font-mono text-[11px] tracking-widest text-muted-foreground
                uppercase
              "
            >
              Base
            </h2>
            <h3 className="truncate text-2xl/tight font-bold">{baseName}</h3>
          </div>
          <div
            className="
              hidden max-w-xs shrink-0
              lg:block
            "
          >
            <Info
              label="multi-tenancy"
              description="Every subdomain is an isolated tenant of this app. Tako routes wildcard hosts to the same process and exposes the tenant via the Host header."
              sourcePath="tako.toml"
            />
          </div>
        </div>

        <div className="border-t border-border/50 pt-4">
          <h4
            className="
              mb-3 font-mono text-[11px] tracking-widest text-muted-foreground
              uppercase
            "
          >
            Inventory
          </h4>
          <div
            className="
              grid grid-cols-2 gap-3
              sm:grid-cols-4
            "
          >
            {resources.map(({ label, icon, count }) => (
              <Resource key={label} label={label} icon={icon} count={count} />
            ))}
          </div>
        </div>
      </CardContent>
    </Card>
  );
});

const Resource = memo(function Resource({
  icon,
  label,
  count,
}: {
  icon: React.ReactNode;
  label: string;
  count: number;
}) {
  return (
    <div className="flex items-center gap-2 rounded-md bg-muted/40 px-3 py-2">
      <span className={count > 0 ? "text-primary" : "text-muted-foreground"}>{icon}</span>
      <div className="flex min-w-0 flex-col leading-tight">
        <span
          className="
            truncate font-mono text-[10px] tracking-widest text-muted-foreground
            uppercase
          "
        >
          {label}
        </span>
        <span
          className={`
            font-mono text-sm font-bold
            ${count > 0 ? "" : `text-muted-foreground`}
          `}
        >
          {count.toString().padStart(2, "0")}
        </span>
      </div>
    </div>
  );
});
