import { strict as assert } from "node:assert";
import { Channel, configureChannels } from "tako.sh/client";

// Run from the installed fixture so this exercises the packed browser SDK and
// deployed proxy together. Only Bun's test socket overrides the routing host.
const [server, route] = process.argv.slice(2);
const baseUrl = `http://${server}:8080`;
const routing = { Host: route!, "X-Forwarded-Proto": "https", Forwarded: "proto=https" };
const sockets: WebSocket[] = [];
class RoutedSocket extends WebSocket {
  constructor(url: string | URL) {
    super(url.toString(), { headers: routing });
    sockets.push(this);
  }
}
configureChannels({ websocket: RoutedSocket });
const options = { baseUrl, authorization: "e2e" };
const events: Array<{ data: { message: string } }> = [];
const layout = new Channel("shared", "ws").connect({
  ...options,
});
(layout.raw as EventTarget).addEventListener("message", (event) => {
  events.push(JSON.parse((event as MessageEvent<string>).data));
});
const memberships: Array<{ connectionId: string; subject: string }> = [];
function presence() {
  const channel = new Channel("tako.presence:lobby", "ws").connect(options);
  (channel.raw as EventTarget).addEventListener("message", (event) => {
    const message = JSON.parse((event as MessageEvent<string>).data);
    if (message.type === "tako.presence.ready")
      channel.send({ type: "tako.presence", data: { page: "chat" } });
    if (message.type === "tako.presence")
      memberships.splice(0, memberships.length, ...message.members);
  });
  return channel;
}
const observer = presence();
const page = presence();
async function until(check: () => boolean) {
  const deadline = Date.now() + 15_000;
  while (!check()) {
    if (Date.now() >= deadline) throw new Error("Shared channel fixture timed out");
    await Bun.sleep(20);
  }
}
try {
  await until(() => memberships.length === 2);
  assert.equal(sockets.length, 1, "WebSocket channels and presence must share a socket");
  assert.ok(memberships.every((member) => member.subject === "e2e-user"));
  page.close();
  await until(() => memberships.length === 1);
  assert.equal(
    sockets[0]?.readyState,
    WebSocket.OPEN,
    "leaving a page must keep the layout connected",
  );
  const message = `shared-${crypto.randomUUID()}`;
  layout.send({ type: "message", data: { message } });
  await until(() => events.some((event) => event.data.message === message));
  assert.equal(sockets.length, 1);
  console.log(`Shared WebSocket and page-presence checks passed on ${server}`);
} finally {
  page.close();
  observer.close();
  layout.close();
}
await until(() => sockets[0]?.readyState === WebSocket.CLOSED);
