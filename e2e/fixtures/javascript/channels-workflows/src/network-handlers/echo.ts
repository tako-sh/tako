import { call, defineNetworkHandler } from "tako.sh";

export default defineNetworkHandler(async (input: { message: string }) => {
  return { echoed: await call("uppercase", input) };
});
