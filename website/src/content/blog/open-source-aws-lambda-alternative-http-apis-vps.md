---
title: "The Open Source AWS Lambda Alternative for HTTP APIs on a VPS"
date: "2026-05-06T06:41"
description: "Compare AWS Lambda HTTP APIs with Tako: fetch handlers, native processes, scale-to-zero, and owned VPS pricing."
image: 0fc512199ec4
---

[AWS Lambda](https://aws.amazon.com/lambda/) is the default answer when someone says "serverless function." It made tiny deployable handlers feel normal, scales without you planning capacity first, and plugs into the rest of AWS like it was born there.

For event glue, cron fan-out, S3 triggers, and bursty workloads inside AWS, Lambda is excellent. This post is about a narrower question: what if your workload is mostly an HTTP API, and what you really want is the Lambda-shaped developer experience without renting the runtime one invocation at a time?

That is the shape Tako is built for: a fetch handler, a VPS, a proxy, TLS, deploys, logs, secrets, and optional scale-to-zero in one small platform.

## Same small handler, different contract

Lambda gives you a handler. For HTTP traffic, you usually put [API Gateway](https://docs.aws.amazon.com/apigateway/latest/developerguide/http-api-develop.html) or a [Lambda function URL](https://docs.aws.amazon.com/lambda/latest/dg/urls-configuration.html) in front of it. Lambda receives the HTTP request, maps it into an event payload, runs your function, and maps the return value back into an HTTP response.

Tako starts from the web API directly:

```typescript
export default function fetch(request: Request): Response {
  return new Response("Hello from a VPS");
}
```

That is a complete Tako app. The interface is the same [fetch handler pattern](/blog/the-fetch-handler-pattern) used by modern runtimes and frameworks: `Request` in, `Response` out. On Bun, the SDK hosts it directly. On Node, the [`tako.sh` SDK](/docs) bridges Node's HTTP server to the same fetch shape. For Go, `tako.ListenAndServe()` wraps any `http.Handler`.

The difference is where the handler lives. Lambda runs inside AWS's managed execution environment. Tako runs as a normal OS process on your server, behind Tako's Pingora proxy.

| Concern       | AWS Lambda HTTP API                                       | Tako on a VPS                                                                                               |
| ------------- | --------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| HTTP entry    | Function URL or API Gateway                               | Route in [`tako.toml`](/docs/tako-toml)                                                                     |
| App shape     | Function handler receiving an event                       | Web-standard fetch handler or Go `http.Handler`                                                             |
| Runtime       | AWS managed runtime environment                           | Native Bun, Node, or Go process                                                                             |
| Billing unit  | Requests plus duration and configured memory              | The VPS bill you already chose                                                                              |
| State         | Invocation model; use external services for durable state | Normal process plus [`TAKO_DATA_DIR`](/blog/stateful-apps-sqlite-uploads-tako-data-dir) for app-owned files |
| Scale-to-zero | Built into the Lambda model                               | Opt in with [`tako scale 0`](/docs/cli)                                                                     |
| Deployment    | Zip/image plus AWS config/IaC                             | Build locally, upload artifact, rolling update via [`tako deploy`](/docs/deployment)                        |

This is not "Lambda is bad." Lambda is very good at being Lambda. The trade is different: managed event compute versus owned-process HTTP hosting.

## Where Lambda shines

Lambda is strongest when the function is a piece of an AWS-native event graph.

S3 object created? Invoke a function. EventBridge schedule fired? Invoke a function. SNS message arrived? Invoke a function. You do not want to think about a process, a port, a server, a service file, or a deploy target. You want a unit of compute that appears when an event arrives and disappears when the work is done.

That model also makes sense when traffic is highly spiky. If an API gets ten requests in the morning and ten thousand at lunch, Lambda's concurrency model is the whole point. You can add provisioned concurrency, reserved concurrency, API Gateway throttling, IAM auth, CloudWatch alarms, X-Ray tracing, and all the other pieces around it.

The cost model is part of that bargain. AWS's own [Lambda pricing page](https://aws.amazon.com/lambda/pricing/) describes the default model as request charges plus duration charges, where duration pricing depends on the memory configured for the function. For many small APIs, that bill can be tiny. For workloads already deep in AWS, keeping the compute next to DynamoDB, SQS, EventBridge, or S3 can be the cleanest architecture.

Tako is not trying to replace that whole universe. If your app is mostly AWS events, stay close to the events.

## Where HTTP APIs get awkward

HTTP APIs are different. They often start as a simple request/response server and slowly collect normal server-shaped needs:

| Need                   | What tends to happen on Lambda                                 | What happens on Tako                                                               |
| ---------------------- | -------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Web framework          | Adapt the framework into Lambda events                         | Run the framework's server output as an app                                        |
| Persistent connections | Reach for API Gateway WebSockets or another service            | Use built-in [durable channels](/blog/durable-channels-built-in) for WebSocket/SSE |
| Background jobs        | Add SQS, EventBridge, Step Functions, or another workflow tool | Use built-in [workflows](/blog/durable-workflows-are-here)                         |
| Local files            | Use `/tmp` for scratch, external storage for durable data      | Write durable app files under `TAKO_DATA_DIR`                                      |
| Long work              | Split around Lambda's invocation model and service quotas      | Run normal processes and move background work to workflows                         |
| Predictable spend      | Model requests, duration, memory, gateway, and add-ons         | Pick a server size and watch the box                                               |

The first few functions feel wonderfully small. The tenth route often starts to look like a web server that has been chopped into pieces.

Tako goes the other direction. Your API is one app. It can still be tiny:

```toml
name = "api"
runtime = "bun"

[envs.production]
route = "api.example.com"
servers = ["ams"]
idle_timeout = 300
```

Deploy it:

```bash
tako init
tako deploy
```

Tako builds locally, uploads a compressed artifact over SFTP, runs production install on the server, starts a new instance, waits for readiness, moves traffic, and drains the old one. The [deployment docs](/docs/deployment) cover the full flow, but the important part is the shape: your app remains an app.

## Scale-to-zero without becoming a function

One reason developers reach for Lambda is scale-to-zero. Paying nothing while idle feels right for side projects, staging APIs, admin tools, webhook receivers, and low-traffic internal services.

Tako has that too, but it is process-based. By default, new deploys keep one desired instance running so the first request after deploy is hot. If you want on-demand mode, set the desired instance count to zero:

```bash
tako scale 0 --env production
```

After the app is idle for its `idle_timeout`, Tako stops the process. The next request wakes it. While the cold start is in progress, the proxy queues waiters behind the first request, then releases them once the app is ready. The deeper version is in [Scale-to-Zero Without Containers](/blog/scale-to-zero-without-containers).

```d2
direction: right

client: Client {
  style.font-size: 13
}

proxy: "Tako proxy" {
  style.font-size: 13
}

zero: "0 running\ninstances" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
  style.font-size: 13
}

app: "Native app\nprocess" {
  style.fill: "#9BC4B6"
  style.font-size: 13
}

client -> proxy: "HTTP request"
proxy -> zero: "wake"
zero -> app: "spawn + fd 4 readiness"
proxy -> app: "route request"
```

This is not the same isolation model as Lambda. Tako does not create a fresh sandbox per invocation, and it does not pretend your server disappeared. It gives you the cost and RAM benefits of stopping idle apps while keeping the mental model of a normal service.

## The VPS version of serverless

The phrase "serverless" never meant servers disappeared. It meant the platform took responsibility for them.

Tako takes responsibility for a smaller, more inspectable platform: the one running on your VPS. It manages routing, HTTPS, deploys, process lifecycle, logs, secrets, static assets, scale-to-zero, durable channels, and workflows. It is [open source on GitHub](https://github.com/lilienblum/tako), and you still own the machine. You can SSH into it. You can run SQLite. You can use native packages. You can put Cloudflare in front if you want a global network edge, or keep it boring with one region and one box.

That makes Tako a good Lambda alternative when:

| Choose Lambda when...                              | Choose Tako when...                             |
| -------------------------------------------------- | ----------------------------------------------- |
| Your app is mostly AWS event handlers              | Your app is mostly an HTTP API                  |
| You want AWS to own the full execution environment | You want to own the server and process          |
| Per-invocation metering matches the workload       | A flat VPS bill is easier to reason about       |
| You need deep AWS service integrations             | You need full Bun, Node, or Go runtime behavior |
| You are composing many cloud services              | You want fewer moving parts on one box          |

The simplest summary is this: Lambda is managed event compute. Tako is owned HTTP infrastructure.

If your API wants to be a function, Lambda is a great place to run it. If your API wants to be a real server but you still want the small-handler DX, [Tako](/docs) gives you that shape on hardware you control.
