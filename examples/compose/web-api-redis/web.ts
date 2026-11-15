const api = JSON.stringify(process.env.API_URL ?? "http://localhost:8081").replaceAll(
  "<",
  "\\u003c",
);

Bun.serve({
  hostname: "0.0.0.0",
  port: 3000,
  fetch(request) {
    if (new URL(request.url).pathname === "/health") return new Response("ok");
    return new Response(
      `<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Compose application group</title>
<style>body{font:18px system-ui;max-width:42rem;margin:4rem auto;padding:1rem}button{font:inherit;padding:.6rem 1rem}</style>
<h1>One group, two apps</h1>
<p>This frontend calls a separate API. Redis keeps the count across deployments.</p>
<button id="visit">Call the API</button>
<p id="result" role="status">Ready.</p>
<script>
const result = document.querySelector('#result');
document.querySelector('#visit').addEventListener('click', async () => {
  try {
    const response = await fetch(${api} + '/visits');
    if (!response.ok) throw new Error('API returned ' + response.status);
    const { visits } = await response.json();
    result.textContent = 'API visits: ' + visits;
  } catch (error) { result.textContent = error.message; }
});
</script>
</html>`,
      { headers: { "content-type": "text/html; charset=utf-8" } },
    );
  },
});
