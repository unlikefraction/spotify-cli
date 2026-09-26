// Asserts every internal link and asset resolves in dist/, page summaries are whole sentences,
// search entries point at real anchors, the CSP lets site.js fetch the index, and install.sh is
// the full installer and parses as sh.
import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "dist");
const pages = [];
const walk = (d) => readdirSync(d).forEach((f) => { const p = join(d, f); statSync(p).isDirectory() ? walk(p) : p.endsWith(".html") && pages.push(p); });
walk(dist);
let bad = 0;
const fail = (message) => { console.error(message); bad++; };
// A summary is prose that ends a sentence (or is cut at a word with "…"), never a table row or heading.
const summaryOk = (s) => s.length >= 12 && s.length <= 200 && /[.!?…]$/.test(s) && !/^[|#`>-]/.test(s);
for (const page of pages) {
  const html = readFileSync(page, "utf8");
  for (const [, url] of html.matchAll(/(?:href|src)="(\/[^"#?]*)/g)) {
    const target = url.endsWith("/") ? join(dist, url, "index.html") : join(dist, url);
    if (!existsSync(target)) fail(`${page.replace(dist, "")}: broken ${url}`);
  }
  const description = /<meta name="description" content="([^"]*)"/.exec(html)?.[1];
  if (!description || !summaryOk(description.replace(/&quot;/g, '"').replace(/&amp;/g, "&"))) fail(`${page.replace(dist, "")}: bad description ${JSON.stringify(description)}`);
  const article = /<main class="article">([\s\S]*)<\/main>/.exec(html)?.[1] || "";
  if (article.split("<pre>").length !== article.split('<div class="codeblock">').length) fail(`${page.replace(dist, "")}: a code block without its header strip`);
}
for (const line of readFileSync(join(dist, "llms.txt"), "utf8").split("\n").filter((l) => l.startsWith("- ["))) {
  if (!summaryOk(line.replace(/^- \[[^\]]*\]\([^)]*\): /, ""))) fail(`llms.txt: bad summary in ${line}`);
}
const search = JSON.parse(readFileSync(join(dist, "search-index.json"), "utf8"));
const text = (html) => html.replace(/<[^>]+>/g, "").replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, "&");
for (const { url, title, section } of search) {
  const [path, anchor] = url.split("#");
  const file = join(dist, path, "index.html");
  if (!title || !existsSync(file)) { fail(`search-index.json: ${url} has no page`); continue; }
  if (!anchor) continue;
  // The anchor must be the heading the entry is named after, not merely some id on the page.
  const heading = new RegExp(`<h2 id="${anchor}">([\\s\\S]*?)</h2>`).exec(readFileSync(file, "utf8"))?.[1];
  if (heading === undefined) fail(`search-index.json: ${url} has no anchor`);
  else if (text(heading) !== section) fail(`search-index.json: ${url} is section ${JSON.stringify(section)} but links to ${JSON.stringify(text(heading))}`);
}
const csp = JSON.parse(readFileSync(join(here, "vercel.json"), "utf8")).headers.flatMap((h) => h.headers).find((h) => h.key === "Content-Security-Policy")?.value || "";
if (!/connect-src [^;]*'self'/.test(csp)) fail("vercel.json: connect-src must allow 'self' (site.js fetches /search-index.json)");
const install = readFileSync(join(dist, "install.sh"), "utf8");
if (!install.startsWith("#!/bin/sh") || !install.includes("install_silicon_spotify \"$@\"")) fail("install.sh is not the full installer");
try { execFileSync("sh", ["-n", join(dist, "install.sh")]); } catch { fail("install.sh does not parse (sh -n)"); }
if (bad) process.exit(1);
console.log(`checked ${pages.length} pages, ${search.length} search entries: ok`);
