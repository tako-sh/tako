import { tako } from "tako.sh";

export default async function fetch(request: Request) {
  const url = new URL(request.url);
  if (url.pathname === "/storage/urls") {
    const storage = tako.storages.uploads;
    if (!storage) return new Response("Missing storage", { status: 500 });
    return Response.json({
      upload: await storage.createUploadUrl("e2e/file.txt"),
      download: await storage.createDownloadUrl("e2e/file.txt"),
    });
  }
  if (url.pathname === "/") {
    return new Response("<!doctype html><html><body><h1>Tako app</h1></body></html>", {
      headers: {
        "content-type": "text/html; charset=utf-8",
      },
    });
  }

  return new Response("Not Found", { status: 404 });
}
