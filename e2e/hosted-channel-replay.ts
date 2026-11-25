import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { Channel } from "tako.sh/client";

// Run in the staged channels-workflows project after both deployments are live.
const [cli, route, home, peer] = process.argv.slice(2);
assert.ok(cli && route && home && peer && home !== peer);
const headers = { Host: route, "X-Forwarded-Proto": "https", Forwarded: "proto=https" };
const prefix = crypto.randomUUID();
type Event = { id: string; channel: string; type: string; data: { message: string } };

async function publish(server: string, i: number) {
  const message = `${prefix}-${i}`;
  const response = await fetch(`http://${server}:8080/publish`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ message }),
    signal: AbortSignal.timeout(15_000),
  });
  assert.equal(response.status, 200, `POST /publish on ${server}`);
  const { ok, id, publisher, databaseBindings, hasReplayBinding } = await response.json();
  assert.equal(ok, true);
  assert.ok(typeof id === "string" && id.length);
  assert.ok(typeof publisher === "string" && publisher.length);
  assert.deepEqual(databaseBindings, ["messages"], `app-visible SQL bindings on ${server}`);
  assert.equal(hasReplayBinding, false, `app received @channels on ${server}`);
  return { publisher, event: { id, type: "message", channel: "demo", data: { message } } };
}

async function resume(server: string, after: string, expected: Event[]) {
  // All expected messages were acknowledged before reconnect. A reset followed
  // by replay-from-start, or waiting for the next live publish, must fail.
  const { promise, resolve, reject } = Promise.withResolvers<void>();
  const timer = setTimeout(() => reject(new Error(`Timed out resuming on ${server}`)), 15_000);
  const received: Event[] = [];
  const sub = new Channel("demo").subscribe({
    baseUrl: `http://${server}:8080`,
    headers,
    authorization: "e2e",
    lastEventId: after,
    onRecovery: (reason) => reject(new Error(`Unexpected recovery on ${server}: ${reason}`)),
    onError: reject,
  });
  try {
    (sub.raw as EventTarget).addEventListener("message", (event) => {
      const { id, channel, type, data } = JSON.parse((event as MessageEvent<string>).data);
      received.push({ id, channel, type, data });
      if (received.length === expected.length) resolve();
    });
    await promise;
    assert.deepEqual(received, expected, `replayed events on ${server}`);
    return received.at(-1)!.id;
  } finally {
    clearTimeout(timer);
    sub.close();
  }
}

function db(...args: string[]) {
  return JSON.parse(execFileSync(cli!, ["--ci", "--json", "db", ...args], { encoding: "utf8" }));
}

const first = await publish(home, 1);
const second = await publish(peer, 2);
assert.notEqual(first.publisher, second.publisher, "both apps must commit from different hosts");
const cursor = await resume(peer, first.event.id, [second.event]);
const concurrent = await Promise.all([publish(home, 3), publish(peer, 4)]);
const position = (event: Event) => Number(event.id.split(".").at(-1));
// Acknowledgements can arrive in a different order than the database commits.
const tail = concurrent.map(({ event }) => event).sort((a, b) => position(a) - position(b));
assert.ok(position(tail[0]!) < position(tail[1]!), "writers must get distinct positions");

const database = "channel-replay";
const { rows } = db(
  "shell",
  database,
  `SELECT id, json_extract(data_json, '$.message') FROM tako_replay_messages
   WHERE app = 'cw-e2e/production' AND channel = 'demo' AND type = 'message'
     AND json_extract(data_json, '$.message') LIKE '${prefix}-%' ORDER BY id`,
);
// The SQLite cursor's final component is its stored sequence. Match each
// acknowledgement to exactly one row before reconnecting either reader.
assert.deepEqual(
  rows,
  [first.event, second.event, ...tail].map((event) => [position(event), event.data.message]),
);
await resume(home, cursor, tail);
await resume(peer, cursor, tail);

const scope = "cw-e2e/production#channels";
const [record] = db("status", database).databases;
assert.equal(record.name, database);
assert.ok([home, peer].includes(record.server), "status must resolve a selected target");
assert.deepEqual(record.status.bindings, [scope]);
assert.deepEqual(record.status.remote_grants, [scope]);
console.log(`Hosted channel replay verified through ${home}, ${peer}, and db shell ${database}`);
