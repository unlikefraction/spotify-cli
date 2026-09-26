// Asserts every internal link and asset resolves in dist/, and install.sh is present and executable text.
import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const dist = join(dirname(fileURLToPath(import.meta.url)), "dist");
const pages = [];
const walk = (d) => readdirSync(d).forEach((f) => { const p = join(d, f); statSync(p).isDirectory() ? walk(p) : p.endsWith(".html") && pages.push(p); });
walk(dist);
let bad = 0;
for (const page of pages) {
  const html = readFileSync(page, "utf8");
  for (const [, url] of html.matchAll(/(?:href|src)="(\/[^"#?]*)/g)) {
    const target = url.endsWith("/") ? join(dist, url, "index.html") : join(dist, url);
    if (!existsSync(target)) { console.error(`${page.replace(dist, "")}: broken ${url}`); bad++; }
  }
}
const install = readFileSync(join(dist, "install.sh"), "utf8");
if (!install.startsWith("#!/bin/sh") || !install.includes("install_silicon_spotify \"$@\"")) { console.error("install.sh is not the full installer"); bad++; }
if (bad) process.exit(1);
console.log(`checked ${pages.length} pages: ok`);
