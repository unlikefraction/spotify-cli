import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const here = dirname(fileURLToPath(import.meta.url));
const origin = "https://spotify.unlikefraction.com";
const write = (path, value) => { mkdirSync(dirname(path), { recursive: true }); writeFileSync(path, value); };
const source = "self.__next_f.push([1,'page']);";
const hash = createHash("sha256").update(source).digest("base64");
const page = (content, noindex = false) => `<!doctype html><html><head><meta charSet="utf-8"/><meta http-equiv="Content-Security-Policy" content="script-src 'self' 'sha256-${hash}'"/><meta content="Use Spotify&#x27;s playback controls." name="description"/><link href="/_next/static/app.css" rel="stylesheet"/>${noindex ? '<meta name="robots" content="noindex"/>' : ""}<script>${source}</script></head><body><main><h1>Spotify</h1>${content}</main></body></html>`;

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), "spotify-check-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const web = join(root, "docs-site");
  const dist = join(web, "dist");
  write(join(web, "lib/docs-core.mjs"), `import { dirname, join } from "node:path"; import { fileURLToPath } from "node:url"; export const here = dirname(dirname(fileURLToPath(import.meta.url))); export const root = dirname(here); export const DIST_DIR = join(here, "dist"); export const origin = ${JSON.stringify(origin)}; export const topics = [["usage", "Usage"]];`);
  mkdirSync(join(web, "scripts"), { recursive: true });
  copyFileSync(join(here, "check.mjs"), join(web, "check.mjs"));
  copyFileSync(join(here, "scripts/csp.mjs"), join(web, "scripts/csp.mjs"));
  write(join(web, "vercel.json"), JSON.stringify({ headers: [{ headers: [{ key: "Content-Security-Policy", value: "script-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'" }] }], redirects: [{ source: "/docs", destination: "/docs/usage/" }] }));
  write(join(dist, "_next/static/app.css"), "body{}");
  write(join(dist, "index.html"), page('<a href="/docs">Docs</a><a href="/docs/usage/#play">Playback</a>'));
  write(join(dist, "404.html"), page('<a href="/">Home</a>', true));
  write(join(dist, "docs/usage/index.html"), page('<a href="../../">Home</a><h2 class="heading" id="play"><a href="#play"><span>Play</span><!-- --> &amp; pause</a></h2><section class="module_block" aria-label="Terminal source code"><header><button aria-label="Copy code">Copy</button></header><div><pre id="_R_1_"><code>spotify play</code></pre></div></section>'));
  const markdown = "# Usage\n\nControl Spotify with the command line.\n\n## Play & pause\n\nRun spotify play.\n";
  write(join(root, "docs/usage.md"), markdown);
  write(join(dist, "markdown/usage.md"), markdown);
  write(join(dist, "llms.txt"), `- [Usage](${origin}/markdown/usage.md): Control Spotify with the command line.\n`);
  write(join(dist, "sitemap.xml"), `<urlset><url><loc>${origin}/</loc></url><url><loc>${origin}/docs/usage/</loc></url></urlset>`);
  write(join(dist, "robots.txt"), `Sitemap: ${origin}/sitemap.xml\n`);
  write(join(dist, "search-index.json"), JSON.stringify([{ topic: "usage", title: "Usage", section: null, url: "/docs/usage/", text: "Control Spotify with the command line." }, { topic: "usage", title: "Usage", section: "Play & pause", url: "/docs/usage/#play", text: "Run spotify play." }]));
  const install = '#!/bin/sh\ninstall_silicon_spotify() { :; }\ninstall_silicon_spotify "$@"\n';
  write(join(root, "scripts/install.sh"), install);
  write(join(dist, "install.sh"), install);
  return { root, dist, run: () => spawnSync(process.execPath, [join(web, "check.mjs")], { encoding: "utf8" }) };
}

test("accepts Next routes, reordered attributes, React comments/entities and native code blocks", (t) => {
  const { run } = fixture(t);
  const result = run();
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /checked 3 pages/);
});

for (const [name, mutate, expected] of [
  ["missing linked anchor", (dist) => write(join(dist, "index.html"), page('<a href="/docs/usage/#missing">Missing</a>')), /missing anchor/],
  ["incorrect search heading", (dist) => {
    const path = join(dist, "search-index.json");
    write(path, readFileSync(path, "utf8").replace('"section":"Play & pause"', '"section":"Different heading"'));
  }, /but links to/],
  ["tampered inline script", (dist) => {
    const path = join(dist, "index.html");
    write(path, readFileSync(path, "utf8").replace(source, "alert('unexpected');"));
  }, /CSP does not match/],
  ["missing native copy header", (dist) => {
    const path = join(dist, "docs/usage/index.html");
    write(path, readFileSync(path, "utf8").replace(/<header>.*?<\/header>/, ""));
  }, /without its native header/],
  ["missing route", (dist) => rmSync(join(dist, "docs/usage/index.html")), /missing page \/docs\/usage\//],
  ["changed Markdown", (dist) => write(join(dist, "markdown/usage.md"), "Outdated docs."), /differs from source/],
  ["truncated installer", (dist) => write(join(dist, "install.sh"), "#!/bin/sh\n"), /not the full source installer/],
]) {
  test(`rejects ${name}`, (t) => {
    const { dist, run } = fixture(t);
    mutate(dist);
    const result = run();
    assert.equal(result.status, 1, result.stderr);
    assert.match(result.stderr, expected);
  });
}
