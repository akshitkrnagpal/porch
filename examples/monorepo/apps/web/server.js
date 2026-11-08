// The web app finds the API by name. `porch get` prints a URL even before the
// API is running, so start order doesn't matter.
const http = require("node:http");
const { execFileSync } = require("node:child_process");

const port = Number(process.env.PORT || 4000);
const host = process.env.HOST || "127.0.0.1";

function apiUrl() {
  if (process.env.API_URL) return process.env.API_URL;
  try {
    return execFileSync("porch", ["get", "api.shop"], { encoding: "utf8" }).trim();
  } catch {
    return "http://127.0.0.1:4001";
  }
}
const api = apiUrl();

http
  .createServer((req, res) => {
    res.setHeader("content-type", "text/html; charset=utf-8");
    res.end(`<!doctype html>
<title>shop</title>
<h1>shop web</h1>
<p>This page: <code>${process.env.PORCH_URL || "(not under Porch)"}</code></p>
<p>API: <a href="${api}">${api}</a></p>
<pre id="out">loading…</pre>
<script>
  fetch(${JSON.stringify(api)}).then(r => r.json()).then(j => {
    document.getElementById("out").textContent = JSON.stringify(j, null, 2);
  });
</script>`);
  })
  .listen(port, host, () => console.log(`web listening on ${host}:${port}`));
