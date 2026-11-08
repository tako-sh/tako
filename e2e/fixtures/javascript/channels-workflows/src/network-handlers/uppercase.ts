import { defineNetworkHandler } from "tako.sh";

export default defineNetworkHandler((input: { message: string }) => input.message.toUpperCase());
