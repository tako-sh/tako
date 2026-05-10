import { createRouter as createTanStackRouter } from "@tanstack/react-router";
import { AppErrorPage } from "./components/app-error";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createTanStackRouter({
    routeTree,
    defaultErrorComponent: AppErrorPage,
  });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof getRouter>;
  }
}
