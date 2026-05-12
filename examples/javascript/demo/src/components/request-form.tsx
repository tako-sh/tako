import { DropIcon, FirstAidKitIcon, PlantIcon, WrenchIcon } from "@phosphor-icons/react";
import { memo } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Info } from "./info";
import { formatBaseName } from "./types";

type Props = {
  tenantSlug: string;
  submitting: boolean;
  onSubmit: (payload: { item: string }) => void;
};

const ITEMS: { label: string; icon: React.ReactNode }[] = [
  { label: "O2 Canisters", icon: <DropIcon data-icon="inline-start" /> },
  { label: "Rover Parts", icon: <WrenchIcon data-icon="inline-start" /> },
  { label: "Medical Pack", icon: <FirstAidKitIcon data-icon="inline-start" /> },
  { label: "Hydroponics", icon: <PlantIcon data-icon="inline-start" /> },
];

export const RequestForm = memo(function RequestForm({ tenantSlug, submitting, onSubmit }: Props) {
  return (
    <Card>
      <CardHeader
        className="
          flex flex-col gap-4
          md:flex-row md:items-start md:justify-between
        "
      >
        <div>
          <CardTitle className="text-xl font-bold tracking-tight uppercase">
            Request Supplies
          </CardTitle>
          <CardDescription
            className="
            mt-1 font-mono text-xs tracking-wider uppercase
          "
          >
            Sector: {formatBaseName(tenantSlug)}
          </CardDescription>
        </div>
        <div className="md:max-w-xs">
          <Info
            label="durable workflows"
            description="Each dispatch enqueues a workflow that fans out into 5 resumable steps. Launch and landing can fail — Tako retries them until the delivery lands."
            sourcePath="src/workflows/order-shipment.ts"
          />
        </div>
      </CardHeader>

      <CardContent>
        <div
          className="
            grid grid-cols-2 gap-3
            sm:grid-cols-4
          "
        >
          {ITEMS.map(({ label, icon }) => (
            <Button
              key={label}
              type="button"
              size="lg"
              variant="outline"
              disabled={submitting}
              onClick={() => onSubmit({ item: label })}
              className="
                h-12 min-w-0 justify-center font-mono text-[11px]
                tracking-wider uppercase
                sm:text-xs
              "
            >
              {icon}
              {label}
            </Button>
          ))}
        </div>
      </CardContent>
    </Card>
  );
});
