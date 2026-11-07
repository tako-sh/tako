---
title: Service contracts
description: "Define typed HTTP services, validate requests and responses, and share generated client contracts without importing server code."
---

`tako.sh/services` defines HTTP contracts for JavaScript and TypeScript. A contract describes methods, paths, input schemas, and response statuses. The SDK derives client and handler types and validates data at runtime.

Clients use ordinary HTTP or HTTPS URLs. Contracts and generated packages work without Tako Cloud or a Tako runtime. The Go and Rust SDKs do not currently expose equivalent contract authoring or client generation APIs. A JavaScript client can call any HTTP server that implements its contract.

## Define a contract

Put a default-exported service in `<app_root>/services/`. This example uses `app_root = "src"`:

```ts
// src/services/catalog.ts
import { Type as t } from "@sinclair/typebox";
import { defineService } from "tako.sh/services";

export default defineService("catalog", {
  getItem: {
    method: "GET",
    path: "/items/{id}",
    params: t.Object({ id: t.String({ minLength: 1 }) }),
    responses: {
      200: t.Object({ id: t.String(), name: t.String() }),
      404: t.Object({ error: t.String() }),
    },
  },
});
```

The service name identifies the generated contract. Operation names become client methods. Keep declarations separate from database access and other server code. Generation imports declaration modules, so their top-level code runs during generation.

## Implement the handler

`createServiceHandler` returns a standard `(Request) => Promise<Response>` function. A fetch-handler app can export it directly. Framework apps can mount it in their router.

```ts
// src/index.ts
import { createServiceHandler } from "tako.sh/services";
import catalog from "./services/catalog";

const items = new Map([["book", { id: "book", name: "Notebook" }]]);

export default createServiceHandler(catalog, {
  getItem: ({ params }) => {
    const item = items.get(params.id);
    if (!item) return { status: 404, body: { error: "Item not found" } };
    return { status: 200, body: item };
  },
});
```

The map is example data. Replace it with your application's storage. Each handler also receives the original `Request` as its second argument. Authentication, authorization, CORS, rate limits, and request-size limits belong to your application or HTTP host.

For a URL prefix, pass `{ basePath: "/api" }` as the handler's third argument and use a client `baseURL` ending in `/api`. Defining a service does not mount routes automatically.

## Generate and call a client

With the project's `tako.sh` package installed, run:

```bash
tako generate
```

For JavaScript projects, this reads `<app_root>/services/` and writes `<app_root>/tako.services.ts`. The generated module exports `services`, `ServiceContracts`, and `createClient`. It contains schemas and client construction code, with no imports from the producer application's implementation.

```ts
// src/catalog-client.ts
import { createClient } from "./tako.services";

const catalog = createClient("catalog", { baseURL: "https://api.example.com" });
const result = await catalog.getItem({ params: { id: "book" } });

if (result.status === 200) {
  console.log(result.body.name);
} else {
  console.log(result.body.error);
}
```

A declared `404` is a typed result, not an exception. Each result contains `status`, `body`, and `headers`. Call options accept `headers` and an abort `signal`. Client options accept `headers`, `credentials`, and a custom `fetch` function.

