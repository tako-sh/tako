import { expect, test } from "bun:test";
import { IncomingMessage } from "node:http";
import { Socket } from "node:net";
import { incomingMessageToRequest } from "../../src/tako/node-http";

test("an early response can cancel the body while the remaining upload drains", async () => {
  const req = new IncomingMessage(new Socket());
  req.method = "POST";
  const request = incomingMessageToRequest(req);
  await request.body!.cancel();

  expect(() => {
    req.emit("data", Buffer.from("remaining upload"));
    req.emit("end");
  }).not.toThrow();
  expect(req.destroyed).toBe(false);
});
