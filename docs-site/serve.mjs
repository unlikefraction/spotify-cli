// Tiny static server for previewing dist/ (node serve.mjs [port]).
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { join, extname, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const dist = join(dirname(fileURLToPath(import.meta.url)), "dist");
const types = { ".html": "text/html; charset=utf-8", ".css": "text/css", ".js": "text/javascript", ".svg": "image/svg+xml", ".json": "application/json", ".txt": "text/plain", ".sh": "text/plain", ".md": "text/markdown", ".xml": "application/xml" };
const port = Number(process.argv[2] || 4321);
createServer(async (req, res) => {
  let path = decodeURIComponent(new URL(req.url, "http://x").pathname);
  if (path.endsWith("/")) path += "index.html";
  try { const body = await readFile(join(dist, path)); res.writeHead(200, { "content-type": types[extname(path)] || "application/octet-stream" }); res.end(body); }
  catch { res.writeHead(404, { "content-type": "text/html" }); res.end(await readFile(join(dist, "404.html"))); }
}).listen(port, () => console.log(`http://localhost:${port}`));
