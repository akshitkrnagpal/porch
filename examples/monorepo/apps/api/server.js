// Porch sets PORT and HOST. Fall back to fixed values so `node server.js` still works alone.
const http = require("node:http");

const port = Number(process.env.PORT || 4001);
const host = process.env.HOST || "127.0.0.1";

http
  .createServer((req, res) => {
    res.setHeader("content-type", "application/json");
    res.setHeader("access-control-allow-origin", "*");
    res.end(JSON.stringify({ service: "api", url: process.env.PORCH_URL || null, path: req.url }));
  })
  .listen(port, host, () => console.log(`api listening on ${host}:${port}`));
