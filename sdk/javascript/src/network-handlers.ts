/** A direct request/response function exposed only to apps on the private network. */
export type NetworkHandler<Input = unknown, Output = unknown> = (
  input: Input,
) => Output | Promise<Output>;

/** Default-export one handler from `<app_root>/network-handlers/<name>.ts`. */
export function defineNetworkHandler<Input, Output>(
  handler: NetworkHandler<Input, Output>,
): NetworkHandler<Input, Output> {
  return handler;
}
