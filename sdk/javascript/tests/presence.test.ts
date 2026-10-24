import { expect, test } from "bun:test";
import { definePresence } from "../src/presence/define";
import { ChannelRegistry } from "../src/channels";

test("presence authorizes with the public space name and a verified subject", async () => {
  const space = definePresence("lobby", {
    paramsSchema: (t) => t.Object({ version: t.String() }),
    auth: {
      verify: ({ channel, header, params }) => {
        expect(channel).toBe("lobby");
        expect(params.version).toBe("1");
        return header?.value === "good" ? { subject: "user-1" } : false;
      },
    },
  });
  const registry = new ChannelRegistry();
  registry.register(space.definition.channel, space);
  const input = {
    channel: space.definition.channel,
    operation: "connect" as const,
    params: { version: "1" },
  };
  expect((await registry.authorize(input)).ok).toBe(false);
  const result = await registry.authorize({ ...input, header: { value: "good" } });
  expect(result.ok).toBe(true);
  expect(result.subject).toBe("user-1");
  expect(result.transport).toBe("ws");
});

test("public presence and ordinary channels have distinct wire names", () => {
  const space = definePresence("lobby");
  expect(space.name).toBe("lobby");
  expect(space.definition.channel).toBe("tako.presence:lobby");
  expect(space.definition.auth).toBe(false);
});

test("presence supports cookie-only auth", async () => {
  const space = definePresence("private", {
    auth: {
      headerName: false,
      cookieName: "session",
      verify: ({ cookie }) => (cookie === "valid" ? { subject: "cookie-user" } : false),
    },
  });
  const registry = new ChannelRegistry();
  registry.register(space.definition.channel, space);
  const result = await registry.authorize({
    channel: space.definition.channel,
    operation: "connect",
    params: {},
    cookie: "valid",
  });
  expect(result.subject).toBe("cookie-user");
  expect(space.definition.auth).toMatchObject({ headerName: false, cookieName: "session" });
});