`baseURL` must be an absolute HTTP or HTTPS URL without a query, fragment, username, or password. Its path prefixes the operation path. Supply a reachable address and any required credentials. The contract package does not configure networking. A [native network](/networks/#use-private-service-addresses) separately provides private app names and service discovery to supported runtimes; workstation applications can use those names while `tako connect` is active.

For code that already imports a contract, `createServiceClient(catalog, { baseURL })` from `tako.sh/services` creates the same typed client without generation.

## Share contracts between projects

Export the generated `tako.services.ts` from a package that consumers can install. Compile it to JavaScript and TypeScript declarations with your package's normal build. Declare `tako.sh` and `@sinclair/typebox` as package dependencies, or as peer dependencies that consumers must install. Consumers import `createClient` from your package and supply their own `baseURL`.

The generated module contains no database credentials, handlers, or imports from the producer app. Schema annotations are included, so keep secrets out of declarations. Sharing the package requires no Tako Cloud account. Version and distribute it through your existing package workflow.

The SDK generator can also write a manifest or OpenAPI 3.1 document. With Bun and the SDK installed, run these commands from the project root for the `src` example:

```bash
bun run tako-sh-gen-service-types ./src/services --format manifest > services.json
bun run tako-sh-gen-service-types ./src/services --format openapi > openapi.json
```

The default format is `typescript`. `serviceManifest`, `serviceOpenApi`, and `serializeServiceDocument` are also exported from `tako.sh/services` for programmatic use.

## Generation and development

Discovery reads a flat directory of `.ts`, `.mts`, `.js`, or `.mjs` files. Each file must default-export a `defineService` result. Nested directories and duplicate service names fail generation. Files whose names start with `.` or `_`, and TypeScript declaration files, are skipped.

`tako generate`, `tako dev`, and JavaScript deployment preparation refresh the contracts. Development watches `services/` and regenerates before restarting the app. Generation errors stop initial development startup and deployment preparation. During a running dev session, failed regeneration reports the error and leaves the current app running. Fix the declaration and save it again.

Deployment generates the module before the build copies project sources. Ensure your build includes it when client code imports it. Removing every declaration, or the services directory, replaces an existing generated module with an empty contract set. Projects with neither service declarations nor a generated module skip service generation.

`tako.services.ts` always lives under the configured `app_root`. The ambient `tako.d.ts` has a separate [location rule](/cli/#project-setup). Edit source declarations and regenerate both files through the CLI.

## Validation and HTTP limits

Both the client and handler validate inputs and declared response bodies. The client validates input before calling `fetch`. The handler rejects invalid requests with a JSON `400` response and validates its result before serialization. Undeclared handler statuses and invalid handler results throw `ServiceError`; handle these through your host's error handling.

The client throws `ServiceError` for validation failures, undeclared statuses, incorrect content types, malformed JSON, or transport failures. Its `code` is `validation`, `unexpected-status`, `content-type`, `invalid-json`, or `transport`. `operation` and `phase` identify the failed call and request or response. `status` and `issues` provide details when available. Redirects are not followed automatically.

The HTTP contract has these constraints:

- Paths use whole-segment placeholders such as `/items/{id}`. Required string params must match those placeholders exactly. Empty strings and dot segments are rejected as param values.
- Query schemas contain scalars or arrays of scalars: strings, finite numbers, integers, or booleans. Arrays use repeated query keys and cannot be empty when sent. Unknown query fields are rejected.
- Bodies use JSON. `GET` and `HEAD` cannot declare a request body. Use `t.Optional(schema)` for an optional body.
- Response statuses range from 200 through 599. Use `null` for an empty response and return `body: undefined` from its handler. `HEAD`, `204`, `205`, and `304` require empty responses. `t.Null()` instead describes the JSON value `null`.

Supported TypeBox schemas are inline strings, finite numbers, integers, booleans, nulls, literals, arrays, objects, unions, and intersections. The supported constraints are:

| Schema            | Constraints                                                                                   |
| ----------------- | --------------------------------------------------------------------------------------------- |
| String            | `minLength`, `maxLength`, `pattern`                                                           |
| Number or integer | `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, positive `multipleOf`           |
| Array             | `minItems`, `maxItems`, `uniqueItems`                                                         |
| Object            | `additionalProperties`, `minProperties`, `maxProperties`; required fields follow `t.Optional` |

References, transforms, formats, and unsupported JSON Schema keywords fail at definition time. Annotations such as `default` do not populate missing values. For closed JSON objects, set `additionalProperties: false` explicitly. Contradictory overrides of object `required` metadata are rejected.

String lengths count Unicode code points. Patterns use JavaScript Unicode regular expressions. `multipleOf` compares normalized decimal spellings, so `0.3` is a multiple of `0.1`, but `0.1 + 0.2` is not. Validation cannot restore precision lost to JSON parsing or JavaScript arithmetic.

Validated objects have null prototypes. Use `Object.hasOwn` for membership checks. Outgoing bodies serialize a snapshot of own properties without calling `toJSON`; getters run once during copying. Cycles, non-finite numbers, and non-JSON body values fail validation, including additional properties. This is not a sandbox for untrusted getters.

Validation does not impose a general payload-size or depth limit. Snapshot memory grows with the payload. `uniqueItems` uses structural comparisons with quadratic worst-case cost in array length. Set `maxItems` where appropriate, and enforce host request limits. Regular-expression cost also remains the host's responsibility.

## Verify

Run `tako generate` and your project's TypeScript check. Call the handler with a valid request and confirm the declared status and body. Send invalid input and confirm `400`. Exercise each declared response status and confirm the client narrows its body by status. Typecheck a separate consumer of your generated package without access to the producer's source tree.
