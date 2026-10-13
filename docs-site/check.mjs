// Verify the Next static export: routes, links, native code blocks, CSP hashes and public docs.
import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, relative } from "node:path";
import { execFileSync } from "node:child_process";
import { here, root, DIST_DIR as dist, origin, topics } from "./lib/docs-core.mjs";
import { META_CSP, policyFor } from "./scripts/csp.mjs";

const pages = [];
const walk = (dir) => readdirSync(dir).forEach((name) => { const path = join(dir, name); statSync(path).isDirectory() ? walk(path) : path.endsWith(".html") && pages.push(path); });
walk(dist);
let bad = 0;
const fail = (message) => { console.error(message); bad++; };
const read = (path) => readFileSync(path, "utf8");
const vercel = JSON.parse(read(join(here, "vercel.json")));
const entities = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: "\u00a0" };
const decode = (value) => value.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (entity, name) => name[0] === "#" ? String.fromCodePoint(name[1].toLowerCase() === "x" ? parseInt(name.slice(2), 16) : Number(name.slice(1))) : entities[name] ?? entity);
const text = (html) => decode(html.replace(/<!--[\s\S]*?-->|<[^>]+>/g, ""));
const attr = (tag, name) => decode(new RegExp(`\\s${name}="([^"]*)"`).exec(tag)?.[1] ?? "");
const summaryOk = (s) => s.length >= 12 && s.length <= 200 && /[.!?…]$/.test(s) && !/^[|#`>-]/.test(s);
const htmlByFile = new Map(pages.map((page) => [page, read(page)]));
const idsByFile = new Map(pages.map((page) => [page, new Set([...htmlByFile.get(page).matchAll(/\sid="([^"]+)"/g)].map((m) => decode(m[1])))]));
const headings = (html) => new Map([...html.matchAll(/<h2\b([^>]*)>([\s\S]*?)<\/h2>/g)].map((m) => [attr(m[1], "id"), text(m[2])]));

function fileFor(path) {
  const redirect = vercel.redirects?.find((rule) => rule.source === path);
  if (redirect) path = new URL(redirect.destination, origin).pathname;
  const clean = decodeURIComponent(path).replace(/\/+$/, "");
  return [join(dist, clean || "/", "index.html"), join(dist, clean), join(dist, `${clean}.html`)]
    .find((file) => existsSync(file) && statSync(file).isFile());
}

let links = 0;
function checkLink(page, href) {
  const pagePath = `/${relative(dist, page).replace(/index\.html$/, "")}`;
  const url = new URL(href, `${origin}${pagePath}`);
  if (url.origin !== origin || url.pathname.startsWith("/api/")) return;
  const target = fileFor(url.pathname);
  if (!target) fail(`${pagePath}: broken ${href}`);
  else if (url.hash && !idsByFile.get(target)?.has(decodeURIComponent(url.hash.slice(1)))) fail(`${pagePath}: missing anchor ${href}`);
  links++;
}

for (const [page, html] of htmlByFile) {
  const name = relative(dist, page);
  for (const [, href] of html.matchAll(/\s(?:href|src)="([^"]+)"/g)) checkLink(page, decode(href));
  const description = [...html.matchAll(/<meta\b[^>]*>/g)].find(([tag]) => attr(tag, "name") === "description")?.[0];
  if (!name.startsWith("_not-found") && !summaryOk(attr(description ?? "", "content"))) fail(`${name}: bad description`);
  if ((html.match(/<h1\b/g) ?? []).length !== 1) fail(`${name}: expected one h1`);
  const ids = [...html.matchAll(/\sid="([^"]+)"/g)].map((m) => decode(m[1]));
  if (ids.length !== new Set(ids).size) fail(`${name}: duplicate ids`);
  if (!/<link\b[^>]*href="\/_next\/static\/[^" ]+\.css"/.test(html)) fail(`${name}: missing Next stylesheet`);
  // Native Silicon UI gives every code sample a labelled section and a copy action above the pre.
  const blocks = [...html.matchAll(/<section\b[^>]*aria-label="[^"]* source code"[^>]*>([\s\S]*?)<\/section>/g)];
  const pres = (html.match(/<pre\b/g) ?? []).length;
  if (blocks.length !== pres || blocks.some(([, block]) => !/<header\b[\s\S]*Copy code[\s\S]*<\/header>[\s\S]*<pre\b/.test(block))) fail(`${name}: a code block without its native header and copy action`);
  const csp = META_CSP.exec(html);
  if (!csp || csp[1] !== policyFor(html)) fail(`${name}: CSP does not match inline scripts`);
  else if (html.indexOf(csp[0]) > html.indexOf("<script")) fail(`${name}: CSP must precede scripts`);
}

const llms = read(join(dist, "llms.txt"));
for (const line of llms.split("\n").filter((line) => line.startsWith("- ["))) {
  if (!summaryOk(line.replace(/^- \[[^\]]*\]\([^)]*\): /, ""))) fail(`llms.txt: bad summary in ${line}`);
}
const search = JSON.parse(read(join(dist, "search-index.json")));
for (const { url, title, section } of search) {
  const link = new URL(url, origin);
  const file = fileFor(link.pathname);
  if (!title || link.origin !== origin || !file || !htmlByFile.has(file)) { fail(`search-index.json: ${url} has no page`); continue; }
  if (!link.hash) continue;
  const heading = headings(htmlByFile.get(file)).get(decodeURIComponent(link.hash.slice(1)));
  if (heading === undefined) fail(`search-index.json: ${url} has no heading anchor`);
  else if (heading !== section) fail(`search-index.json: ${url} is section ${JSON.stringify(section)} but links to ${JSON.stringify(heading)}`);
}

const sitemap = read(join(dist, "sitemap.xml"));
for (const route of ["/", ...topics.map(([name]) => `/docs/${name}/`)]) {
  const file = fileFor(route);
  if (!file) fail(`missing page ${route}`);
  if (!sitemap.includes(`<loc>${origin}${route}</loc>`)) fail(`sitemap.xml: missing ${route}`);
  if (route === "/" || !file) continue;
  for (const [id, heading] of headings(htmlByFile.get(file))) {
    if (!search.some((entry) => entry.url === `${route}#${id}` && entry.section === heading)) fail(`search-index.json: missing ${route}#${id}`);
  }
}
for (const [name] of topics) {
  const markdown = join(dist, "markdown", `${name}.md`);
  if (!existsSync(markdown) || read(markdown) !== read(join(root, "docs", `${name}.md`))) fail(`markdown/${name}.md: missing or differs from source`);
  if (!llms.includes(`${origin}/markdown/${name}.md`)) fail(`llms.txt: missing ${name}`);
  if (!search.some((entry) => entry.topic === name && entry.url === `/docs/${name}/` && entry.text)) fail(`search-index.json: missing ${name} introduction`);
}
for (const name of readdirSync(join(root, "docs")).filter((name) => name.endsWith(".md"))) {
  if (!topics.some(([topic]) => `${topic}.md` === name)) fail(`docs/${name}: not published`);
}
if (!read(join(dist, "robots.txt")).includes(`Sitemap: ${origin}/sitemap.xml`)) fail("robots.txt: missing sitemap");
const notFound = htmlByFile.get(join(dist, "404.html"));
if (!notFound || !/<meta\b[^>]*name="robots"[^>]*content="noindex/.test(notFound)) fail("404.html: missing or not noindex");
const csp = vercel.headers.flatMap((rule) => rule.headers).find((header) => header.key === "Content-Security-Policy")?.value ?? "";
if (!/connect-src [^;]*'self'/.test(csp)) fail("vercel.json: connect-src must allow 'self' for docs search");
if (!/script-src [^;]*'self'/.test(csp) || !/frame-ancestors 'none'/.test(csp)) fail("vercel.json: missing script/frame restrictions");
const install = read(join(dist, "install.sh"));
if (install !== read(join(root, "scripts", "install.sh")) || !install.startsWith("#!/bin/sh\n") || install.trimEnd().split("\n").at(-1) !== 'install_silicon_spotify "$@"') fail("install.sh is not the full source installer");
try { execFileSync("sh", ["-n", join(dist, "install.sh")], { stdio: "pipe" }); } catch { fail("install.sh does not parse (sh -n)"); }
if (bad) process.exit(1);
console.log(`checked ${pages.length} pages, ${links} links, ${search.length} search entries, CSP hashes, Markdown, sitemap and installer: ok`);
