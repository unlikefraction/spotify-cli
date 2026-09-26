// Tiny static server for previewing dist/ (node serve.mjs [port] [dir]). It sends vercel.json's
// headers, so the CSP is exercised locally, except that connect-src is narrowed to 'self': a
// preview never posts telemetry to the production backend. It listens on 127.0.0.1 only and never
// serves a file outside the dist directory.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { readFileSync } from "node:fs";
import { join, extname, dirname, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const dist = resolve(process.argv[3] || join(here, "dist"));
const types = { ".html": "text/html; charset=utf-8", ".css": "text/css", ".js": "text/javascript", ".svg": "image/svg+xml", ".json": "application/json", ".txt": "text/plain", ".sh": "text/plain", ".md": "text/markdown", ".xml": "application/xml" };
// vercel.json sources are literal paths, optionally with a "(.*)" wildcard.
const pattern = (source) => new RegExp(`^${source.split("(.*)").map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("(.*)")}$`);
const rules = JSON.parse(readFileSync(join(here, "vercel.json"), "utf8")).headers.map(({ source, headers }) => ({ re: pattern(source), headers }));
const headersFor = (path) => {
  const out = {};
  for (const { re, headers } of rules) if (re.test(path)) for (const { key, value } of headers) out[key] = value;
  if (out["Content-Security-Policy"]) out["Content-Security-Policy"] = out["Content-Security-Policy"].replace(/connect-src [^;]*/, "connect-src 'self'");
  delete out["Strict-Transport-Security"];
  return out;
};
const port = Number(process.argv[2] || 4321);
createServer(async (req, res) => {
  let path;
  try { path = decodeURIComponent(new URL(req.url, "http://x").pathname); }
  catch { res.writeHead(400, { "Content-Type": "text/plain" }); res.end("bad request path\n"); return; }
  const headers = headersFor(path);
  if (path.endsWith("/")) path += "index.html";
  const file = join(dist, path);
  try {
    if (!file.startsWith(dist + sep)) throw new Error("outside dist"); // "/..%2f" must not escape
    const body = await readFile(file);
    res.writeHead(200, { "Content-Type": types[extname(path)] || "application/octet-stream", ...headers });
    res.end(body);
  } catch {
    const body = await readFile(join(dist, "404.html")).catch(() => "Not found\n");
    res.writeHead(404, { ...headers, "Content-Type": "text/html; charset=utf-8" });
    res.end(body);
  }
}).listen(port, "127.0.0.1", () => console.log(`http://127.0.0.1:${port}`));
